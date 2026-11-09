//! FramePlayerX's media pipeline on top of the bundled FFmpeg.
//!
//! - `io`: FFmpeg reads through [`fp_core::ByteSource`], so all network
//!   access stays in Rust.
//! - [`info`]: stream inspection, including spherical/stereo metadata and HDR.
//! - `decode`: video (hardware V4L2 first, software fallback), audio and
//!   subtitle decoders.
//! - `v4l2`: the native V4L2 stateful decoder for H.264/HEVC (10-bit HEVC as
//!   Q10C), with the Steam Frame's admission-check workaround.
//! - `mvc`: 3D Blu-ray video (H.264 MVC) on a plain H.264 decoder;
//! - `frame_seq`: frame sequential 3D, its eyes paired into one picture;
//!   `second_view`: its second view read from transport streams.
//! - `bluray`: Blu-ray disc images, their main title played clip after clip.
//! - [`frame`]: decoded video frames in GPU-uploadable layouts.
//! - [`player`]: the threaded player with a video-master clock.
//! - [`audio`]: output, time-stretch and ambisonic rendering.
//! - [`thumb`]: probing and frame grabs for the library.

pub mod audio;
pub mod bench;
pub(crate) mod bluray;
pub mod clock;
pub(crate) mod decode;
pub mod frame;
pub(crate) mod frame_seq;
pub mod info;
pub(crate) mod input;
pub(crate) mod io;
pub(crate) mod mvc;
pub(crate) mod nal;
pub mod player;
pub(crate) mod second_view;
pub mod subtitle;
pub mod thumb;
#[cfg(test)]
pub(crate) mod ts_fixture;
pub(crate) mod v4l2;

pub use decode::{
    HwDecode, hardware_sessions, hevc_10bit_hardware, set_hevc_10bit_hardware, set_shared_frames,
    shared_frames,
};
pub use frame::{ColorInfo, PixelLayout, Q10cPicture, SharedPicture, VideoFrame};
pub use info::{MediaInfo, StreamInfo, StreamKind};
pub use player::{LastJump, Player, PlayerConfig, PlayerState, Restart, Stats, Timeline, Tracks};
pub use subtitle::Cue;

use fp_ffmpeg_sys as ff;

/// Errors from the media pipeline.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{context}: {message}")]
    Ffmpeg {
        context: &'static str,
        code: i32,
        message: String,
    },
    #[error("no {0} stream")]
    NoStream(&'static str),
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// A disc image that can't be read as a Blu-ray.
    #[error("disc image: {0}")]
    Disc(String),
    /// The hardware decoder has no room for the stream beside the other
    /// video sessions open, encoders' too (Steam's recording holds one).
    #[error("hardware decoder busy: {0}")]
    HardwareBusy(String, SessionHolders),
    /// The stream is more than the hardware decoder takes for one session,
    /// even with nothing else open.
    #[error("too big for the hardware decoder: {0}")]
    HardwareTooBig(String),
    #[error("audio output: {0}")]
    Audio(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("interrupted")]
    Interrupted,
}

impl Error {
    /// The file was read but can't play here: a codec or container this
    /// build has no decoder or demuxer for, no video or audio in it, or a
    /// picture the hardware decoder has no room for. The server
    /// transcoding it may play where it didn't; a read that failed may not.
    pub fn cannot_play_here(&self) -> bool {
        match self {
            Error::NoStream(_)
            | Error::Unsupported(_)
            | Error::HardwareBusy(..)
            | Error::HardwareTooBig(_) => true,
            Error::Ffmpeg { code, .. } => {
                *code == ff::AVERROR_INVALIDDATA_
                    || *code == ff::AVERROR_DECODER_NOT_FOUND_
                    || *code == ff::AVERROR_DEMUXER_NOT_FOUND_
            }
            Error::Disc(_) | Error::Audio(_) | Error::Io(_) | Error::Interrupted => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Who else had sessions on the hardware decoder's driver when it found no
/// room for a stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionHolders {
    /// The other programs with sessions open, as far as `/proc` shows.
    pub programs: Vec<String>,
    /// Steam holds the encoder to record or stream, and the stream would fit
    /// without it.
    pub recording_blocks: bool,
}

/// Turns a negative FFmpeg return code into an [`Error`].
pub(crate) fn check(code: i32, context: &'static str) -> Result<i32> {
    if code < 0 {
        Err(Error::Ffmpeg {
            context,
            code,
            message: ff::err_to_string(code),
        })
    } else {
        Ok(code)
    }
}

/// Routes FFmpeg's own log output through the `log` crate (target
/// `ffmpeg`) instead of stderr. FFmpeg errors are often expected (a missing
/// hardware decoder we fall back from), so they are logged at info level.
pub fn init_logging() {
    unsafe extern "C" fn callback(
        avcl: *mut std::ffi::c_void,
        level: std::ffi::c_int,
        fmt: *const std::ffi::c_char,
        vl: ff::LogVaList,
    ) {
        if level > ff::AV_LOG_WARNING as i32 {
            return;
        }
        let mut buf = [0 as std::ffi::c_char; 1024];
        let mut prefix: std::ffi::c_int = 1;
        // SAFETY: arguments come straight from FFmpeg's av_log; the buffer
        // is NUL-terminated by av_log_format_line2 (it truncates).
        let line = unsafe {
            ff::av_log_format_line2(
                avcl,
                level,
                fmt,
                vl,
                buf.as_mut_ptr(),
                buf.len() as i32,
                &mut prefix,
            );
            std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy()
        };
        let line = line.trim_end();
        if line.is_empty() {
            return;
        }
        if level <= ff::AV_LOG_FATAL as i32 {
            log::error!(target: "ffmpeg", "{line}");
        } else if level <= ff::AV_LOG_ERROR as i32 {
            log::info!(target: "ffmpeg", "{line}");
        } else {
            log::debug!(target: "ffmpeg", "{line}");
        }
    }
    // SAFETY: installs a thread-safe callback on global FFmpeg state.
    unsafe {
        ff::av_log_set_level(ff::AV_LOG_WARNING as i32);
        ff::av_log_set_callback(Some(callback));
    }
}

/// FFmpeg rational to f64 seconds multiplier.
pub(crate) fn q2d(q: ff::AVRational) -> f64 {
    if q.den == 0 {
        0.0
    } else {
        q.num as f64 / q.den as f64
    }
}
