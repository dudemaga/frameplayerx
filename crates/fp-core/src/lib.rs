//! Types shared by every FramePlayerX crate.
//!
//! - [`format`](mod@format): how a video maps onto the sphere or screen ([`Projection`],
//!   [`StereoLayout`]) and detection from file names and metadata.
//! - [`view`]: per-video picture corrections ([`ViewSettings`]).
//! - [`source`]: the contract between media sources and the player
//!   ([`ByteSource`], [`Entry`]).
//! - [`playback`]: playback status and commands shared with haptics and the
//!   remote-control API.
//! - [`language`]: language tags of subtitle and audio tracks.
//! - [`dirs`]: where FramePlayerX keeps config, data and cache.
//! - [`sync`]: locking that survives a panicking thread.
//! - [`volume`]: microSD cards and USB drives, known by their filesystem's ID.

pub mod dirs;
pub mod format;
pub mod language;
pub mod playback;
pub mod shuffle;
pub mod source;
pub mod sync;
pub mod view;
pub mod volume;

pub use format::{DetectedFormat, Projection, StereoLayout, VideoFormat};
pub use playback::{PlaybackStatus, PlayerCommand, SubtitleTrack};
pub use source::{ByteSource, Entry, EntryKind};
pub use view::ViewSettings;
