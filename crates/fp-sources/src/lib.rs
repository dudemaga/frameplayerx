//! Media sources for FramePlayerX: browsing and random-access reading of
//! local and network media.
//!
//! Every source implements [`Source`]: it lists folders as
//! [`fp_core::Entry`] values and opens files as [`fp_core::ByteSource`]s.
//! The media pipeline wraps a `ByteSource` in an FFmpeg I/O context, so
//! FFmpeg never does network I/O itself; network readers here are tuned for
//! demuxing (block cache, read-ahead, connection reuse, retries).
//!
//! | Source | Module | Locations |
//! |---|---|---|
//! | Local folder | [`local`] | absolute paths |
//! | HTTP(S) autoindex | [`httpdir`] | `http(s)://` URLs |
//! | WebDAV | [`webdav`] | `http(s)://` URLs |
//! | DLNA/UPnP | [`dlna`] | `dlna:<object id>` folders, `http://` media |
//! | DeoVR / HereSphere feeds | [`feed`] | feed and scene URLs |
//! | SMB2/3 | [`smb`] | `smb://host/share/path` |
//! | Jellyfin / Emby | [`mediabrowser`] | `mb:<url>#<kind>=<id>` |
//! | Plex | [`plex`] | `plex://source/<source>/<machine>/<kind>/<id>` |
//!
//! Sources are built from a serializable [`SourceConfig`] with [`build`];
//! [`save_configs`] / [`load_configs`] persist them (credentials included)
//! in a 0600 JSON file. Blocking I/O and plain threads only.

pub mod cache;
pub mod catalog;
pub mod config;
pub mod dlna;
pub mod error;
pub mod feed;
pub mod http;
pub mod httpdir;
mod json;
pub mod local;
pub mod location;
pub mod mediabrowser;
pub mod plex;
pub mod sidecar;
pub mod smb;
#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod test_server;
pub mod timeutil;
pub mod transcode;
pub mod urlutil;
pub mod watch;
pub mod webdav;
mod xml;

pub use cache::{CacheOptions, CacheStats};
pub use catalog::{
    Catalog, CatalogCapabilities, CatalogFacets, CatalogFilter, CatalogHome, CatalogItem,
    CatalogKind, CatalogPage, CatalogQuery, CatalogSort, CatalogVersion,
};
pub use config::{
    CardRef, Credentials, DlnaConfig, FeedConfig, HttpConfig, LibraryConfig, LocalConfig,
    MediaBrowserConfig, PlexConfig, SmbConfig, SourceConfig, SourceKind, default_config_path,
    load_configs, save_configs,
};
pub use dlna::{DlnaDevice, DlnaSource, SsdpResponse, discover};
pub use error::{Error, Result};
pub use feed::{FeedLibrary, FeedScene, FeedSource, MediaSource, SceneFeed, SceneInfo};
pub use http::{HttpClient, HttpFile, HttpOptions};
pub use httpdir::HttpDirSource;
pub use local::LocalSource;
pub use mediabrowser::{
    MbFacets, MbFilter, MbHome, MbItem, MbKind, MbPage, MbQuery, MbSort, MbVersion,
    MediaBrowserSource, QuickConnectState, quick_connect_initiate_url, quick_connect_poll_url,
};
pub use sidecar::{ScriptFile, Sidecars, SubtitleFile, match_sidecars};
pub use smb::SmbSource;
pub use transcode::TranscodeTarget;
pub use watch::{PlaybackTrack, WatchEvent, WatchKind, WatchSession};
pub use webdav::WebDavSource;

use fp_core::source::{ByteSource, Entry, EntryKind, is_video_name};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A place media can be browsed and read from.
///
/// Locations are opaque strings produced by the source itself (in
/// [`Entry::location`]); pass them back unchanged. Implementations block and
/// may be called from any thread.
pub trait Source: Send + Sync {
    /// Stable identifier (from the configuration).
    fn id(&self) -> &str;

    fn name(&self) -> &str;

    /// What kind of source this is.
    fn kind(&self) -> SourceKind;

    /// One-line description for logs and the UI, without secrets.
    fn describe(&self) -> String;

    /// Converts a location owned by this source to its persistent identity.
    /// This performs no I/O. Unknown or external locations are left unchanged.
    fn canonical_location(&self, _location: &str) -> Option<String> {
        None
    }

    /// Lists a folder. `None` lists the root of the source.
    fn list(&self, location: Option<&str>) -> Result<Vec<Entry>>;

    /// Opens a file for random access.
    fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>>;

    /// Opens media using the same session as its playback reports.
    fn open_playback(
        &self,
        location: &str,
        _session: &WatchSession,
    ) -> Result<Arc<dyn ByteSource>> {
        self.open(location)
    }

    /// A refreshed configuration to persist; taking it performs no network I/O.
    fn updated_config(&self) -> Option<SourceConfig> {
        None
    }

    /// A picture of this source as an HTTP request: the client allowed to ask
    /// for it, and its address. The image cache uses it to ask whether the
    /// picture changed, where opening it could only download it again. `None`
    /// for one that isn't over HTTP.
    fn picture_request(&self, _location: &str) -> Option<(HttpClient, String)> {
        None
    }

    /// Folder containing `location`, when the source can list it.
    fn parent(&self, _location: &str) -> Option<String> {
        None
    }

    /// Completes an entry with details that are too expensive to fetch for a
    /// whole listing (feeds: format, scripts, subtitles, markers). The default
    /// converts recognized locations to this source's persistent identities.
    fn details(&self, entry: &Entry) -> Result<Entry> {
        let mut entry = entry.clone();
        if let Some(location) = self.canonical_location(&entry.location) {
            entry.location = location;
        }
        if let Some(url) = entry.thumbnail_url.as_mut()
            && let Some(location) = self.canonical_location(url)
        {
            *url = location;
        }
        for location in entry
            .scripts
            .iter_mut()
            .chain(entry.subtitles.iter_mut().map(|s| &mut s.location))
        {
            if let Some(stable) = self.canonical_location(location) {
                *location = stable;
            }
        }
        Ok(entry)
    }

    /// Finds the haptic scripts (`stem.funscript`, `stem.<axis>.funscript`)
    /// and subtitles (`stem.srt`, `stem.<lang>.srt`, ...) belonging to a
    /// video: those the entry already carries, plus matching files in the
    /// same folder when the source can list it.
    fn sidecars(&self, video: &Entry) -> Result<Sidecars> {
        sidecars_from_folder(self, video)
    }

    /// Starts telling the server how watching `video` goes, when it keeps
    /// track (see [`watch`]). May ask the server, so not on the frame loop.
    fn watch_session(&self, _video: &Entry) -> Option<WatchSession> {
        None
    }

    /// Original track choices when an encoding exposes only its selected tracks.
    fn playback_tracks(&self, _video: &Entry) -> Result<Vec<PlaybackTrack>> {
        Ok(Vec::new())
    }

    /// Tells the server one change in a [`Source::watch_session`].
    fn report_watch(&self, _session: &WatchSession, _event: WatchEvent) -> Result<()> {
        Ok(())
    }

    /// What the server can transcode `video` to, tallest first: for when
    /// the file itself can't play, or not smoothly. Nothing is encoded
    /// until one is opened. May ask the server, so not on the frame loop.
    fn transcode_targets(&self, _video: &Entry) -> Vec<TranscodeTarget> {
        Vec::new()
    }

    /// Asks the server to transcode `location` to target `id` (from
    /// [`Source::transcode_targets`]) from `start` seconds on, and returns
    /// the stream as it encodes; one that isn't seekable starts at the
    /// beginning whatever `start` is. The server encodes one at a time for
    /// us: asking for another stops the one before.
    fn open_transcode(&self, location: &str, id: &str, _start: f64) -> Result<Arc<dyn ByteSource>> {
        Err(Error::NotFound(format!("transcode {id} of {location}")))
    }

    /// Restarts an encoding within the video's existing playback session.
    fn open_transcode_session(
        &self,
        location: &str,
        id: &str,
        start: f64,
        _session: &WatchSession,
    ) -> Result<Arc<dyn ByteSource>> {
        self.open_transcode(location, id, start)
    }

    /// This source as a DeoVR feed or HereSphere API, when it is one.
    fn as_scene_feed(&self) -> Option<&dyn SceneFeed> {
        None
    }

    /// This source as a Jellyfin/Emby server, when it is one.
    fn as_media_browser(&self) -> Option<&MediaBrowserSource> {
        None
    }

    /// This source's rich media catalog, when it has one.
    fn as_catalog(&self) -> Option<&dyn Catalog> {
        None
    }
}

/// Whether a location binds its resource to one configured source.
pub fn is_source_location(location: &str) -> bool {
    location.split_once(':').is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("source") || scheme.eq_ignore_ascii_case("plex")
    })
}

/// The exact owner of a persistent source location, including Plex items.
pub fn location_owner(location: &str) -> Option<String> {
    location::owner(location).or_else(|| plex::owner(location))
}

/// Default [`Source::sidecars`]: the entry's own scripts and subtitles plus
/// matching files listed in [`Source::parent`] of the video.
pub fn sidecars_from_folder<S: Source + ?Sized>(source: &S, video: &Entry) -> Result<Sidecars> {
    let mut out = Sidecars::from_entry(video);
    if let Some(parent) = source.parent(&video.location) {
        let siblings = source.list(Some(&parent))?;
        let mut file_name = urlutil::last_segment(&video.location);
        if file_name.is_empty() {
            file_name = video.name.clone();
        }
        out.merge(match_sidecars(&file_name, &siblings));
    }
    Ok(out)
}

/// Builds the source described by a configuration. No network I/O happens
/// until the source is used.
pub fn build(config: &SourceConfig) -> Result<Box<dyn Source>> {
    Ok(match config {
        SourceConfig::Local(c) => Box::new(LocalSource::new(c)),
        SourceConfig::Library(c) => Box::new(LocalSource::library(c)),
        SourceConfig::Http(c) => Box::new(HttpDirSource::new(c)?),
        SourceConfig::WebDav(c) => Box::new(WebDavSource::new(c)?),
        SourceConfig::Dlna(c) => Box::new(DlnaSource::new(c)?),
        SourceConfig::DeoVr(c) => Box::new(FeedSource::deovr(c)?),
        SourceConfig::HereSphere(c) => Box::new(FeedSource::heresphere(c)?),
        SourceConfig::Smb(c) => Box::new(SmbSource::new(c)?),
        SourceConfig::MediaBrowser(c) => Box::new(MediaBrowserSource::new(c)?),
        SourceConfig::Plex(c) => Box::new(plex::PlexSource::new(c)?),
    })
}

/// Builds the entry for a file: `Video` (with the format detected from its
/// name) when the extension is a video one, `Other` otherwise.
pub fn file_entry(
    name: impl Into<String>,
    location: impl Into<String>,
    size: Option<u64>,
    modified: Option<i64>,
) -> Entry {
    let name = name.into();
    let kind = if is_video_name(&name) {
        EntryKind::Video
    } else {
        EntryKind::Other
    };
    let mut e = Entry::new(name, location, kind);
    e.size = size;
    e.modified = modified;
    if kind == EntryKind::Video {
        e.format = fp_core::format::detect_from_name(&e.name);
    }
    e
}

/// Builds the entry for a folder.
pub fn dir_entry(
    name: impl Into<String>,
    location: impl Into<String>,
    modified: Option<i64>,
) -> Entry {
    let mut e = Entry::new(name, location, EntryKind::Directory);
    e.modified = modified;
    e
}

/// Sorts folders first, then names in natural order (`ep2` before `ep10`).
pub fn sort_entries(entries: &mut [Entry]) {
    sort_entries_by(entries, EntrySort::Name);
}

/// How a folder listing is ordered when browsing a source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntrySort {
    /// Natural name order (`ep2` before `ep10`).
    #[default]
    Name,
    /// Most recently modified first.
    Newest,
    /// Least recently modified first.
    Oldest,
    /// Biggest files first.
    Largest,
    /// Smallest files first.
    Smallest,
}

/// Sorts folders first, then by `sort`. Entries the source gave no date or
/// size for go after the rest; ties (and folders, which have no size when
/// sorting by size) fall back to natural name order.
pub fn sort_entries_by(entries: &mut [Entry], sort: EntrySort) {
    use std::cmp::Ordering;
    // Known values first, in the given direction; unknown ones last.
    fn known_first<T: Ord>(a: Option<T>, b: Option<T>, descending: bool) -> Ordering {
        match (a, b) {
            (Some(a), Some(b)) if descending => b.cmp(&a),
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    }
    entries.sort_by(|a, b| {
        let da = a.kind != EntryKind::Directory;
        let db = b.kind != EntryKind::Directory;
        let by = match sort {
            EntrySort::Name => Ordering::Equal,
            EntrySort::Newest => known_first(a.modified, b.modified, true),
            EntrySort::Oldest => known_first(a.modified, b.modified, false),
            EntrySort::Largest => known_first(a.size, b.size, true),
            EntrySort::Smallest => known_first(a.size, b.size, false),
        };
        da.cmp(&db)
            .then(by)
            .then_with(|| urlutil::natural_cmp(&a.name, &b.name))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::format::{Projection, StereoLayout};

    #[test]
    fn file_entries_classify_and_detect() {
        let e = file_entry("Scene_180_LR.mp4", "/v/Scene_180_LR.mp4", Some(5), Some(7));
        assert_eq!(e.kind, EntryKind::Video);
        let f = e.format.unwrap();
        assert_eq!(f.projection, Projection::EQUIRECT_180);
        assert_eq!(f.stereo, StereoLayout::SideBySide);
        assert_eq!((e.size, e.modified), (Some(5), Some(7)));
        let e = file_entry("Movie.mkv", "/v/Movie.mkv", None, None);
        assert_eq!(e.kind, EntryKind::Video);
        assert!(e.format.is_none());
        let e = file_entry("Scene_180_LR.funscript", "/v/x", None, None);
        assert_eq!(e.kind, EntryKind::Other);
        assert!(e.format.is_none());
    }

    #[test]
    fn sorting() {
        let mut v = vec![
            file_entry("b10.mp4", "x", None, None),
            dir_entry("zeta", "z", None),
            file_entry("B2.mp4", "x", None, None),
            dir_entry("Alpha", "a", None),
        ];
        sort_entries(&mut v);
        let names: Vec<_> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "zeta", "B2.mp4", "b10.mp4"]);
    }

    #[test]
    fn sorting_by_date_and_size() {
        let mut v = vec![
            file_entry("a.mp4", "x", Some(30), Some(100)),
            file_entry("b.mp4", "x", None, None),
            dir_entry("old", "o", Some(1)),
            file_entry("c.mp4", "x", Some(10), Some(300)),
            dir_entry("new", "n", Some(500)),
            file_entry("d.mp4", "x", Some(20), Some(200)),
        ];
        let order = |v: &mut Vec<Entry>, s| {
            sort_entries_by(v, s);
            v.iter().map(|e| e.name.clone()).collect::<Vec<_>>()
        };
        let newest = ["new", "old", "c.mp4", "d.mp4", "a.mp4", "b.mp4"];
        assert_eq!(order(&mut v, EntrySort::Newest), newest);
        let oldest = ["old", "new", "a.mp4", "d.mp4", "c.mp4", "b.mp4"];
        assert_eq!(order(&mut v, EntrySort::Oldest), oldest);
        // Folders have no size: they stay first, by name.
        let largest = ["new", "old", "a.mp4", "d.mp4", "c.mp4", "b.mp4"];
        assert_eq!(order(&mut v, EntrySort::Largest), largest);
        let smallest = ["new", "old", "c.mp4", "d.mp4", "a.mp4", "b.mp4"];
        assert_eq!(order(&mut v, EntrySort::Smallest), smallest);
    }
}
