//! FramePlayerX's media library: a SQLite index of videos from every source.
//!
//! - [`Library`]: the database handle (cheap to clone, shareable between
//!   threads). Open with [`Library::open`] or [`Library::open_in_memory`].
//!   The schema is versioned with `PRAGMA user_version` and migrated forward
//!   on open; the file uses WAL journaling.
//! - Indexing: [`Library::scan_folder`] for local folders (incremental,
//!   side-file aware, marks vanished files missing) and
//!   [`Library::upsert_entry`] for entries from remote sources. Neither ever
//!   overwrites user data.
//! - Reading: [`Library::search`] with a [`Query`] (forgiving ranked free
//!   text plus filters and sorting), [`Library::continue_watching`],
//!   [`Library::recently_added`], [`Library::get`].
//! - User data: ratings, favourites, tags, markers, format overrides, view
//!   settings, keyframes, playback progress and history, playlists.
//! - [`MetadataWorker`]: a background thread that fills in duration, size,
//!   codec and format via an app-provided [`MediaProber`] and writes JPEG
//!   thumbnails for videos whose server offers no picture, and the frame
//!   where each partly watched video was left off.

mod error;
mod image;
mod library;
mod locations;
mod playlist;
mod query;
mod record;
mod scan;
mod schema;
mod worker;

pub use error::{Error, Result};
pub use image::RgbaImage;
pub use library::{
    Hidden, Library, MediaUpsert, PlaybackUpdate, SourceDetails, UpsertOutcome, UpsertStatus,
    WATCHED_FRACTION, default_db_path,
};
pub use playlist::{Playlist, PlaylistEntry};
pub use query::{Query, Sort, SortDirection, StereoFilter};
pub use record::{
    HistoryEntry, Marker, MarkerId, MediaId, MediaRecord, PickedSubtitles, PickedTracks,
    PlaylistId, ProjectionKind, SessionId, TagCount,
};
pub use scan::{
    SCRIPT_AXES, ScanError, ScanOptions, ScanPhase, ScanProgress, ScanReport, script_axis,
};
pub use schema::SCHEMA_VERSION;
pub use worker::{
    MediaProber, MetadataWorker, ProbeInfo, ThumbnailCrop, WorkerEvent, WorkerOptions, WorkerStats,
    process_pending,
};
