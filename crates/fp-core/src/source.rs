//! The contract between media sources (local, SMB, WebDAV, HTTP, DLNA,
//! DeoVR feeds) and the rest of FramePlayerX.

use serde::{Deserialize, Serialize};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Random-access read of a remote or local file. The media pipeline wraps
/// this in an FFmpeg I/O context, so FFmpeg never touches the network.
///
/// Implementations must be safe to call from the demux thread while another
/// thread holds the same `Arc`.
pub trait ByteSource: Send + Sync {
    /// Total size in bytes, when known. Seeking needs it for most containers.
    fn size(&self) -> Option<u64>;
    /// Reads up to `buf.len()` bytes at `offset`. Returns 0 at end of file.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    /// Human-readable description for logs and errors.
    fn describe(&self) -> String;
    /// Read front to back only, as it arrives (a server transcoding live):
    /// demuxers must not seek in it. Told it could, FFmpeg's MP4 reader
    /// reads a fragmented stream to its end first, which a live one never
    /// reaches.
    fn streamed(&self) -> bool {
        false
    }
    /// Read over a network, whose speed can drop at any moment, so a player
    /// should keep more in hand. A live transcode isn't: it arrives no faster
    /// than the server encodes.
    fn remote(&self) -> bool {
        false
    }
    /// How reading it over the network is going; `None` when it isn't read
    /// through the block cache.
    fn net(&self) -> Option<Arc<NetCounters>> {
        None
    }
}

/// Running totals of a network file's reads, counted as they happen.
#[derive(Debug, Default)]
pub struct NetCounters {
    /// Bytes received.
    pub bytes: AtomicU64,
    /// Requests that failed, and SMB connections made again.
    pub retries: AtomicU64,
    awaiting_first_read: AtomicBool,
    first_read_ms: AtomicU64,
}

impl NetCounters {
    /// A skip is starting: the next read at a new place is its first.
    pub fn expect_first_read(&self) {
        self.first_read_ms.store(0, Ordering::Relaxed);
        self.awaiting_first_read.store(true, Ordering::Relaxed);
    }

    /// Whether a read at a new place is the first one being waited for.
    pub fn take_first_read(&self) -> bool {
        self.awaiting_first_read.swap(false, Ordering::Relaxed)
    }

    pub fn set_first_read(&self, took: Duration) {
        // 0 means not known, so the quickest read counts as 1 ms.
        let ms = (took.as_millis() as u64).max(1);
        self.first_read_ms.store(ms, Ordering::Relaxed);
    }

    /// How long the last skip's first read took, once it has finished.
    pub fn first_read(&self) -> Option<Duration> {
        match self.first_read_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        }
    }
}

/// A `ByteSource` over a local file.
pub struct FileSource {
    file: std::fs::File,
    size: u64,
    path: String,
}

impl FileSource {
    pub fn open(path: &std::path::Path) -> io::Result<FileSource> {
        let file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        Ok(FileSource {
            file,
            size,
            path: path.display().to_string(),
        })
    }
}

impl ByteSource for FileSource {
    fn size(&self) -> Option<u64> {
        Some(self.size)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_at(buf, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            self.file.seek_read(buf, offset)
        }
    }
    fn describe(&self) -> String {
        self.path.clone()
    }
}

/// An in-memory `ByteSource`, for tests.
pub struct MemorySource(pub Vec<u8>);

impl ByteSource for MemorySource {
    fn size(&self) -> Option<u64> {
        Some(self.0.len() as u64)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let start = (offset as usize).min(self.0.len());
        let n = buf.len().min(self.0.len() - start);
        buf[..n].copy_from_slice(&self.0[start..start + n]);
        Ok(n)
    }
    fn describe(&self) -> String {
        format!("memory ({} bytes)", self.0.len())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Directory,
    Video,
    /// Haptic script (`.funscript`), subtitle or other side file.
    Other,
}

/// One item when browsing a source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    /// Location understood by the source that produced it: a path for local
    /// files, a URL otherwise (`smb://`, `http(s)://`, `webdav(s)://`, ...).
    pub location: String,
    pub kind: EntryKind,
    #[serde(default)]
    pub size: Option<u64>,
    /// Unix seconds.
    #[serde(default)]
    pub modified: Option<i64>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    /// Format the source declares (DeoVR feeds, DLNA metadata).
    #[serde(default)]
    pub format: Option<crate::format::VideoFormat>,
    /// Haptic script locations that belong to this video.
    #[serde(default)]
    pub scripts: Vec<String>,
    /// Subtitle files that belong to this video.
    #[serde(default)]
    pub subtitles: Vec<SubtitleFile>,
    /// Named timestamps (chapters) the source provides.
    #[serde(default)]
    pub markers: Vec<(f64, String)>,
    /// Where the server says watching stopped, in seconds: Jellyfin and
    /// Emby keep it per user (0 when not started or watched to the end).
    #[serde(default)]
    pub resume: Option<f64>,
    /// What the source says about it: Jellyfin's overview, a scene's
    /// description.
    #[serde(default)]
    pub description: Option<String>,
    /// The source's tags for it.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A subtitle file belonging to a video.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleFile {
    /// File name or title.
    pub name: String,
    /// Location to open with the same source (or a URL for feeds).
    pub location: String,
    /// Language tag from the name or the source (`en`, `de.forced`); see
    /// [`crate::language::Tag`].
    #[serde(default)]
    pub language: Option<String>,
}

impl Entry {
    pub fn new(name: impl Into<String>, location: impl Into<String>, kind: EntryKind) -> Entry {
        Entry {
            name: name.into(),
            location: location.into(),
            kind,
            size: None,
            modified: None,
            duration: None,
            thumbnail_url: None,
            format: None,
            scripts: Vec::new(),
            subtitles: Vec::new(),
            markers: Vec::new(),
            resume: None,
            description: None,
            tags: Vec::new(),
        }
    }
}

/// File extensions treated as video.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "mov", "avi", "ts", "m2ts", "mts", "ssif", "iso", "flv", "wmv",
    "mpg", "mpeg", "ogv",
];

/// File extensions treated as subtitles.
pub const SUBTITLE_EXTENSIONS: &[&str] = &["srt", "ass", "ssa", "vtt"];

/// Whether a source location ends in a server's ID for the video rather than a
/// file name: an ID of 180 is no format hint.
pub fn location_names_no_file(location: &str) -> bool {
    if let Some(rest) = location.strip_prefix("plex://source/") {
        return rest.split('/').nth(2) == Some("item");
    }
    location
        .strip_prefix("source://source/")
        .and_then(|rest| rest.split('/').nth(1))
        .is_some_and(|kind| {
            matches!(kind, "dlna" | "mb") || kind.ends_with("-id") || kind.ends_with("-group")
        })
}

pub fn is_video_name(name: &str) -> bool {
    ext_in(name, VIDEO_EXTENSIONS)
}

pub fn is_subtitle_name(name: &str) -> bool {
    ext_in(name, SUBTITLE_EXTENSIONS)
}

pub fn is_script_name(name: &str) -> bool {
    ext_in(name, &["funscript"])
}

fn ext_in(name: &str, list: &[&str]) -> bool {
    name.rsplit_once('.')
        .map(|(_, e)| list.iter().any(|x| x.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_source_reads() {
        let s = MemorySource(b"hello world".to_vec());
        let mut b = [0u8; 5];
        assert_eq!(s.read_at(6, &mut b).unwrap(), 5);
        assert_eq!(&b, b"world");
        assert_eq!(s.read_at(11, &mut b).unwrap(), 0);
        assert_eq!(s.read_at(99, &mut b).unwrap(), 0);
    }

    #[test]
    fn a_skip_times_only_its_first_read() {
        let net = NetCounters::default();
        assert!(!net.take_first_read(), "nothing to time until a skip");
        net.set_first_read(Duration::from_millis(40));
        net.expect_first_read();
        assert_eq!(net.first_read(), None, "the last skip's is gone");
        assert!(net.take_first_read());
        assert!(!net.take_first_read(), "only the first");
        net.set_first_read(Duration::ZERO);
        assert_eq!(net.first_read(), Some(Duration::from_millis(1)));
    }

    #[test]
    fn classifies_names() {
        assert!(is_video_name("a.MP4"));
        assert!(is_video_name("x.y.mkv"));
        assert!(!is_video_name("a.funscript"));
        assert!(is_script_name("a.funscript"));
        assert!(is_subtitle_name("a.en.srt"));
        assert!(!is_video_name("mp4"));
    }

    #[test]
    fn a_servers_id_names_no_file_but_a_path_does() {
        for location in [
            "plex://source/s/m/item/180",
            "plex://source/s/m/item/180?media=1",
            "source://source/s/deovr-id/180",
            "source://source/s/heresphere-id/360",
            "source://source/s/dlna/item%3A64%241%24180",
            "source://source/s/mb/item%3Dabc",
        ] {
            assert!(location_names_no_file(location), "{location}");
        }
        for location in [
            "plex://source/s/m/image/resource?path=%2Fa.jpg",
            "source://source/s/http/VR%2FClip_180_LR.mp4",
            "source://source/s/dlna-file/x",
            "http://h/item/180.mp4",
            "/videos/180.mp4",
        ] {
            assert!(!location_names_no_file(location), "{location}");
        }
    }
}
