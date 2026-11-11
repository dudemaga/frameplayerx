//! The library's metadata worker, implemented with FFmpeg through fp-media.

use crate::services::Opener;
use fp_library::{MediaProber, ProbeInfo, RgbaImage, ThumbnailCrop};
use fp_media::thumb::{Crop, Grabber};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub struct FfmpegProber {
    pub opener: Arc<Opener>,
    /// Set while a video plays and when quitting: probes stop at the next
    /// read and the video stays pending. Thumbnails are decoded in
    /// software, and a 10-bit 4K file over SMB would otherwise take the
    /// CPU and the network from playback for minutes (and quitting waited
    /// for it).
    pub busy: Arc<AtomicBool>,
    /// The video probed last, still open for its thumbnail: the worker
    /// asks for it right after the probe.
    open: Mutex<Option<(String, Grabber)>>,
}

impl FfmpegProber {
    pub fn new(opener: Arc<Opener>, busy: Arc<AtomicBool>) -> FfmpegProber {
        FfmpegProber {
            opener,
            busy,
            open: Mutex::new(None),
        }
    }

    /// Maps an error, as cancelled when `busy` caused it.
    fn err(&self, e: impl std::fmt::Display) -> fp_library::Error {
        if self.busy.load(Ordering::Acquire) {
            fp_library::Error::Cancelled
        } else {
            fp_library::Error::Probe(e.to_string())
        }
    }

    fn check(&self) -> fp_library::Result<()> {
        if self.busy.load(Ordering::Acquire) {
            Err(fp_library::Error::Cancelled)
        } else {
            Ok(())
        }
    }

    fn grabber(&self, location: &str) -> fp_library::Result<Grabber> {
        background();
        let src = self.opener.open(location).map_err(|e| self.err(e))?;
        Grabber::open(src, location, self.busy.clone()).map_err(|e| self.err(e))
    }

    /// `location` open for thumbnails: as its probe left it, or anew.
    fn take(&self, location: &str) -> fp_library::Result<Grabber> {
        let open = self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match open {
            Some((at, g)) if at == location => Ok(g),
            _ => self.grabber(location),
        }
    }
}

/// Lowers the calling thread's priority, once: thumbnails are background
/// work, and the interface and the audio come first.
pub(crate) fn background() {
    thread_local! {
        static LOWERED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if LOWERED.replace(true) {
        return;
    }
    #[cfg(target_os = "linux")]
    // SAFETY: a plain call; Linux keeps a nice value per thread, and 0 is
    // the calling one.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
}

fn crop(crop: ThumbnailCrop) -> Crop {
    match crop {
        ThumbnailCrop::LeftEye => Crop::LeftHalf,
        ThumbnailCrop::LeftEyeStretched => Crop::LeftHalfStretched,
        ThumbnailCrop::TopEye => Crop::TopHalf,
        ThumbnailCrop::TopEyeStretched => Crop::TopHalfStretched,
        ThumbnailCrop::Full => Crop::Full,
    }
}

impl MediaProber for FfmpegProber {
    fn probe(&self, location: &str) -> fp_library::Result<ProbeInfo> {
        self.check()?;
        let g = self.grabber(location)?;
        let info = g.info();
        let v = info.video_stream();
        let (width, height) = v.map(fp_media::StreamInfo::display_size).unwrap_or((0, 0));
        let probed = ProbeInfo {
            duration: (info.duration > 0.0).then_some(info.duration),
            width,
            height,
            video_codec: v.map(|v| v.codec.clone()),
            hints: info.hints,
        };
        *self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((location.to_string(), g));
        Ok(probed)
    }

    fn thumbnail(
        &self,
        location: &str,
        at_seconds: f64,
        max_width: u32,
        max_height: u32,
        crop: ThumbnailCrop,
    ) -> fp_library::Result<RgbaImage> {
        self.check()?;
        let img = self
            .take(location)?
            .grab(at_seconds, max_width, max_height, self::crop(crop))
            .map_err(|e| self.err(e))?;
        Ok(RgbaImage {
            width: img.width,
            height: img.height,
            pixels: img.pixels,
        })
    }
}
