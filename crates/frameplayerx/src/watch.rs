//! Watch sync from the open video (see [`fp_sources::watch`]): its server
//! hears when it starts, pauses, resumes, jumps, changes speed and stops,
//! and where it is every few seconds. Reports go out on one thread in
//! order, so a slow server never holds up the frame loop and one video's
//! "stopped" reaches the server before the next one's "started".

use fp_core::source::Entry;
use fp_sources::{Source, WatchEvent, WatchKind, WatchSession};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// How often a server hears where playback is between changes.
const TICK: Duration = Duration::from_secs(10);

/// A jump between two frames bigger than this, in seconds, is a seek.
const SEEK: f64 = 1.5;

struct Report {
    source: Arc<dyn Source>,
    session: Arc<WatchSession>,
    event: WatchEvent,
}

/// Queues reports for the sending thread; cheap to clone.
#[derive(Clone)]
pub struct Reports {
    tx: Sender<Report>,
    /// Reports queued and not yet answered.
    pending: Arc<AtomicUsize>,
}

impl Reports {
    /// Starts the thread that sends reports.
    pub fn start() -> Reports {
        let (tx, rx) = channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let p = pending.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("fp-watch".into())
            .spawn(move || deliver(&rx, &p))
        {
            log::warn!("no watch sync: cannot start its thread: {e}");
        }
        Reports { tx, pending }
    }

    /// Waits up to `limit` for queued reports to go out, as the app quits:
    /// otherwise the last video's "stopped" is lost.
    pub fn flush(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        while self.pending.load(Ordering::Acquire) > 0 {
            if Instant::now() >= deadline {
                log::warn!("watch sync: quitting with reports unsent");
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn send(&self, report: Report) {
        self.pending.fetch_add(1, Ordering::AcqRel);
        if self.tx.send(report).is_err() {
            self.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

fn deliver(rx: &Receiver<Report>, pending: &AtomicUsize) {
    // The session that failed last: a server that is down fails every
    // report of a playback, which warns once.
    let mut failing = String::new();
    for r in rx {
        // Behind a slow server, a newer report makes a position update moot.
        let stale = r.event.kind == WatchKind::Tick && pending.load(Ordering::Acquire) > 1;
        if !stale && let Err(e) = r.source.report_watch(&r.session, r.event) {
            if failing != r.session.id {
                log::warn!(
                    "watch sync with {}: {:?} not taken: {e}",
                    r.source.name(),
                    r.event.kind
                );
                failing.clone_from(&r.session.id);
            } else {
                log::debug!("watch sync: {:?} not taken: {e}", r.event.kind);
            }
        }
        pending.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The open video's watch sync with its server.
pub struct Watch {
    reports: Reports,
    source: Arc<dyn Source>,
    session: Arc<WatchSession>,
    /// Position, pause and speed seen last frame, and when.
    seen: (f64, bool, f64, Instant),
    last_report: Instant,
}

impl Watch {
    /// Tells the server `video` came from that it started at `position`,
    /// when the server keeps track. May ask the server (see
    /// [`Source::watch_session`]).
    pub fn start(
        reports: &Reports,
        source: Arc<dyn Source>,
        video: &Entry,
        position: f64,
    ) -> Option<Watch> {
        let session = Arc::new(source.watch_session(video)?);
        Some(Self::start_session(
            reports,
            source,
            (*session).clone(),
            position,
        ))
    }

    /// Starts reports using the session that opened the media stream.
    pub fn start_session(
        reports: &Reports,
        source: Arc<dyn Source>,
        session: WatchSession,
        position: f64,
    ) -> Watch {
        let session = Arc::new(session);
        let mut w = Watch {
            reports: reports.clone(),
            source,
            session,
            seen: (position, false, 1.0, Instant::now()),
            last_report: Instant::now(),
        };
        w.send(WatchKind::Start, position, false, 1.0);
        w
    }

    /// Every frame: a pause, resume, seek or change of speed goes out at
    /// once, the position every [`TICK`]. Until the video shows its first
    /// picture (`ready`), nothing is reported: it is still opening.
    pub fn update(&mut self, position: f64, paused: bool, speed: f64, ready: bool) {
        if !ready {
            self.seen.3 = Instant::now();
            return;
        }
        let (before, was_paused, was_speed, at) = self.seen;
        let kind = change(
            (before, was_paused, was_speed),
            at.elapsed().as_secs_f64(),
            (position, paused, speed),
        )
        .or_else(|| (self.last_report.elapsed() >= TICK).then_some(WatchKind::Tick));
        self.seen = (position, paused, speed, Instant::now());
        if let Some(kind) = kind {
            self.send(kind, position, paused, speed);
        }
    }

    /// Tells the server the video closed at `position`.
    pub fn stop(mut self, position: f64) {
        let (_, paused, speed, _) = self.seen;
        self.send(WatchKind::Stop, position, paused, speed);
    }

    fn send(&mut self, kind: WatchKind, position: f64, paused: bool, speed: f64) {
        self.last_report = Instant::now();
        self.reports.send(Report {
            source: self.source.clone(),
            session: self.session.clone(),
            event: WatchEvent {
                kind,
                position,
                paused,
                speed,
            },
        });
    }
}

/// What changed between two frames `elapsed` seconds apart, each seen as
/// (position, paused, speed).
fn change(before: (f64, bool, f64), elapsed: f64, now: (f64, bool, f64)) -> Option<WatchKind> {
    if now.1 != before.1 {
        return Some(if now.1 {
            WatchKind::Pause
        } else {
            WatchKind::Resume
        });
    }
    let expected = before.0 + if now.1 { 0.0 } else { elapsed * before.2 };
    if (now.0 - expected).abs() > SEEK {
        Some(WatchKind::Seek)
    } else if (now.2 - before.2).abs() > 1e-3 {
        Some(WatchKind::Speed)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_sources::{Result, SourceKind};
    use std::sync::Mutex;

    #[test]
    fn changes_between_frames() {
        let frame = 1.0 / 90.0;
        let playing = (10.0, false, 1.0);
        assert_eq!(change(playing, frame, (10.0 + frame, false, 1.0)), None);
        // A stall while buffering: the picture holds, nothing jumped.
        assert_eq!(change(playing, frame, (10.0, false, 1.0)), None);
        assert_eq!(
            change(playing, frame, (10.0, true, 1.0)),
            Some(WatchKind::Pause)
        );
        assert_eq!(
            change((10.0, true, 1.0), frame, (10.0, false, 1.0)),
            Some(WatchKind::Resume)
        );
        assert_eq!(
            change(playing, frame, (40.0, false, 1.0)),
            Some(WatchKind::Seek)
        );
        assert_eq!(
            change(playing, frame, (2.0, false, 1.0)),
            Some(WatchKind::Seek)
        );
        assert_eq!(
            change((10.0, true, 1.0), frame, (30.0, true, 1.0)),
            Some(WatchKind::Seek)
        );
        assert_eq!(
            change(playing, frame, (10.0, false, 2.0)),
            Some(WatchKind::Speed)
        );
        // Two seconds between frames at double speed is four into the video.
        assert_eq!(change((10.0, false, 2.0), 2.0, (14.0, false, 2.0)), None);
    }

    /// A source that records what it is told.
    struct Recorder(Mutex<Vec<(String, WatchKind)>>);

    impl Source for Recorder {
        fn id(&self) -> &str {
            "rec"
        }
        fn name(&self) -> &str {
            "Recorder"
        }
        fn kind(&self) -> SourceKind {
            SourceKind::MediaBrowser
        }
        fn describe(&self) -> String {
            String::new()
        }
        fn list(&self, _: Option<&str>) -> Result<Vec<Entry>> {
            Ok(Vec::new())
        }
        fn open(&self, l: &str) -> Result<Arc<dyn fp_core::ByteSource>> {
            Err(fp_sources::Error::NotFound(l.into()))
        }
        fn watch_session(&self, video: &Entry) -> Option<WatchSession> {
            let mut s = Recorder::session(&video.location);
            s.title.clone_from(&video.name);
            Some(s)
        }
        fn report_watch(&self, session: &WatchSession, event: WatchEvent) -> Result<()> {
            // Slow enough that reports queue up behind each other.
            std::thread::sleep(Duration::from_millis(30));
            self.0
                .lock()
                .unwrap()
                .push((session.item.clone(), event.kind));
            Ok(())
        }
    }

    impl Recorder {
        fn session(item: &str) -> WatchSession {
            WatchSession {
                duration_ms: None,
                media_id: None,
                audio_stream: None,
                subtitle_stream: None,
                item: item.into(),
                title: String::new(),
                endpoint: None,
                id: item.into(),
            }
        }
    }

    #[test]
    fn videos_report_in_order_and_quitting_waits() {
        let reports = Reports::start();
        let rec = Arc::new(Recorder(Mutex::default()));
        let video = |l: &str| Entry::new(l, l, fp_core::EntryKind::Video);
        let a = Watch::start(&reports, rec.clone(), &video("a"), 0.0).unwrap();
        a.stop(5.0);
        let mut b = Watch::start(&reports, rec.clone(), &video("b"), 0.0).unwrap();
        b.update(0.0, false, 1.0, true);
        b.update(0.0, true, 1.0, true);
        b.stop(0.0);
        reports.flush(Duration::from_secs(5));
        let got = rec.0.lock().unwrap().clone();
        let want: Vec<(String, WatchKind)> = [
            ("a", WatchKind::Start),
            ("a", WatchKind::Stop),
            ("b", WatchKind::Start),
            ("b", WatchKind::Pause),
            ("b", WatchKind::Stop),
        ]
        .iter()
        .map(|(i, k)| (i.to_string(), *k))
        .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn nothing_is_reported_while_opening() {
        let reports = Reports::start();
        let rec = Arc::new(Recorder(Mutex::default()));
        let v = Entry::new("v", "v", fp_core::EntryKind::Video);
        let mut w = Watch::start(&reports, rec.clone(), &v, 120.0).unwrap();
        // The position reads 0 and then the resume point while opening.
        w.update(0.0, false, 1.0, false);
        w.update(120.0, false, 1.0, false);
        w.update(120.0, false, 1.0, true);
        w.stop(120.0);
        reports.flush(Duration::from_secs(5));
        let kinds: Vec<WatchKind> = rec.0.lock().unwrap().iter().map(|r| r.1).collect();
        assert_eq!(kinds, [WatchKind::Start, WatchKind::Stop]);
    }
}
