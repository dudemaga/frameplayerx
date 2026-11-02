//! Rows of the library as Rust values.

use std::borrow::Cow;
use std::fmt;
use std::path::PathBuf;

use fp_core::format::{DetectedFormat, Evidence};
use fp_core::view::Keyframes;
use fp_core::{Projection, StereoLayout, VideoFormat, ViewSettings};
use rusqlite::Row;
use rusqlite::types::Type;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::library::WATCHED_FRACTION;

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// Primary key of a media row. Stable for the life of the database.
    MediaId
);
id_type!(
    /// Primary key of a playlist.
    PlaylistId
);
id_type!(
    /// Primary key of a marker (bookmark).
    MarkerId
);
id_type!(
    /// One playback session in the watch history, returned by
    /// [`Library::start_playback`](crate::Library::start_playback).
    SessionId
);

/// Coarse projection family, used for filtering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionKind {
    /// Flat screen.
    Flat,
    /// Equirectangular up to 180° wide.
    Equirect180,
    /// Equirectangular wider than 180° (normally 360°).
    Equirect360,
    /// Any fisheye.
    Fisheye,
    /// Equi-angular cubemap.
    Eac,
}

impl ProjectionKind {
    /// Family of a concrete projection.
    pub fn of(projection: &Projection) -> ProjectionKind {
        match *projection {
            Projection::Flat => ProjectionKind::Flat,
            Projection::Equirect { h_fov, .. } if h_fov <= 180.0 => ProjectionKind::Equirect180,
            Projection::Equirect { .. } => ProjectionKind::Equirect360,
            Projection::Fisheye { .. } => ProjectionKind::Fisheye,
            Projection::Eac { .. } => ProjectionKind::Eac,
        }
    }

    /// Value stored in the `projection_kind` column.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ProjectionKind::Flat => "flat",
            ProjectionKind::Equirect180 => "equirect180",
            ProjectionKind::Equirect360 => "equirect360",
            ProjectionKind::Fisheye => "fisheye",
            ProjectionKind::Eac => "eac",
        }
    }
}

/// Value stored in the `stereo` column.
pub(crate) fn stereo_str(stereo: StereoLayout) -> &'static str {
    match stereo {
        StereoLayout::Mono => "mono",
        StereoLayout::SideBySide => "side_by_side",
        StereoLayout::HalfSideBySide => "half_side_by_side",
        StereoLayout::TopBottom => "top_bottom",
        StereoLayout::HalfTopBottom => "half_top_bottom",
    }
}

pub(crate) fn evidence_str(evidence: Evidence) -> &'static str {
    match evidence {
        Evidence::User => "user",
        Evidence::Metadata => "metadata",
        Evidence::FileName => "file_name",
        Evidence::AspectRatio => "aspect_ratio",
        Evidence::Default => "default",
    }
}

fn parse_evidence(s: &str) -> Evidence {
    match s {
        "user" => Evidence::User,
        "metadata" => Evidence::Metadata,
        "file_name" => Evidence::FileName,
        "aspect_ratio" => Evidence::AspectRatio,
        _ => Evidence::Default,
    }
}

/// `(projection_kind, stereo)` column values for the effective format.
pub(crate) fn effective_columns(
    detected: &VideoFormat,
    user: Option<&VideoFormat>,
) -> (&'static str, &'static str) {
    let f = user.unwrap_or(detected);
    (
        ProjectionKind::of(&f.projection).as_str(),
        stereo_str(f.stereo),
    )
}

/// One video in the library, mirroring a `media` row plus its tags.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaRecord {
    pub id: MediaId,
    /// Stable media location (a path, source-owned URI or legacy URL). Unique.
    pub location: String,
    /// Which configured source produced this row (`"local"`, a share id...).
    pub source_id: String,
    /// Display title (file stem unless the source gave one).
    pub title: String,
    /// Bytes, when known.
    pub size: Option<u64>,
    /// Modification time, Unix seconds, when known.
    pub mtime: Option<i64>,
    /// Seconds, once probed or declared by the source.
    pub duration: Option<f64>,
    /// Frame width in pixels, once probed.
    pub width: Option<u32>,
    /// Frame height in pixels, once probed.
    pub height: Option<u32>,
    /// Video codec name reported by the prober (`"hevc"`, `"av1"`...).
    pub video_codec: Option<String>,
    /// Format detected from metadata, file name or aspect ratio.
    pub detected: DetectedFormat,
    /// Format the user forced for this file.
    pub user_format: Option<VideoFormat>,
    /// Per-video picture corrections, when the user changed any.
    pub view_settings: Option<ViewSettings>,
    /// Settings that change over the timeline.
    pub keyframes: Keyframes,
    /// 0 (unrated) to 5.
    pub rating: u8,
    pub favorite: bool,
    /// Times watched to the end (or past 90 %).
    pub play_count: u32,
    /// Unix seconds of the last playback.
    pub last_played: Option<i64>,
    /// Seconds to resume from; 0 when not started or watched.
    pub resume_position: f64,
    /// Unix seconds when first indexed.
    pub added_at: i64,
    /// The file was not found by the last scan of its folder.
    pub missing: bool,
    /// Generated thumbnail JPEG, for videos whose source offers none.
    pub thumbnail: Option<PathBuf>,
    /// The frame where it was left off, made by the metadata worker after
    /// playback.
    pub resume_thumbnail: Option<PathBuf>,
    /// Picture offered by its server (Jellyfin, DLNA, a DeoVR feed).
    pub thumbnail_url: Option<String>,
    /// The metadata worker has processed this row.
    pub probed: bool,
    /// Why the metadata worker failed on this row, if it did.
    pub probe_error: Option<String>,
    /// Haptic scripts: the main `stem.funscript` first, then axis scripts.
    pub scripts: Vec<String>,
    pub subtitles: Vec<String>,
    /// Tag names, sorted case-insensitively.
    pub tags: Vec<String>,
    /// What its source says about it (Jellyfin's overview, a scene's
    /// description).
    pub description: Option<String>,
    /// The audio and subtitles picked while it played.
    pub tracks: PickedTracks,
}

/// The audio and subtitles picked for a video, which it plays with again
/// when it opens (while they are still there). What wasn't picked follows
/// the settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickedTracks {
    /// The audio stream, by index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitles: Option<PickedSubtitles>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickedSubtitles {
    Off,
    /// An embedded stream, by index.
    Stream(usize),
    /// A subtitle file, by location.
    File(String),
}

impl MediaRecord {
    /// The format to play with: the user's override when set, else the
    /// detected one (the precedence of [`fp_core::format::resolve`]).
    pub fn effective_format(&self) -> DetectedFormat {
        match self.user_format {
            Some(format) => DetectedFormat {
                format,
                evidence: Evidence::User,
            },
            None => self.detected,
        }
    }

    /// Last path segment of the location.
    pub fn file_name(&self) -> Cow<'_, str> {
        file_name_of(&self.location)
    }

    /// Fraction watched (0..=1) from the resume position, when the duration
    /// is known.
    pub fn progress(&self) -> Option<f64> {
        self.duration
            .filter(|d| *d > 0.0)
            .map(|d| (self.resume_position / d).clamp(0.0, 1.0))
    }

    /// Started and not yet watched, as
    /// [`Library::continue_watching`](crate::Library::continue_watching)
    /// lists it.
    pub fn partly_watched(&self) -> bool {
        self.resume_position > 0.0 && self.progress().is_none_or(|p| p < WATCHED_FRACTION)
    }

    /// What a card shows, as a path or URL: the frame where it was left off
    /// while partly watched, else its server's picture, else the thumbnail
    /// made for it.
    pub fn picture(&self) -> Option<String> {
        self.resume_thumbnail
            .as_ref()
            .filter(|_| self.partly_watched())
            .map(|p| p.display().to_string())
            .or_else(|| self.thumbnail_url.clone())
            .or_else(|| self.thumbnail.as_ref().map(|p| p.display().to_string()))
    }
}

/// Last segment of a path or URL, without a query string.
pub(crate) fn file_name_of(location: &str) -> Cow<'_, str> {
    // Its location ends in the server's ID, and an ID of 180 is no format hint.
    if location.starts_with("plex://") {
        return Cow::Borrowed("");
    }
    if let Some(rest) = location.strip_prefix("source://source/") {
        let mut parts = rest.splitn(3, '/');
        if let (Some(source), Some(kind), Some(key)) = (parts.next(), parts.next(), parts.next())
            && !source.is_empty()
            && !kind.is_empty()
            && !key.contains(['/', '?', '#'])
            && let Ok(key) = percent_encoding::percent_decode_str(key).decode_utf8()
        {
            if fp_core::source::location_names_no_file(kind) {
                return Cow::Borrowed("");
            }
            // The outer URI stores the provider key as one opaque segment.
            // Extract its basename before decoding the original URL escapes,
            // so folders and query values cannot masquerade as format hints.
            let name = path_file_name(&key);
            return Cow::Owned(
                percent_encoding::percent_decode_str(name)
                    .decode_utf8_lossy()
                    .into_owned(),
            );
        }
    }
    Cow::Borrowed(path_file_name(location))
}

fn path_file_name(location: &str) -> &str {
    let no_query = location.split(['?', '#']).next().unwrap_or(location);
    let trimmed = no_query.trim_end_matches(['/', '\\']);
    trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed)
}

/// File name without its extension.
pub(crate) fn stem_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// A timestamped bookmark inside a video.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub id: MarkerId,
    pub media_id: MediaId,
    /// Seconds from the start.
    pub time: f64,
    pub name: String,
    /// Provided by the source (chapters); replaced on re-index, never
    /// exported. User markers have this `false`.
    pub from_source: bool,
}

/// One playback session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: SessionId,
    pub media_id: MediaId,
    /// Unix seconds when playback started.
    pub started_at: i64,
    /// Unix seconds of the last progress report.
    pub updated_at: i64,
    /// Last reported position, seconds.
    pub position: f64,
    /// The session counted as a full watch.
    pub completed: bool,
}

/// A tag and how many videos carry it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagCount {
    /// Tag name as first written.
    pub name: String,
    /// Number of videos.
    pub count: usize,
}

/// Columns read by [`media_from_row`], for a `media` table aliased `m`.
pub(crate) const MEDIA_COLUMNS: &str = "m.id, m.location, m.source_id, m.title, m.size, m.mtime,
    m.duration, m.width, m.height, m.video_codec, m.detected_format, m.detected_evidence,
    m.user_format, m.view_settings, m.keyframes, m.rating, m.favorite, m.play_count,
    m.last_played, m.resume_position, m.added_at, m.missing, m.thumbnail, m.resume_thumbnail,
    m.thumbnail_url, m.probed, m.probe_error, m.scripts, m.subtitles, m.description, m.tracks,
    (SELECT json_group_array(t.name ORDER BY t.name COLLATE NOCASE)
       FROM media_tags mt JOIN tags t ON t.id = mt.tag_id WHERE mt.media_id = m.id)";

fn json_col<T: DeserializeOwned>(row: &Row<'_>, idx: usize) -> rusqlite::Result<T> {
    let text: String = row.get(idx)?;
    serde_json::from_str(&text)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e)))
}

fn json_opt<T: DeserializeOwned>(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<T>> {
    match row.get::<_, Option<String>>(idx)? {
        None => Ok(None),
        Some(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e))),
    }
}

/// The format the viewer chose. One saved before masks existed (no
/// `alpha_packed`) takes the mask from the file name, as detection does, so
/// an `_alpha` video uses its mask; one saved since keeps its own choice.
fn user_format_col(
    row: &Row<'_>,
    idx: usize,
    location: &str,
) -> rusqlite::Result<Option<VideoFormat>> {
    let Some(text) = row.get::<_, Option<String>>(idx)? else {
        return Ok(None);
    };
    let mut f: VideoFormat = serde_json::from_str(&text)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e)))?;
    if !text.contains("alpha_packed") {
        f.alpha_packed = fp_core::format::detect_from_name(&file_name_of(location))
            .is_some_and(|d| d.alpha_packed);
    }
    Ok(Some(f))
}

fn u32_opt(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<u32>> {
    Ok(row
        .get::<_, Option<i64>>(idx)?
        .map(|v| v.clamp(0, u32::MAX as i64) as u32))
}

/// Builds a record from a row selected with [`MEDIA_COLUMNS`].
pub(crate) fn media_from_row(row: &Row<'_>) -> rusqlite::Result<MediaRecord> {
    let evidence: String = row.get(11)?;
    let location: String = row.get(1)?;
    Ok(MediaRecord {
        id: MediaId(row.get(0)?),
        user_format: user_format_col(row, 12, &location)?,
        location,
        source_id: row.get(2)?,
        title: row.get(3)?,
        size: row.get::<_, Option<i64>>(4)?.map(|v| v.max(0) as u64),
        mtime: row.get(5)?,
        duration: row.get(6)?,
        width: u32_opt(row, 7)?,
        height: u32_opt(row, 8)?,
        video_codec: row.get(9)?,
        detected: DetectedFormat {
            format: json_col(row, 10)?,
            evidence: parse_evidence(&evidence),
        },
        view_settings: json_opt(row, 13)?,
        keyframes: json_col(row, 14)?,
        rating: row.get::<_, i64>(15)?.clamp(0, 5) as u8,
        favorite: row.get(16)?,
        play_count: row.get::<_, i64>(17)?.clamp(0, u32::MAX as i64) as u32,
        last_played: row.get(18)?,
        resume_position: row.get(19)?,
        added_at: row.get(20)?,
        missing: row.get(21)?,
        thumbnail: row.get::<_, Option<String>>(22)?.map(PathBuf::from),
        resume_thumbnail: row.get::<_, Option<String>>(23)?.map(PathBuf::from),
        thumbnail_url: row.get(24)?,
        probed: row.get(25)?,
        probe_error: row.get(26)?,
        scripts: json_col(row, 27)?,
        subtitles: json_col(row, 28)?,
        description: row.get(29)?,
        tracks: json_opt(row, 30)?.unwrap_or_default(),
        tags: json_col(row, 31)?,
    })
}

/// Current time in Unix seconds.
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_kinds() {
        assert_eq!(
            ProjectionKind::of(&Projection::EQUIRECT_180),
            ProjectionKind::Equirect180
        );
        assert_eq!(
            ProjectionKind::of(&Projection::EQUIRECT_360),
            ProjectionKind::Equirect360
        );
        assert_eq!(
            ProjectionKind::of(&Projection::fisheye(200.0)),
            ProjectionKind::Fisheye
        );
        assert_eq!(
            ProjectionKind::of(&Projection::Eac { h_fov: 360.0 }),
            ProjectionKind::Eac
        );
        assert_eq!(ProjectionKind::of(&Projection::Flat), ProjectionKind::Flat);
    }

    #[test]
    fn evidence_round_trips() {
        for e in [
            Evidence::User,
            Evidence::Metadata,
            Evidence::FileName,
            Evidence::AspectRatio,
            Evidence::Default,
        ] {
            assert_eq!(parse_evidence(evidence_str(e)), e);
        }
    }

    #[test]
    fn names() {
        assert_eq!(file_name_of("/a/b/c.mp4"), "c.mp4");
        assert_eq!(file_name_of("plex://source/s/m/item/180?media=1"), "");
        assert_eq!(file_name_of("source://source/s/deovr-id/180"), "");
        assert_eq!(
            file_name_of("source://source/s/dlna/item%3A64%241%24180"),
            ""
        );
        assert_eq!(file_name_of("http://h/x/y.mp4?token=1"), "y.mp4");
        assert_eq!(file_name_of("smb://h/share/dir/"), "dir");
        assert_eq!(stem_of("a.b.mp4"), "a.b");
        assert_eq!(stem_of(".hidden"), ".hidden");
    }

    #[test]
    fn source_locations_extract_the_provider_basename_before_decoding_url_escapes() {
        let source = |key: &str| {
            format!(
                "source://source/server/http/{}",
                percent_encoding::utf8_percent_encode(key, percent_encoding::NON_ALPHANUMERIC)
            )
        };
        assert_eq!(
            file_name_of(&source("VR_180_LR/Flat.mp4?projection=360_TB")),
            "Flat.mp4"
        );
        assert_eq!(
            file_name_of(&source("films/Caf%C3%A9%20%23%3F_180_LR.mkv?token=ignored")),
            "Café #?_180_LR.mkv"
        );
        assert_eq!(
            file_name_of(&source("films/Literal%2520name.mkv")),
            "Literal%20name.mkv",
            "URL filename escapes are decoded once, not recursively"
        );
        assert_eq!(
            file_name_of(&source("films/休暇_180_LR.mkv")),
            "休暇_180_LR.mkv"
        );
    }

    #[test]
    fn legacy_filenames_remain_borrowed_and_unchanged() {
        for location in [
            "/v/a%20b.mp4",
            "http://host/VR_180_LR/a%20b.mp4?projection=360_TB",
            "smb://nas/share/a%20b.mp4",
        ] {
            assert!(matches!(file_name_of(location), Cow::Borrowed("a%20b.mp4")));
        }
    }
}
