//! Background metadata and thumbnail extraction.
//!
//! The library does not decode video itself. The app implements
//! [`MediaProber`] (with FFmpeg); the [`MetadataWorker`] takes videos that
//! lack metadata or a picture from the database, asks the prober, refines
//! the detected format with container metadata and the frame size, and
//! writes `<id>.jpg` thumbnails to a cache folder for videos whose server
//! offers no picture. For partly watched videos it also makes the frame
//! where they were left off, `<id>_at<seconds>.jpg`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use fp_core::format::{ContainerHints, resolve};
use fp_core::{StereoLayout, VideoFormat};

use crate::error::{Error, Result};
use crate::image::RgbaImage;
use crate::library::{Library, StoredProbe};
use crate::record::{MediaId, MediaRecord};

/// What a prober learned about a video's container and stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbeInfo {
    /// Seconds; `None` for live streams or when unknown.
    pub duration: Option<f64>,
    /// Frame width in pixels as shown (whole frame, both eyes): turned
    /// upright and unsqueezed.
    pub width: u32,
    /// Frame height in pixels as shown.
    pub height: u32,
    /// Codec name (`"hevc"`, `"av1"`, `"h264"`...).
    pub video_codec: Option<String>,
    /// Spherical / stereo metadata from the container.
    pub hints: ContainerHints,
}

/// Which part of the frame a thumbnail shows, so stereo videos give a
/// single-eye picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThumbnailCrop {
    /// Left half (side-by-side stereo).
    LeftEye,
    /// Left half stretched horizontally by 2x (half side-by-side stereo).
    LeftEyeStretched,
    /// Top half (top/bottom stereo).
    TopEye,
    /// Top half stretched vertically by 2x (half top/bottom stereo).
    TopEyeStretched,
    /// The whole frame (mono).
    Full,
}

impl ThumbnailCrop {
    /// The crop for a format: SBS → left half, TB → top half, mono → full.
    pub fn for_format(format: &VideoFormat) -> ThumbnailCrop {
        match format.stereo {
            StereoLayout::SideBySide => ThumbnailCrop::LeftEye,
            StereoLayout::HalfSideBySide => ThumbnailCrop::LeftEyeStretched,
            StereoLayout::TopBottom => ThumbnailCrop::TopEye,
            StereoLayout::HalfTopBottom => ThumbnailCrop::TopEyeStretched,
            StereoLayout::Mono => ThumbnailCrop::Full,
        }
    }
}

/// Reads metadata and frames from media. Implemented by the app on top of
/// FFmpeg; the library only calls it from the worker thread. Any call may
/// return [`Error::Cancelled`] to stop the pass and leave the video pending
/// for the next one.
pub trait MediaProber: Send + Sync {
    /// Container and stream information for `location` (path or URL).
    fn probe(&self, location: &str) -> Result<ProbeInfo>;

    /// One frame near `at_seconds`, cropped as asked, scaled to fit within
    /// `max_width` × `max_height` keeping its aspect ratio. Larger results
    /// are scaled down by the worker. A video's thumbnail is asked for
    /// right after its [`probe`](Self::probe).
    fn thumbnail(
        &self,
        location: &str,
        at_seconds: f64,
        max_width: u32,
        max_height: u32,
        crop: ThumbnailCrop,
    ) -> Result<RgbaImage>;
}

/// Tuning for the metadata worker.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerOptions {
    /// Where the JPEGs go. Created when needed.
    pub cache_dir: PathBuf,
    /// Thumbnail bounding box.
    pub thumbnail_max_width: u32,
    /// Thumbnail bounding box.
    pub thumbnail_max_height: u32,
    /// Where to take the thumbnail, as a fraction of the duration (the
    /// first frame when the duration is unknown).
    pub thumbnail_position: f64,
    /// Bounding box of the frame where a video was left off: large enough
    /// for the banner across Home.
    pub resume_max_width: u32,
    /// Bounding box of the frame where a video was left off.
    pub resume_max_height: u32,
    /// Make thumbnails for videos whose server offers no picture; those
    /// made stay when off.
    pub thumbnails: bool,
    /// Make the frames where partly watched videos were left off; off,
    /// those made are deleted.
    pub resume_thumbnails: bool,
    /// JPEG quality, 1–100.
    pub jpeg_quality: u8,
    /// Rows fetched from the database at a time.
    pub batch_size: usize,
    /// Also look for new work this often without being woken; `None` waits
    /// for [`MetadataWorker::wake`].
    pub poll_interval: Option<Duration>,
}

impl WorkerOptions {
    /// Defaults with a specific cache folder.
    pub fn with_cache_dir(cache_dir: impl Into<PathBuf>) -> WorkerOptions {
        WorkerOptions {
            cache_dir: cache_dir.into(),
            thumbnail_max_width: 640,
            thumbnail_max_height: 640,
            thumbnail_position: 0.2,
            resume_max_width: 1280,
            resume_max_height: 1280,
            thumbnails: true,
            resume_thumbnails: true,
            jpeg_quality: 82,
            batch_size: 16,
            poll_interval: None,
        }
    }
}

impl Default for WorkerOptions {
    /// Cache in `$XDG_CACHE_HOME/frameplayerx/thumbnails`.
    fn default() -> Self {
        WorkerOptions::with_cache_dir(fp_core::dirs::cache_dir().join("thumbnails"))
    }
}

/// Progress reported by the worker.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkerEvent {
    /// A pass over pending videos began.
    Started {
        /// Videos waiting to be probed.
        pending: usize,
    },
    /// A video was probed. `warning` explains a missing thumbnail.
    Processed {
        id: MediaId,
        /// Videos handled in this pass, this one included.
        done: usize,
        /// `done` plus what is still waiting.
        total: usize,
        /// A thumbnail was written.
        thumbnail: bool,
        warning: Option<String>,
    },
    /// Probing failed; the error is stored on the row and the video is not
    /// retried until [`Library::retry_failed_probes`].
    Failed {
        id: MediaId,
        /// Videos handled in this pass, this one included.
        done: usize,
        /// `done` plus what is still waiting.
        total: usize,
        error: String,
    },
    /// The frame where a partly watched video was left off was made, or
    /// could not be (`error`; not tried again for that position).
    ResumeThumbnail { id: MediaId, error: Option<String> },
    /// Frames where videos were left off were deleted: the videos were
    /// watched since, or the frames were turned off.
    ResumeThumbnailsDeleted(usize),
    /// Nothing left to do for now.
    Idle {
        /// Videos probed successfully in this pass.
        processed: usize,
        /// Videos that failed in this pass.
        failed: usize,
    },
    /// A database or cache-folder error interrupted a pass.
    Error(String),
    /// The worker thread exited.
    Stopped,
}

/// Counts from one [`process_pending`] pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkerStats {
    /// Videos probed successfully.
    pub processed: usize,
    /// Videos whose probe failed.
    pub failed: usize,
}

/// Writes `data` to `path` through a temporary file, so readers never see a
/// half-written JPEG.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("jpg.tmp");
    std::fs::write(&tmp, data).map_err(|e| Error::io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))
}

fn thumbnail_path(dir: &Path, id: MediaId) -> PathBuf {
    dir.join(format!("{id}.jpg"))
}

/// Grabs a frame of `rec` and writes it to `path` as a JPEG.
fn write_frame(
    prober: &dyn MediaProber,
    options: &WorkerOptions,
    rec: &MediaRecord,
    at: f64,
    (width, height): (u32, u32),
    path: PathBuf,
) -> Result<PathBuf> {
    let (w, h) = (width.max(1), height.max(1));
    let crop = ThumbnailCrop::for_format(&rec.effective_format().format);
    let img = prober.thumbnail(&rec.location, at, w, h, crop)?;
    write_atomic(&path, &img.fit_within(w, h)?.to_jpeg(options.jpeg_quality)?)?;
    Ok(path)
}

/// Probes one video and stores the result. `Err` means the probe itself
/// failed; a thumbnail problem comes back as a warning. `None`: the video
/// changed while it was probed, and is still pending.
fn process_one(
    library: &Library,
    prober: &dyn MediaProber,
    options: &WorkerOptions,
    rec: &MediaRecord,
) -> Result<Option<(bool, Option<String>)>> {
    let info = prober.probe(&rec.location)?;
    // Detect from the file name only, so a URL's query string cannot add
    // format tokens.
    let resolved = resolve(None, info.hints, &rec.file_name(), info.width, info.height);
    // Evidence orders strongest first: keep a stronger earlier detection
    // (e.g. a format declared by a DeoVR feed) over a weaker new guess.
    let detected = if resolved.evidence <= rec.detected.evidence {
        resolved
    } else {
        rec.detected
    };
    let duration = info
        .duration
        .filter(|d| d.is_finite() && *d > 0.0)
        .or(rec.duration);

    // A server's picture is shown as it is (one made before goes); with
    // thumbnails off, the one made before stays.
    let mut thumbnail = None;
    let mut written = false;
    let mut thumb_error = None;
    if rec.thumbnail_url.is_none() && !options.thumbnails {
        thumbnail = rec.thumbnail.clone();
    } else if rec.thumbnail_url.is_none() {
        let probed = MediaRecord {
            detected,
            ..rec.clone()
        };
        let at = duration.map_or(0.0, |d| d * options.thumbnail_position.clamp(0.0, 1.0));
        let size = (options.thumbnail_max_width, options.thumbnail_max_height);
        let path = thumbnail_path(&options.cache_dir, rec.id);
        match write_frame(prober, options, &probed, at, size, path) {
            Ok(p) => {
                thumbnail = Some(p);
                written = true;
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => thumb_error = Some(format!("thumbnail: {e}")),
        }
    }

    let positive = |v: u32| (v > 0).then_some(v);
    let stored = library.store_probe(
        rec,
        &StoredProbe {
            duration,
            width: positive(info.width),
            height: positive(info.height),
            video_codec: info.video_codec.clone(),
            detected,
            thumbnail: thumbnail.clone(),
            error: thumb_error.clone(),
        },
    )?;
    if !stored {
        return Ok(None);
    }
    Ok(Some((written, thumb_error)))
}

/// Deletes the files in the cache folder no video uses: thumbnails of
/// videos since removed from the library, and leftovers of interrupted
/// writes.
fn sweep(library: &Library, options: &WorkerOptions) -> Result<()> {
    let Ok(dir) = std::fs::read_dir(&options.cache_dir) else {
        return Ok(());
    };
    let used = library.picture_files()?;
    for e in dir.flatten() {
        let path = e.path();
        let ours = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".jpg") || n.ends_with(".jpg.tmp"));
        if ours && !used.contains(&path) {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Deletes the frames of videos no longer partly watched (all of them when
/// turned off) and makes the missing ones. True when `stop` was set or the
/// prober cancelled.
fn resume_thumbnails(
    library: &Library,
    prober: &dyn MediaProber,
    options: &WorkerOptions,
    stop: &AtomicBool,
    on_event: &mut dyn FnMut(WorkerEvent),
) -> Result<bool> {
    let stale = library.take_resume_thumbnails(!options.resume_thumbnails)?;
    if !stale.is_empty() {
        for path in &stale {
            // Already gone is as good.
            let _ = std::fs::remove_file(path);
        }
        on_event(WorkerEvent::ResumeThumbnailsDeleted(stale.len()));
    }
    if !options.resume_thumbnails {
        return Ok(false);
    }
    loop {
        let batch = library.pending_resume_thumbnails(options.batch_size.max(1))?;
        if batch.is_empty() {
            return Ok(false);
        }
        std::fs::create_dir_all(&options.cache_dir)
            .map_err(|e| Error::io(&options.cache_dir, e))?;
        for rec in &batch {
            if stop.load(Ordering::Relaxed) {
                return Ok(true);
            }
            let at = rec.resume_position;
            // A new name for each position, so the app shows the new frame
            // rather than the one it decoded before.
            let path = options
                .cache_dir
                .join(format!("{}_at{}.jpg", rec.id, at as u64));
            let size = (options.resume_max_width, options.resume_max_height);
            let (made, error) = match write_frame(prober, options, rec, at, size, path) {
                Ok(p) => (Some(p), None),
                Err(Error::Cancelled) => return Ok(true),
                Err(e) => (None, Some(e.to_string())),
            };
            library.store_resume_thumbnail(rec, made.as_deref())?;
            if let Some(old) = &rec.resume_thumbnail
                && made.as_ref() != Some(old)
            {
                let _ = std::fs::remove_file(old);
            }
            on_event(WorkerEvent::ResumeThumbnail { id: rec.id, error });
        }
    }
}

/// Processes every pending video once, on the calling thread, until none is
/// left, `stop` becomes true or the prober cancels: first the files no
/// video uses go, then the frames where partly watched videos were left off
/// are made (few, and what Home shows first), then the probes.
/// [`MetadataWorker`] runs this in the background; call it directly for
/// synchronous use (tools, tests).
pub fn process_pending(
    library: &Library,
    prober: &dyn MediaProber,
    options: &WorkerOptions,
    stop: &AtomicBool,
    on_event: &mut dyn FnMut(WorkerEvent),
) -> Result<WorkerStats> {
    let mut stats = WorkerStats::default();
    let mut total = library.pending_metadata_count(options.thumbnails)?;
    on_event(WorkerEvent::Started { pending: total });
    sweep(library, options)?;
    let cancelled = resume_thumbnails(library, prober, options, stop, on_event)?;
    if total > 0 {
        std::fs::create_dir_all(&options.cache_dir)
            .map_err(|e| Error::io(&options.cache_dir, e))?;
    }
    let mut done = 0;
    'outer: while !cancelled && !stop.load(Ordering::Relaxed) {
        let batch = library.pending_metadata(options.batch_size.max(1), options.thumbnails)?;
        if batch.is_empty() {
            break;
        }
        for rec in &batch {
            if stop.load(Ordering::Relaxed) {
                break 'outer;
            }
            let r = match process_one(library, prober, options, rec) {
                // Left pending for a later pass.
                Err(Error::Cancelled) => break 'outer,
                // Probed again with the next batch.
                Ok(None) => continue,
                Ok(Some(r)) => Ok(r),
                Err(e) => Err(e),
            };
            done += 1;
            total = total.max(done);
            report(library, rec, done, total, r, &mut stats, on_event)?;
        }
        total = total.max(done + library.pending_metadata_count(options.thumbnails)?);
    }
    on_event(WorkerEvent::Idle {
        processed: stats.processed,
        failed: stats.failed,
    });
    Ok(stats)
}

/// Stores a failure and sends the event for one processed video.
fn report(
    library: &Library,
    rec: &MediaRecord,
    done: usize,
    total: usize,
    result: Result<(bool, Option<String>)>,
    stats: &mut WorkerStats,
    on_event: &mut dyn FnMut(WorkerEvent),
) -> Result<()> {
    match result {
        Ok((thumbnail, warning)) => {
            stats.processed += 1;
            on_event(WorkerEvent::Processed {
                id: rec.id,
                done,
                total,
                thumbnail,
                warning,
            });
        }
        Err(e) => {
            let error = e.to_string();
            if !library.store_probe_error(rec, &error)? {
                return Ok(());
            }
            stats.failed += 1;
            on_event(WorkerEvent::Failed {
                id: rec.id,
                done,
                total,
                error,
            });
        }
    }
    Ok(())
}

/// A background thread that keeps metadata and thumbnails up to date.
///
/// It makes a pass when spawned, then sleeps until [`wake`](Self::wake)
/// (call it after a scan, an upsert or playback), [`set_options`](Self::set_options)
/// or the poll interval. Dropping it (or calling [`stop`](Self::stop)) stops
/// it after the current video and joins the thread.
pub struct MetadataWorker {
    control: Option<mpsc::Sender<Control>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

enum Control {
    Wake,
    Options(Box<WorkerOptions>),
}

impl std::fmt::Debug for MetadataWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetadataWorker")
            .field("running", &self.is_running())
            .finish()
    }
}

impl MetadataWorker {
    /// Starts the worker. Events arrive on the returned receiver; dropping
    /// the receiver is fine (events are then discarded).
    pub fn spawn(
        library: Library,
        prober: Arc<dyn MediaProber>,
        options: WorkerOptions,
    ) -> Result<(MetadataWorker, mpsc::Receiver<WorkerEvent>)> {
        let (event_tx, event_rx) = mpsc::channel();
        let (control_tx, control_rx) = mpsc::channel::<Control>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("fp-metadata".into())
            .spawn(move || {
                let send = |e: WorkerEvent| {
                    // A dropped receiver only means nobody listens.
                    let _ = event_tx.send(e);
                };
                let mut options = options;
                loop {
                    let mut forward = |e: WorkerEvent| send(e);
                    if let Err(e) =
                        process_pending(&library, &*prober, &options, &thread_stop, &mut forward)
                    {
                        send(WorkerEvent::Error(e.to_string()));
                    }
                    if thread_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let next = match options.poll_interval {
                        Some(every) => match control_rx.recv_timeout(every) {
                            Ok(c) => Some(c),
                            Err(mpsc::RecvTimeoutError::Timeout) => Some(Control::Wake),
                            Err(mpsc::RecvTimeoutError::Disconnected) => None,
                        },
                        None => control_rx.recv().ok(),
                    };
                    let Some(first) = next else { break };
                    if thread_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    // Coalesce what arrived meanwhile; the newest options win.
                    for c in std::iter::once(first).chain(control_rx.try_iter()) {
                        if let Control::Options(o) = c {
                            options = *o;
                        }
                    }
                }
                send(WorkerEvent::Stopped);
            })
            .map_err(Error::Spawn)?;
        Ok((
            MetadataWorker {
                control: Some(control_tx),
                stop,
                handle: Some(handle),
            },
            event_rx,
        ))
    }

    /// Asks the worker to look for new pending videos.
    pub fn wake(&self) {
        self.send(Control::Wake);
    }

    /// Changes the options from the next pass on, and starts one.
    pub fn set_options(&self, options: WorkerOptions) {
        self.send(Control::Options(Box::new(options)));
    }

    fn send(&self, c: Control) {
        if let Some(tx) = &self.control {
            // Fails only when the thread has exited, which `is_running` shows.
            let _ = tx.send(c);
        }
    }

    /// Whether the thread is still alive.
    pub fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Stops after the current video and waits for the thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Disconnecting the control channel wakes a sleeping thread.
        self.control.take();
        if let Some(h) = self.handle.take() {
            // A panic in the worker has already been reported by the panic
            // hook; there is nothing more to do with it here.
            let _ = h.join();
        }
    }
}

impl Drop for MetadataWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::jpeg_size;
    use crate::library::MediaUpsert;
    use fp_core::Projection;
    use fp_core::format::{DetectedFormat, Evidence};
    use fp_core::source::{Entry, EntryKind};
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    /// A prober that knows a fixed set of files and paints solid frames of
    /// the right (cropped, fitted) size.
    #[derive(Default)]
    struct FakeProber {
        infos: HashMap<String, ProbeInfo>,
        broken_thumbnails: HashSet<String>,
        calls: Mutex<Vec<(String, f64, ThumbnailCrop)>>,
        /// Thumbnail calls from this one on are cancelled.
        cancel_from_call: Option<usize>,
    }

    impl FakeProber {
        fn with(mut self, loc: &str, w: u32, h: u32, dur: f64, hints: ContainerHints) -> Self {
            self.infos.insert(
                loc.into(),
                ProbeInfo {
                    duration: Some(dur),
                    width: w,
                    height: h,
                    video_codec: Some("hevc".into()),
                    hints,
                },
            );
            self
        }

        fn calls_for(&self, loc: &str) -> Vec<(f64, ThumbnailCrop)> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.0 == loc)
                .map(|c| (c.1, c.2))
                .collect()
        }

        fn crops_for(&self, loc: &str) -> Vec<ThumbnailCrop> {
            self.calls_for(loc).into_iter().map(|c| c.1).collect()
        }
    }

    impl MediaProber for FakeProber {
        fn probe(&self, location: &str) -> Result<ProbeInfo> {
            self.infos
                .get(location)
                .cloned()
                .ok_or_else(|| Error::Probe(format!("cannot open {location}")))
        }

        fn thumbnail(
            &self,
            location: &str,
            at: f64,
            max_w: u32,
            max_h: u32,
            crop: ThumbnailCrop,
        ) -> Result<RgbaImage> {
            let mut calls = self.calls.lock().unwrap();
            if self.cancel_from_call.is_some_and(|n| calls.len() >= n) {
                return Err(Error::Cancelled);
            }
            calls.push((location.into(), at, crop));
            drop(calls);
            if self.broken_thumbnails.contains(location) {
                return Err(Error::Probe("decoder error".into()));
            }
            let info = self.probe(location)?;
            let (w, h) = match crop {
                ThumbnailCrop::LeftEye => (info.width / 2, info.height),
                ThumbnailCrop::TopEye => (info.width, info.height / 2),
                ThumbnailCrop::LeftEyeStretched
                | ThumbnailCrop::TopEyeStretched
                | ThumbnailCrop::Full => (info.width, info.height),
            };
            let (w, h) = crate::image::fit_size(w, h, max_w, max_h);
            Ok(RgbaImage::filled(w, h, [200, 100, 50, 255]))
        }
    }

    fn opts(dir: &Path) -> WorkerOptions {
        WorkerOptions {
            batch_size: 2,
            ..WorkerOptions::with_cache_dir(dir)
        }
    }

    fn run(
        lib: &Library,
        prober: &FakeProber,
        o: &WorkerOptions,
    ) -> (WorkerStats, Vec<WorkerEvent>) {
        let mut events = Vec::new();
        let stats = process_pending(lib, prober, o, &AtomicBool::new(false), &mut |e| {
            events.push(e);
        })
        .unwrap();
        (stats, events)
    }

    /// Plays `id` up to `at` seconds and stops.
    fn watch(lib: &Library, id: MediaId, at: f64) {
        let s = lib.start_playback(id).unwrap();
        lib.update_playback(s, at, false).unwrap();
    }

    #[test]
    fn a_source_location_uses_only_the_decoded_filename_for_probe_format_detection() {
        let lib = Library::open_in_memory().unwrap();
        let key = "VR_180_LR/Flat.mp4?projection=360_TB";
        let location = format!(
            "source://source/server/http/{}",
            percent_encoding::utf8_percent_encode(key, percent_encoding::NON_ALPHANUMERIC)
        );
        let id = lib
            .upsert(&MediaUpsert::new(&location, "server"))
            .unwrap()
            .id;
        let prober =
            FakeProber::default().with(&location, 1920, 1080, 30.0, ContainerHints::default());
        let cache = tempfile::tempdir().unwrap();
        let mut options = opts(cache.path());
        options.thumbnails = false;
        let (stats, _) = run(&lib, &prober, &options);
        assert_eq!(stats.processed, 1);
        let stored = lib.get(id).unwrap().unwrap();
        assert_eq!(stored.detected.format.projection, Projection::Flat);
        assert_eq!(stored.detected.format.stereo, StereoLayout::Mono);
    }

    #[test]
    fn crop_follows_stereo_layout() {
        let f = |s| VideoFormat::new(Projection::EQUIRECT_180, s);
        assert_eq!(
            ThumbnailCrop::for_format(&f(StereoLayout::SideBySide)),
            ThumbnailCrop::LeftEye
        );
        assert_eq!(
            ThumbnailCrop::for_format(&f(StereoLayout::HalfSideBySide)),
            ThumbnailCrop::LeftEyeStretched
        );
        assert_eq!(
            ThumbnailCrop::for_format(&f(StereoLayout::TopBottom)),
            ThumbnailCrop::TopEye
        );
        assert_eq!(
            ThumbnailCrop::for_format(&f(StereoLayout::HalfTopBottom)),
            ThumbnailCrop::TopEyeStretched
        );
        assert_eq!(
            ThumbnailCrop::for_format(&f(StereoLayout::Mono)),
            ThumbnailCrop::Full
        );
    }

    #[test]
    fn processes_pending_media() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let lib = Library::open_in_memory().unwrap();
        let sbs = lib
            .upsert(&MediaUpsert::new("/v/a_LR.mp4", "local"))
            .unwrap()
            .id;
        let meta = lib
            .upsert(&MediaUpsert::new("/v/b.mp4", "local"))
            .unwrap()
            .id;
        let aspect = lib
            .upsert(&MediaUpsert::new("/v/c.mp4", "local"))
            .unwrap()
            .id;
        let broken = lib
            .upsert(&MediaUpsert::new("/v/broken.mp4", "local"))
            .unwrap()
            .id;
        let prober = FakeProber::default()
            .with("/v/a_LR.mp4", 3840, 1080, 100.0, ContainerHints::default())
            .with(
                "/v/b.mp4",
                4096,
                2048,
                60.0,
                ContainerHints {
                    projection: Some(Projection::EQUIRECT_360),
                    stereo: Some(StereoLayout::Mono),
                    ..Default::default()
                },
            )
            .with("/v/c.mp4", 5760, 2880, 30.0, ContainerHints::default());
        let o = opts(&cache);
        assert_eq!(lib.pending_metadata_count(true).unwrap(), 4);

        let (stats, events) = run(&lib, &prober, &o);
        assert_eq!(
            stats,
            WorkerStats {
                processed: 3,
                failed: 1
            }
        );
        assert_eq!(events.first(), Some(&WorkerEvent::Started { pending: 4 }));
        assert_eq!(
            events.last(),
            Some(&WorkerEvent::Idle {
                processed: 3,
                failed: 1
            })
        );
        let dones: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::Processed { done, total, .. }
                | WorkerEvent::Failed { done, total, .. } => {
                    assert_eq!(*total, 4);
                    Some(*done)
                }
                _ => None,
            })
            .collect();
        assert_eq!(dones, vec![1, 2, 3, 4]);
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::Failed { id, .. } if *id == broken
        )));

        // SBS from the name: left-eye crop, square-ish thumbnail at 20 %.
        let a = lib.get(sbs).unwrap().unwrap();
        assert!(a.probed && a.probe_error.is_none());
        assert_eq!(
            (a.width, a.height, a.duration),
            (Some(3840), Some(1080), Some(100.0))
        );
        assert_eq!(a.video_codec.as_deref(), Some("hevc"));
        assert_eq!(a.detected.evidence, Evidence::FileName);
        assert_eq!(
            prober.calls_for("/v/a_LR.mp4"),
            vec![(20.0, ThumbnailCrop::LeftEye)]
        );
        let thumb = a.thumbnail.clone().unwrap();
        assert_eq!(thumb, cache.join(format!("{sbs}.jpg")));
        let jpg = std::fs::read(&thumb).unwrap();
        assert_eq!(jpeg_size(&jpg), Some((640, 360)));
        assert_eq!(a.picture(), Some(thumb.display().to_string()));

        // Container metadata wins over the name.
        let b = lib.get(meta).unwrap().unwrap();
        assert_eq!(b.detected.evidence, Evidence::Metadata);
        assert_eq!(b.detected.format.projection, Projection::EQUIRECT_360);
        assert_eq!(prober.crops_for("/v/b.mp4"), vec![ThumbnailCrop::Full]);

        // Aspect ratio: 2:1 at 5760 wide is SBS 180, so the filter finds it,
        // along with the unreadable file, which plays as the 180 SBS fallback.
        let c = lib.get(aspect).unwrap().unwrap();
        assert_eq!(c.detected.evidence, Evidence::AspectRatio);
        let found = lib
            .search(&crate::Query {
                projections: vec![crate::ProjectionKind::Equirect180],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            found.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![broken, aspect]
        );

        // The failure is recorded and not retried...
        let x = lib.get(broken).unwrap().unwrap();
        assert!(x.probe_error.unwrap().contains("cannot open"));
        assert_eq!(lib.pending_metadata_count(true).unwrap(), 0);
        let (stats, _) = run(&lib, &prober, &o);
        assert_eq!(stats, WorkerStats::default());
        // ...until asked.
        assert_eq!(lib.retry_failed_probes(Some("other")).unwrap(), 0);
        assert_eq!(lib.retry_failed_probes(None).unwrap(), 1);
        assert_eq!(lib.pending_metadata_count(true).unwrap(), 1);
    }

    #[test]
    fn a_servers_picture_is_used_rather_than_a_grab() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let mut e = Entry::new("Beach", "http://jf/item/1", EntryKind::Video);
        e.thumbnail_url = Some("http://jf/Items/1/Images/Thumb?tag=t1".into());
        let id = lib.upsert_entry("jellyfin", &e).unwrap().id;
        let prober = FakeProber::default().with(
            "http://jf/item/1",
            1920,
            1080,
            600.0,
            ContainerHints::default(),
        );
        let (stats, events) = run(&lib, &prober, &opts(tmp.path()));
        assert_eq!(stats.processed, 1);
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::Processed {
                thumbnail: false,
                warning: None,
                ..
            }
        )));
        assert!(prober.calls_for("http://jf/item/1").is_empty(), "no grab");
        let r = lib.get(id).unwrap().unwrap();
        assert_eq!((r.probed, r.duration), (true, Some(600.0)));
        assert!(r.thumbnail.is_none());
        assert_eq!(
            r.picture().as_deref(),
            Some("http://jf/Items/1/Images/Thumb?tag=t1")
        );
        assert_eq!(lib.pending_metadata_count(true).unwrap(), 0);

        // A browse entry carrying the library's own thumbnail file brings no
        // server picture with it.
        let mut e = Entry::new("Local", "smb://nas/v/a.mp4", EntryKind::Video);
        e.thumbnail_url = Some(tmp.path().join("7.jpg").display().to_string());
        let id = lib.upsert_entry("nas", &e).unwrap().id;
        assert_eq!(lib.get(id).unwrap().unwrap().thumbnail_url, None);
    }

    #[test]
    fn partly_watched_videos_show_where_they_were_left_off() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().to_path_buf();
        let lib = Library::open_in_memory().unwrap();
        let id = lib
            .upsert(&MediaUpsert::new("/v/a_180_LR.mp4", "local"))
            .unwrap()
            .id;
        let prober = FakeProber::default().with(
            "/v/a_180_LR.mp4",
            7680,
            3840,
            100.0,
            ContainerHints::default(),
        );
        let o = opts(&cache);
        run(&lib, &prober, &o);
        let thumb = lib.get(id).unwrap().unwrap().thumbnail.unwrap();

        watch(&lib, id, 40.5);
        let (_, events) = run(&lib, &prober, &o);
        assert!(events.contains(&WorkerEvent::ResumeThumbnail { id, error: None }));
        let r = lib.get(id).unwrap().unwrap();
        let first = cache.join(format!("{id}_at40.jpg"));
        assert_eq!(r.resume_thumbnail.as_ref(), Some(&first));
        assert_eq!(r.picture(), Some(first.display().to_string()));
        assert_eq!(
            jpeg_size(&std::fs::read(&first).unwrap()),
            Some((1280, 1280)),
            "one eye, banner sized"
        );
        assert_eq!(
            prober.calls_for("/v/a_180_LR.mp4").last(),
            Some(&(40.5, ThumbnailCrop::LeftEye))
        );
        // Nothing more to do until it is played again.
        let calls = prober.calls.lock().unwrap().len();
        run(&lib, &prober, &o);
        assert_eq!(prober.calls.lock().unwrap().len(), calls);

        // Watched further: a new frame under a new name, the old one goes.
        watch(&lib, id, 70.0);
        run(&lib, &prober, &o);
        let second = cache.join(format!("{id}_at70.jpg"));
        assert_eq!(
            lib.get(id).unwrap().unwrap().resume_thumbnail,
            Some(second.clone())
        );
        assert!(second.exists() && !first.exists());

        // Watched: back to the thumbnail, the frame deleted.
        watch(&lib, id, 95.0);
        let r = lib.get(id).unwrap().unwrap();
        assert_eq!(r.picture(), Some(thumb.display().to_string()));
        run(&lib, &prober, &o);
        assert_eq!(lib.get(id).unwrap().unwrap().resume_thumbnail, None);
        assert!(!second.exists() && thumb.exists());
    }

    #[test]
    fn a_frame_that_cannot_be_made_is_not_tried_again_for_that_position() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let mut u = MediaUpsert::new("/v/x.mp4", "local");
        u.duration = Some(100.0);
        let id = lib.upsert(&u).unwrap().id;
        let mut prober =
            FakeProber::default().with("/v/x.mp4", 1920, 1080, 100.0, ContainerHints::default());
        prober.broken_thumbnails.insert("/v/x.mp4".into());
        watch(&lib, id, 30.0);
        let (_, events) = run(&lib, &prober, &opts(tmp.path()));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WorkerEvent::ResumeThumbnail { error: Some(_), .. }))
        );
        assert!(lib.pending_resume_thumbnails(10).unwrap().is_empty());
        let r = lib.get(id).unwrap().unwrap();
        assert!(r.resume_thumbnail.is_none());
        assert_eq!(r.picture(), None);
    }

    #[test]
    fn changing_what_the_format_shows_makes_the_pictures_again() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let id = lib
            .upsert(&MediaUpsert::new("/v/wide.mp4", "local"))
            .unwrap()
            .id;
        let prober =
            FakeProber::default().with("/v/wide.mp4", 5760, 2880, 100.0, ContainerHints::default());
        let o = opts(tmp.path());
        run(&lib, &prober, &o);
        watch(&lib, id, 50.0);
        run(&lib, &prober, &o);
        assert_eq!(
            prober.crops_for("/v/wide.mp4"),
            vec![ThumbnailCrop::LeftEye; 2],
            "guessed SBS from its shape"
        );
        // Another projection, the same half of the frame: nothing to redo.
        let sbs360 = VideoFormat::new(Projection::EQUIRECT_360, StereoLayout::SideBySide);
        lib.set_user_format(id, Some(sbs360)).unwrap();
        run(&lib, &prober, &o);
        assert_eq!(prober.crops_for("/v/wide.mp4").len(), 2);
        // 2D: the whole frame, thumbnail and resume frame alike.
        let mono = VideoFormat::new(Projection::EQUIRECT_360, StereoLayout::Mono);
        lib.set_user_format(id, Some(mono)).unwrap();
        let r = lib.get(id).unwrap().unwrap();
        assert!(r.thumbnail.is_some(), "the old one shows until replaced");
        run(&lib, &prober, &o);
        assert_eq!(
            prober.crops_for("/v/wide.mp4")[2..],
            [ThumbnailCrop::Full; 2]
        );
    }

    #[test]
    fn keeps_stronger_detection_and_honours_user_format() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let mut u = MediaUpsert::new("http://srv/feed1.mp4", "deovr");
        u.detected = DetectedFormat {
            format: VideoFormat::new(Projection::fisheye(200.0), StereoLayout::SideBySide),
            evidence: Evidence::Metadata,
        };
        let declared = lib.upsert(&u).unwrap().id;
        let user = lib
            .upsert(&MediaUpsert::new("/v/plain.mp4", "local"))
            .unwrap()
            .id;
        lib.set_user_format(
            user,
            Some(VideoFormat::new(
                Projection::EQUIRECT_180,
                StereoLayout::TopBottom,
            )),
        )
        .unwrap();
        let prober = FakeProber::default()
            .with(
                "http://srv/feed1.mp4",
                1920,
                1080,
                10.0,
                ContainerHints::default(),
            )
            .with("/v/plain.mp4", 4000, 4000, 10.0, ContainerHints::default());
        run(&lib, &prober, &opts(tmp.path()));

        assert_eq!(lib.get(declared).unwrap().unwrap().detected, u.detected);
        assert_eq!(
            prober.crops_for("http://srv/feed1.mp4"),
            vec![ThumbnailCrop::LeftEye]
        );
        assert_eq!(
            prober.crops_for("/v/plain.mp4"),
            vec![ThumbnailCrop::TopEye]
        );
        let r = lib.get(user).unwrap().unwrap();
        assert_eq!(r.effective_format().evidence, Evidence::User);
        assert_eq!(r.detected.evidence, Evidence::AspectRatio);
    }

    #[test]
    fn thumbnail_failure_keeps_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let id = lib
            .upsert(&MediaUpsert::new("/v/x.mp4", "local"))
            .unwrap()
            .id;
        let mut prober =
            FakeProber::default().with("/v/x.mp4", 1920, 1080, 10.0, ContainerHints::default());
        prober.broken_thumbnails.insert("/v/x.mp4".into());
        let (stats, events) = run(&lib, &prober, &opts(tmp.path()));
        assert_eq!(stats.processed, 1);
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::Processed { thumbnail: false, warning: Some(w), .. }
                if w.contains("thumbnail")
        )));
        let r = lib.get(id).unwrap().unwrap();
        assert_eq!(r.duration, Some(10.0));
        assert!(r.thumbnail.is_none());
        assert!(r.probe_error.is_some());
        assert_eq!(lib.pending_metadata_count(true).unwrap(), 0);
    }

    /// Probes like `inner`, but a rescan finds a new file while the first
    /// probe of each video runs, and that first probe sees the old one.
    struct RescannedMidProbe {
        inner: FakeProber,
        lib: Library,
        probes: Mutex<usize>,
    }

    impl MediaProber for RescannedMidProbe {
        fn probe(&self, location: &str) -> Result<ProbeInfo> {
            let mut info = self.inner.probe(location)?;
            let mut probes = self.probes.lock().unwrap();
            *probes += 1;
            if *probes == 1 {
                let mut u = MediaUpsert::new(location, "local");
                u.size = Some(2);
                self.lib.upsert(&u).unwrap();
                info.duration = Some(10.0);
            }
            Ok(info)
        }

        fn thumbnail(
            &self,
            location: &str,
            at: f64,
            max_w: u32,
            max_h: u32,
            crop: ThumbnailCrop,
        ) -> Result<RgbaImage> {
            self.inner.thumbnail(location, at, max_w, max_h, crop)
        }
    }

    #[test]
    fn a_file_replaced_while_probed_is_probed_again() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let mut u = MediaUpsert::new("/v/x.mp4", "local");
        u.size = Some(1);
        let id = lib.upsert(&u).unwrap().id;
        let prober = RescannedMidProbe {
            inner: FakeProber::default().with(
                "/v/x.mp4",
                1920,
                1080,
                120.0,
                ContainerHints::default(),
            ),
            lib: lib.clone(),
            probes: Mutex::new(0),
        };
        let stop = AtomicBool::new(false);
        let stats = process_pending(&lib, &prober, &opts(tmp.path()), &stop, &mut |_| {}).unwrap();
        // The old file's 10 s were dropped; the new file was probed.
        assert_eq!(*prober.probes.lock().unwrap(), 2);
        assert_eq!(stats.processed, 1);
        let r = lib.get(id).unwrap().unwrap();
        assert_eq!((r.size, r.duration, r.probed), (Some(2), Some(120.0), true));
        assert_eq!(lib.pending_metadata_count(true).unwrap(), 0);
    }

    #[test]
    fn cancelled_video_stays_pending_without_error() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        for loc in ["/v/a.mp4", "/v/b.mp4"] {
            lib.upsert(&MediaUpsert::new(loc, "local")).unwrap();
        }
        let mut prober = FakeProber::default()
            .with("/v/a.mp4", 1920, 1080, 10.0, ContainerHints::default())
            .with("/v/b.mp4", 1920, 1080, 10.0, ContainerHints::default());
        // Cancel the second video's thumbnail.
        prober.cancel_from_call = Some(1);
        let (stats, events) = run(&lib, &prober, &opts(tmp.path()));
        assert_eq!((stats.processed, stats.failed), (1, 0));
        assert!(matches!(events.last(), Some(WorkerEvent::Idle { .. })));
        assert_eq!(lib.pending_metadata_count(true).unwrap(), 1);
        let pending = lib.pending_metadata(10, true).unwrap();
        assert!(pending[0].probe_error.is_none());
        assert!(pending[0].thumbnail.is_none());

        // The next pass finishes it.
        prober.cancel_from_call = None;
        let (stats, _) = run(&lib, &prober, &opts(tmp.path()));
        assert_eq!(stats.processed, 1);
        assert_eq!(lib.pending_metadata_count(true).unwrap(), 0);
    }

    #[test]
    fn rescan_of_changed_file_requeues_it() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let mut u = MediaUpsert::new("/v/x.mp4", "local");
        u.size = Some(1);
        lib.upsert(&u).unwrap();
        let prober =
            FakeProber::default().with("/v/x.mp4", 1920, 1080, 10.0, ContainerHints::default());
        run(&lib, &prober, &opts(tmp.path()));
        assert_eq!(lib.pending_metadata_count(true).unwrap(), 0);
        u.size = Some(2);
        lib.upsert(&u).unwrap();
        assert_eq!(lib.pending_metadata_count(true).unwrap(), 1);
    }

    #[test]
    fn migrating_a_location_during_a_failed_probe_leaves_the_new_identity_pending() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib
            .upsert(&MediaUpsert::new("http://old/movie.mp4", "server"))
            .unwrap()
            .id;
        let snapshot = lib.get(id).unwrap().unwrap();
        let stable = "source://source/server/http/movie.mp4";
        lib.rewrite_source_locations("server", |_| Some(stable.into()))
            .unwrap();
        let mut stats = WorkerStats::default();
        let mut events = Vec::new();
        report(
            &lib,
            &snapshot,
            1,
            1,
            Err(Error::Probe("old endpoint unavailable".into())),
            &mut stats,
            &mut |event| events.push(event),
        )
        .unwrap();
        assert_eq!(stats.failed, 0);
        assert!(events.is_empty());
        let current = lib.get(id).unwrap().unwrap();
        assert_eq!(current.location, stable);
        assert!(current.probe_error.is_none());
        assert_eq!(lib.pending_metadata_count(false).unwrap(), 1);

        let prober =
            FakeProber::default().with(stable, 1920, 1080, 10.0, ContainerHints::default());
        let cache = tempfile::tempdir().unwrap();
        let mut options = opts(cache.path());
        options.thumbnails = false;
        let (stats, _) = run(&lib, &prober, &options);
        assert_eq!(stats.processed, 1);
        assert!(lib.get(id).unwrap().unwrap().probed);
    }

    #[test]
    fn files_no_video_uses_are_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().to_path_buf();
        let lib = Library::open_in_memory().unwrap();
        let kept = lib
            .upsert(&MediaUpsert::new("/v/kept.mp4", "a"))
            .unwrap()
            .id;
        let gone = lib
            .upsert(&MediaUpsert::new("/v/gone.mp4", "b"))
            .unwrap()
            .id;
        let prober = FakeProber::default()
            .with("/v/kept.mp4", 1920, 1080, 10.0, ContainerHints::default())
            .with("/v/gone.mp4", 1920, 1080, 10.0, ContainerHints::default());
        let o = opts(&cache);
        run(&lib, &prober, &o);
        let kept_jpg = cache.join(format!("{kept}.jpg"));
        let gone_jpg = cache.join(format!("{gone}.jpg"));
        assert!(kept_jpg.exists() && gone_jpg.exists());
        // Source `b` removed: its unwatched video leaves the library. Beside
        // it, an old preview strip, an interrupted write and a stranger.
        assert_eq!(lib.forget_unwatched("b").unwrap(), 1);
        let strip = cache.join(format!("{kept}_strip.jpg"));
        let partial = cache.join("9.jpg.tmp");
        let stranger = cache.join("notes.txt");
        for p in [&strip, &partial, &stranger] {
            std::fs::write(p, b"x").unwrap();
        }
        run(&lib, &prober, &o);
        assert!(kept_jpg.exists() && stranger.exists());
        assert!(!gone_jpg.exists() && !strip.exists() && !partial.exists());
    }

    #[test]
    fn thumbnails_can_be_turned_off() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let id = lib
            .upsert(&MediaUpsert::new("/v/x.mp4", "local"))
            .unwrap()
            .id;
        let prober =
            FakeProber::default().with("/v/x.mp4", 1920, 1080, 100.0, ContainerHints::default());
        let on = opts(tmp.path());
        let off = WorkerOptions {
            thumbnails: false,
            ..on.clone()
        };
        let (stats, _) = run(&lib, &prober, &off);
        assert_eq!(stats.processed, 1, "probed all the same");
        assert!(prober.calls_for("/v/x.mp4").is_empty());
        let r = lib.get(id).unwrap().unwrap();
        assert!(r.probed && r.thumbnail.is_none());
        // Nothing to do while off; turned on, it is made.
        assert_eq!(lib.pending_metadata_count(false).unwrap(), 0);
        assert_eq!(lib.pending_metadata_count(true).unwrap(), 1);
        run(&lib, &prober, &on);
        let thumb = lib.get(id).unwrap().unwrap().thumbnail;
        assert!(thumb.is_some());
        // Off again, a video probed anew keeps the one it has.
        let sbs = VideoFormat::new(Projection::Flat, StereoLayout::SideBySide);
        lib.set_user_format(id, Some(sbs)).unwrap();
        run(&lib, &prober, &off);
        assert_eq!(prober.calls_for("/v/x.mp4").len(), 1);
        assert_eq!(lib.get(id).unwrap().unwrap().thumbnail, thumb);
    }

    #[test]
    fn frames_where_videos_were_left_off_can_be_turned_off() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let mut u = MediaUpsert::new("/v/x.mp4", "local");
        u.duration = Some(100.0);
        let id = lib.upsert(&u).unwrap().id;
        let prober =
            FakeProber::default().with("/v/x.mp4", 1920, 1080, 100.0, ContainerHints::default());
        let on = opts(tmp.path());
        watch(&lib, id, 40.0);
        run(&lib, &prober, &on);
        let frame = lib.get(id).unwrap().unwrap().resume_thumbnail.unwrap();
        assert!(frame.exists());
        let off = WorkerOptions {
            resume_thumbnails: false,
            ..on
        };
        let (_, events) = run(&lib, &prober, &off);
        assert!(events.contains(&WorkerEvent::ResumeThumbnailsDeleted(1)));
        assert!(!frame.exists());
        let r = lib.get(id).unwrap().unwrap();
        assert!(r.resume_thumbnail.is_none());
        assert_eq!(r.picture(), r.thumbnail.map(|p| p.display().to_string()));
        // Played on, still none.
        watch(&lib, id, 60.0);
        run(&lib, &prober, &off);
        assert!(!prober.calls_for("/v/x.mp4").iter().any(|c| c.0 == 60.0));
    }

    fn wait_for_idle(rx: &mpsc::Receiver<WorkerEvent>) -> Vec<WorkerEvent> {
        let mut seen = Vec::new();
        loop {
            let e = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("worker went quiet");
            let idle = matches!(e, WorkerEvent::Idle { .. });
            seen.push(e);
            if idle {
                return seen;
            }
        }
    }

    #[test]
    fn background_worker_runs_wakes_and_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let first = lib
            .upsert(&MediaUpsert::new("/v/1.mp4", "local"))
            .unwrap()
            .id;
        let prober = Arc::new(
            FakeProber::default()
                .with("/v/1.mp4", 1920, 1080, 10.0, ContainerHints::default())
                .with(
                    "/v/2_180_LR.mp4",
                    4000,
                    2000,
                    10.0,
                    ContainerHints::default(),
                )
                .with("/v/3.mp4", 1920, 1080, 10.0, ContainerHints::default()),
        );
        let (worker, rx) =
            MetadataWorker::spawn(lib.clone(), prober.clone(), opts(tmp.path())).unwrap();
        let events = wait_for_idle(&rx);
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::Processed { id, thumbnail: true, .. } if *id == first
        )));
        assert!(worker.is_running());

        let second = lib
            .upsert(&MediaUpsert::new("/v/2_180_LR.mp4", "local"))
            .unwrap()
            .id;
        worker.wake();
        let events = wait_for_idle(&rx);
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::Processed { id, .. } if *id == second
        )));
        assert!(lib.get(second).unwrap().unwrap().thumbnail.is_some());

        // New options start a pass of their own and hold for the next.
        worker.set_options(WorkerOptions {
            thumbnails: false,
            ..opts(tmp.path())
        });
        wait_for_idle(&rx);
        let third = lib
            .upsert(&MediaUpsert::new("/v/3.mp4", "local"))
            .unwrap()
            .id;
        worker.wake();
        wait_for_idle(&rx);
        let r = lib.get(third).unwrap().unwrap();
        assert!(r.probed && r.thumbnail.is_none());

        worker.stop();
        let rest: Vec<WorkerEvent> = rx.iter().collect();
        assert_eq!(rest.last(), Some(&WorkerEvent::Stopped));
    }

    #[test]
    fn worker_reports_cache_errors_and_drop_stops_it() {
        let tmp = tempfile::tempdir().unwrap();
        // A file where the cache folder should be.
        let blocker = tmp.path().join("cache");
        std::fs::write(&blocker, b"x").unwrap();
        let lib = Library::open_in_memory().unwrap();
        lib.upsert(&MediaUpsert::new("/v/1.mp4", "local")).unwrap();
        let prober = Arc::new(FakeProber::default());
        let (worker, rx) = MetadataWorker::spawn(lib, prober, opts(&blocker)).unwrap();
        let mut got_error = false;
        while let Ok(e) = rx.recv_timeout(Duration::from_secs(10)) {
            if matches!(e, WorkerEvent::Error(_)) {
                got_error = true;
                break;
            }
        }
        assert!(got_error);
        drop(worker);
        assert_eq!(rx.iter().last(), Some(WorkerEvent::Stopped));
    }
}
