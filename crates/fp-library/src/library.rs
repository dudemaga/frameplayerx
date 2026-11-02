//! The [`Library`] handle: opening, reading and changing media rows.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use fp_core::format::{DetectedFormat, Evidence, detect_from_name};
use fp_core::source::{Entry, EntryKind};
use fp_core::view::Keyframes;
use fp_core::{VideoFormat, ViewSettings};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::error::{Error, Result};
use crate::record::{
    HistoryEntry, MEDIA_COLUMNS, Marker, MarkerId, MediaId, MediaRecord, PickedTracks, SessionId,
    TagCount, effective_columns, evidence_str, file_name_of, media_from_row, now, stem_of,
};
use crate::schema;
use crate::worker::ThumbnailCrop;
use fp_core::sync::{lock, read, write};

/// Fraction of the duration after which a playback counts as a full watch
/// and the video drops out of "continue watching".
pub const WATCHED_FRACTION: f64 = 0.9;

/// SQL for [`Library::pending_metadata`], of a media table aliased `m`.
fn pending_sql(thumbnails: bool) -> String {
    let wanted = if thumbnails {
        "(m.probed = 0 OR (m.thumbnail IS NULL AND m.thumbnail_url IS NULL))"
    } else {
        "m.probed = 0"
    };
    format!(
        "m.missing = 0 AND m.probe_error IS NULL AND {wanted}{}",
        on_inserted_cards()
    )
}

/// SQL true for videos [`MediaRecord::partly_watched`] is true for, of a
/// media table aliased `m`.
fn partly_watched_sql() -> String {
    format!(
        "(m.resume_position > 0 AND (m.duration IS NULL OR m.duration <= 0
              OR m.resume_position < m.duration * {WATCHED_FRACTION}))"
    )
}

/// Handle to the media library database.
///
/// # Concurrency
///
/// A `Library` wraps one SQLite connection behind an `Arc<Mutex<_>>`.
/// Cloning is cheap and every clone shares the same connection, so the type
/// is `Send + Sync` and can be handed to the UI thread, the scanner and the
/// [`MetadataWorker`](crate::MetadataWorker) at once. Each public method takes
/// the lock for the duration of one statement or one transaction, never
/// across file-system work or prober calls (the scanner walks folders and
/// the worker decodes frames without holding it). A single connection keeps
/// in-memory databases and transactions simple; WAL mode still lets other
/// processes read the file while we write. A panic inside a locked section
/// cannot leave a half-applied transaction (rusqlite rolls back on drop), so
/// a poisoned lock is recovered rather than propagated.
#[derive(Clone)]
pub struct Library {
    conn: Arc<Mutex<Connection>>,
    /// Videos listings leave out (see [`Library::set_hidden`]); shared by
    /// every clone.
    hidden: Arc<RwLock<Hidden>>,
}

/// Videos left out of listings: those of some sources (the app hides
/// sources locked with a PIN).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hidden {
    /// Source ids.
    pub sources: Vec<String>,
}

impl Hidden {
    /// Nothing hidden.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Whether the videos of `source_id` are hidden; the same test as the
    /// SQL from `hidden_sql`.
    pub fn hides(&self, source_id: &str) -> bool {
        self.sources.iter().any(|s| s == source_id)
    }
}

/// The condition keeping only what `hidden` does not hide, for a media
/// table whose columns are prefixed `prefix` (`"m."` or `""`), with its
/// parameters; `None` when nothing is hidden, so queries are unchanged then.
pub(crate) fn hidden_sql(prefix: &str, hidden: &Hidden) -> Option<(String, Vec<Value>)> {
    if hidden.is_empty() {
        return None;
    }
    let sql = format!(
        "{prefix}source_id NOT IN ({})",
        vec!["?"; hidden.sources.len()].join(",")
    );
    Some((
        sql,
        hidden.sources.iter().cloned().map(Value::Text).collect(),
    ))
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library").finish_non_exhaustive()
    }
}

/// Default database file: `$XDG_DATA_HOME/frameplayerx/library.sqlite3`.
pub fn default_db_path() -> PathBuf {
    fp_core::dirs::data_dir().join("library.sqlite3")
}

/// What to write when indexing one video, from a folder scan or a remote
/// source. User data (rating, tags, settings...) is never part of it, so an
/// upsert cannot overwrite it.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaUpsert {
    /// Stable media location (a path, source-owned URI or legacy URL); the unique key.
    pub location: String,
    /// Source that produced the entry.
    pub source_id: String,
    pub title: String,
    /// Bytes, when known.
    pub size: Option<u64>,
    /// Modification time in Unix seconds, when known.
    pub mtime: Option<i64>,
    /// Duration declared by the source, seconds.
    pub duration: Option<f64>,
    /// Format detected so far. On re-upsert of an unchanged file a stored
    /// detection with stronger evidence (e.g. container metadata found by
    /// the worker) is kept.
    pub detected: DetectedFormat,
    /// Haptic script locations, main script first.
    pub scripts: Vec<String>,
    /// Subtitle locations.
    pub subtitles: Vec<String>,
    /// Picture offered by the source's server.
    pub thumbnail_url: Option<String>,
    /// What the source says about it; `None` keeps what was stored.
    pub description: Option<String>,
    /// The source's tags for it, replacing those stored; `None` keeps them
    /// (a scan's files have none of their own).
    pub tags: Option<Vec<String>>,
    /// Chapters from the source. `Some` replaces earlier source markers;
    /// user markers are never touched.
    pub source_markers: Option<Vec<(f64, String)>>,
    /// Treat the file as changed even when size and mtime match, so the
    /// worker probes it again.
    pub reset_metadata: bool,
}

/// What a video's source says about it, for [`Library::update_details`]:
/// each field replaces what was stored when given.
#[derive(Clone, Copy, Debug, Default)]
pub struct SourceDetails<'a> {
    pub title: Option<&'a str>,
    pub description: Option<&'a str>,
    pub thumbnail_url: Option<&'a str>,
    pub tags: Option<&'a [String]>,
}

impl MediaUpsert {
    /// An entry with the title taken from the file stem and the format
    /// detected from the file name.
    pub fn new(location: impl Into<String>, source_id: impl Into<String>) -> MediaUpsert {
        let location = location.into();
        let name = file_name_of(&location);
        MediaUpsert {
            title: stem_of(&name).to_string(),
            detected: detect_name(&name),
            location,
            source_id: source_id.into(),
            size: None,
            mtime: None,
            duration: None,
            scripts: Vec::new(),
            subtitles: Vec::new(),
            thumbnail_url: None,
            description: None,
            tags: None,
            source_markers: None,
            reset_metadata: false,
        }
    }

    /// Converts a browse entry from a remote source. A format the source
    /// declares counts as container metadata.
    pub fn from_entry(source_id: &str, entry: &Entry) -> Result<MediaUpsert> {
        if entry.kind != EntryKind::Video {
            return Err(Error::InvalidArgument(format!(
                "{} is not a video entry",
                entry.location
            )));
        }
        let mut u = MediaUpsert::new(entry.location.clone(), source_id);
        // A file name loses its extension; a title ("Dr. Strangelove") stays.
        let title = if fp_core::source::is_video_name(&entry.name) {
            stem_of(&entry.name)
        } else {
            &entry.name
        }
        .trim();
        if !title.is_empty() {
            u.title = title.to_string();
        }
        if let Some(format) = entry.format {
            u.detected = DetectedFormat {
                format,
                evidence: Evidence::Metadata,
            };
        } else if let Some(format) = detect_from_name(&entry.name) {
            u.detected = DetectedFormat {
                format,
                evidence: Evidence::FileName,
            };
        }
        u.size = entry.size;
        u.mtime = entry.modified;
        u.duration = entry.duration.filter(|d| d.is_finite() && *d > 0.0);
        // A server's picture only: a browse entry can carry the library's
        // own thumbnail file.
        u.thumbnail_url = entry.thumbnail_url.clone().filter(|t| {
            t.starts_with("http://")
                || t.starts_with("https://")
                || t.starts_with("source://")
                || t.starts_with("plex://")
        });
        u.description = entry.description.clone();
        u.tags = (!entry.tags.is_empty()).then(|| entry.tags.clone());
        u.scripts = entry.scripts.clone();
        u.subtitles = entry.subtitles.iter().map(|s| s.location.clone()).collect();
        u.source_markers = Some(entry.markers.clone());
        Ok(u)
    }
}

/// Format detected from a file name alone.
pub(crate) fn detect_name(name: &str) -> DetectedFormat {
    match detect_from_name(name) {
        Some(format) => DetectedFormat {
            format,
            evidence: Evidence::FileName,
        },
        None => DetectedFormat {
            format: VideoFormat::FALLBACK,
            evidence: Evidence::Default,
        },
    }
}

/// What an upsert did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpsertStatus {
    /// A new row was created.
    Added,
    /// An existing row changed.
    Updated,
    Unchanged,
}

/// Result of [`Library::upsert`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpsertOutcome {
    pub id: MediaId,
    /// What happened to the row.
    pub status: UpsertStatus,
    /// The row was marked missing and is back.
    pub restored: bool,
}

/// Result of [`Library::update_playback`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackUpdate {
    /// Resume position now stored.
    pub resume_position: f64,
    /// This update counted the session as a full watch (play count + 1).
    pub counted_now: bool,
    /// Play count after the update.
    pub play_count: u32,
}

fn json<T: serde::Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

fn json_opt<T: serde::Serialize>(value: Option<&T>) -> Result<Option<String>> {
    value.map(json).transpose()
}

fn finite(value: f64, what: &str) -> Result<f64> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(Error::InvalidArgument(format!("{what} must be finite")))
    }
}

fn affected(n: usize, id: MediaId) -> Result<()> {
    if n == 0 {
        Err(Error::MediaNotFound(id))
    } else {
        Ok(())
    }
}

pub(crate) fn ensure_media(conn: &Connection, id: MediaId) -> Result<()> {
    let found: Option<i64> = conn
        .query_row("SELECT 1 FROM media WHERE id = ?1", [id.0], |r| r.get(0))
        .optional()?;
    affected(found.map_or(0, |_| 1), id)
}

/// Normalised tag names: trimmed, empty ones and case-insensitive
/// duplicates dropped, first spelling kept.
pub(crate) fn normalize_tags<S: AsRef<str>>(tags: &[S]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags {
        let t = t.as_ref().trim();
        if !t.is_empty() && !out.iter().any(|o| o.to_lowercase() == t.to_lowercase()) {
            out.push(t.to_string());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Connection-level helpers. Library methods lock once and call these, so
// they compose inside one transaction without re-locking.
// ---------------------------------------------------------------------------

pub(crate) fn get_conn(conn: &Connection, id: MediaId) -> Result<Option<MediaRecord>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {MEDIA_COLUMNS} FROM media m WHERE m.id = ?1"
    ))?;
    Ok(stmt.query_row([id.0], media_from_row).optional()?)
}

pub(crate) fn get_by_location_conn(
    conn: &Connection,
    location: &str,
) -> Result<Option<MediaRecord>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {MEDIA_COLUMNS} FROM media m WHERE m.location = ?1"
    ))?;
    Ok(stmt.query_row([location], media_from_row).optional()?)
}

/// Records for `ids`, in the order given; ids that do not exist are skipped.
pub(crate) fn get_many(conn: &Connection, ids: &[MediaId]) -> Result<Vec<MediaRecord>> {
    let mut by_id = std::collections::HashMap::with_capacity(ids.len());
    for chunk in ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!("SELECT {MEDIA_COLUMNS} FROM media m WHERE m.id IN ({placeholders})");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params_from_iter(chunk.iter().map(|id| id.0)),
            media_from_row,
        )?;
        for row in rows {
            let r = row?;
            by_id.insert(r.id, r);
        }
    }
    Ok(ids.iter().filter_map(|id| by_id.get(id).cloned()).collect())
}

pub(crate) fn query_records(
    conn: &Connection,
    where_order_limit: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<MediaRecord>> {
    let sql = format!("SELECT {MEDIA_COLUMNS} FROM media m {where_order_limit}");
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params, media_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

struct ExistingRow {
    id: MediaId,
    source_id: String,
    title: String,
    size: Option<i64>,
    mtime: Option<i64>,
    duration: Option<f64>,
    detected: DetectedFormat,
    user_format: Option<VideoFormat>,
    scripts: String,
    subtitles: String,
    thumbnail_url: Option<String>,
    description: Option<String>,
    tags: Vec<String>,
    missing: bool,
}

fn existing_row(conn: &Connection, location: &str) -> Result<Option<ExistingRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {MEDIA_COLUMNS} FROM media m WHERE m.location = ?1"
    ))?;
    let rec = stmt.query_row([location], media_from_row).optional()?;
    Ok(match rec {
        None => None,
        Some(r) => Some(ExistingRow {
            id: r.id,
            source_id: r.source_id,
            title: r.title,
            size: r.size.map(|s| s as i64),
            mtime: r.mtime,
            duration: r.duration,
            detected: r.detected,
            user_format: r.user_format,
            scripts: json(&r.scripts)?,
            subtitles: json(&r.subtitles)?,
            thumbnail_url: r.thumbnail_url,
            description: r.description,
            tags: r.tags,
            missing: r.missing,
        }),
    })
}

fn size_i64(size: Option<u64>) -> Option<i64> {
    size.map(|s| i64::try_from(s).unwrap_or(i64::MAX))
}

fn replace_source_markers(
    conn: &Connection,
    id: MediaId,
    markers: &[(f64, String)],
) -> Result<bool> {
    let mut stmt = conn.prepare_cached(
        "SELECT time, name FROM markers WHERE media_id = ?1 AND from_source = 1 ORDER BY time, id",
    )?;
    let current: Vec<(f64, String)> = stmt
        .query_map([id.0], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut wanted: Vec<(f64, String)> = markers
        .iter()
        .filter(|(t, _)| t.is_finite())
        .cloned()
        .collect();
    wanted.sort_by(|a, b| a.0.total_cmp(&b.0));
    if current == wanted {
        return Ok(false);
    }
    conn.execute(
        "DELETE FROM markers WHERE media_id = ?1 AND from_source = 1",
        [id.0],
    )?;
    let mut ins = conn.prepare_cached(
        "INSERT INTO markers (media_id, time, name, from_source) VALUES (?1, ?2, ?3, 1)",
    )?;
    for (time, name) in &wanted {
        ins.execute(params![id.0, time, name])?;
    }
    Ok(true)
}

pub(crate) fn upsert_conn(conn: &Connection, u: &MediaUpsert) -> Result<UpsertOutcome> {
    if u.location.is_empty() {
        return Err(Error::InvalidArgument("empty location".into()));
    }
    let scripts = json(&u.scripts)?;
    let subtitles = json(&u.subtitles)?;
    let size = size_i64(u.size);
    let duration = u.duration.filter(|d| d.is_finite() && *d > 0.0);
    let Some(old) = existing_row(conn, &u.location)? else {
        let (kind, stereo) = effective_columns(&u.detected.format, None);
        conn.prepare_cached(
            "INSERT INTO media (location, source_id, title, size, mtime, duration,
                 detected_format, detected_evidence, projection_kind, stereo,
                 scripts, subtitles, thumbnail_url, added_at, description)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )?
        .execute(params![
            u.location,
            u.source_id,
            u.title,
            size,
            u.mtime,
            duration,
            json(&u.detected.format)?,
            evidence_str(u.detected.evidence),
            kind,
            stereo,
            scripts,
            subtitles,
            u.thumbnail_url,
            now(),
            u.description,
        ])?;
        let id = MediaId(conn.last_insert_rowid());
        if let Some(markers) = &u.source_markers {
            replace_source_markers(conn, id, markers)?;
        }
        if let Some(tags) = &u.tags {
            set_tags_conn(conn, id, tags)?;
        }
        return Ok(UpsertOutcome {
            id,
            status: UpsertStatus::Added,
            restored: false,
        });
    };

    let content_changed = u.reset_metadata
        || (size.is_some() && size != old.size)
        || (u.mtime.is_some() && u.mtime != old.mtime);
    let new_size = size.or(old.size);
    let new_mtime = u.mtime.or(old.mtime);
    let new_duration = duration.or(if content_changed { None } else { old.duration });
    // Evidence orders strongest first, so `<=` means "at least as strong".
    let detected = if content_changed || u.detected.evidence <= old.detected.evidence {
        u.detected
    } else {
        old.detected
    };
    let thumbnail_url = u.thumbnail_url.clone().or(old.thumbnail_url.clone());
    let description = u.description.clone().or(old.description.clone());

    let mut changed = content_changed
        || old.missing
        || old.source_id != u.source_id
        || old.title != u.title
        || old.size != new_size
        || old.mtime != new_mtime
        || old.duration != new_duration
        || old.detected != detected
        || old.scripts != scripts
        || old.subtitles != subtitles
        || old.thumbnail_url != thumbnail_url
        || old.description != description;

    if changed {
        let (kind, stereo) = effective_columns(&detected.format, old.user_format.as_ref());
        conn.prepare_cached(
            "UPDATE media SET source_id = ?2, title = ?3, size = ?4, mtime = ?5, duration = ?6,
                 detected_format = ?7, detected_evidence = ?8, projection_kind = ?9, stereo = ?10,
                 scripts = ?11, subtitles = ?12, thumbnail_url = ?13, description = ?14,
                 missing = 0
             WHERE id = ?1",
        )?
        .execute(params![
            old.id.0,
            u.source_id,
            u.title,
            new_size,
            new_mtime,
            new_duration,
            json(&detected.format)?,
            evidence_str(detected.evidence),
            kind,
            stereo,
            scripts,
            subtitles,
            thumbnail_url,
            description,
        ])?;
    }
    if content_changed {
        conn.prepare_cached(
            "UPDATE media SET width = NULL, height = NULL, video_codec = NULL, probed = 0,
                 probe_error = NULL, thumbnail = NULL, resume_thumbnail_at = NULL
             WHERE id = ?1",
        )?
        .execute([old.id.0])?;
    }
    if let Some(markers) = &u.source_markers {
        changed |= replace_source_markers(conn, old.id, markers)?;
    }
    if let Some(tags) = &u.tags
        && normalize_tags(tags) != old.tags
    {
        set_tags_conn(conn, old.id, tags)?;
        changed = true;
    }
    Ok(UpsertOutcome {
        id: old.id,
        status: if changed {
            UpsertStatus::Updated
        } else {
            UpsertStatus::Unchanged
        },
        restored: old.missing,
    })
}

fn tag_id(conn: &Connection, name: &str) -> Result<i64> {
    conn.prepare_cached("INSERT OR IGNORE INTO tags (name) VALUES (?1)")?
        .execute([name])?;
    Ok(conn
        .prepare_cached("SELECT id FROM tags WHERE name = ?1")?
        .query_row([name], |r| r.get(0))?)
}

fn prune_tags(conn: &Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM tags WHERE id NOT IN (SELECT tag_id FROM media_tags)",
        [],
    )?;
    Ok(())
}

pub(crate) fn set_tags_conn<S: AsRef<str>>(
    conn: &Connection,
    id: MediaId,
    tags: &[S],
) -> Result<()> {
    ensure_media(conn, id)?;
    conn.execute("DELETE FROM media_tags WHERE media_id = ?1", [id.0])?;
    for name in normalize_tags(tags) {
        let tid = tag_id(conn, &name)?;
        conn.prepare_cached("INSERT OR IGNORE INTO media_tags (media_id, tag_id) VALUES (?1, ?2)")?
            .execute([id.0, tid])?;
    }
    prune_tags(conn)
}

pub(crate) fn markers_conn(conn: &Connection, id: MediaId) -> Result<Vec<Marker>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, media_id, time, name, from_source FROM markers
         WHERE media_id = ?1 ORDER BY time, id",
    )?;
    let rows = stmt.query_map([id.0], |r| {
        Ok(Marker {
            id: MarkerId(r.get(0)?),
            media_id: MediaId(r.get(1)?),
            time: r.get(2)?,
            name: r.get(3)?,
            from_source: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn set_user_markers_conn(
    conn: &Connection,
    id: MediaId,
    markers: &[(f64, String)],
) -> Result<()> {
    ensure_media(conn, id)?;
    conn.execute(
        "DELETE FROM markers WHERE media_id = ?1 AND from_source = 0",
        [id.0],
    )?;
    let mut ins = conn.prepare_cached(
        "INSERT INTO markers (media_id, time, name, from_source) VALUES (?1, ?2, ?3, 0)",
    )?;
    for (time, name) in markers {
        ins.execute(params![id.0, finite(*time, "marker time")?.max(0.0), name])?;
    }
    Ok(())
}

pub(crate) fn set_user_format_conn(
    conn: &Connection,
    id: MediaId,
    format: Option<&VideoFormat>,
) -> Result<()> {
    let rec = get_conn(conn, id)?.ok_or(Error::MediaNotFound(id))?;
    let (kind, stereo) = effective_columns(&rec.detected.format, format);
    conn.execute(
        "UPDATE media SET user_format = ?2, projection_kind = ?3, stereo = ?4 WHERE id = ?1",
        params![id.0, json_opt(format)?, kind, stereo],
    )?;
    let crop =
        |f: Option<&VideoFormat>| ThumbnailCrop::for_format(f.unwrap_or(&rec.detected.format));
    // The worker makes its pictures again, showing the new part of the frame.
    if crop(format) != crop(rec.user_format.as_ref()) {
        conn.execute(
            "UPDATE media SET probed = 0, resume_thumbnail_at = NULL WHERE id = ?1",
            [id.0],
        )?;
    }
    Ok(())
}

impl Library {
    /// Opens (creating if needed) the database at `path`, creating parent
    /// directories, enabling WAL and migrating to the latest schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Library> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        Library::init(Connection::open(path)?)
    }

    /// Opens the database at [`default_db_path`].
    pub fn open_default() -> Result<Library> {
        Library::open(default_db_path())
    }

    /// A private in-memory database, for tests and previews.
    pub fn open_in_memory() -> Result<Library> {
        Library::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Library> {
        schema::configure(&conn)?;
        schema::migrate(&mut conn, schema::MIGRATIONS)?;
        Ok(Library {
            conn: Arc::new(Mutex::new(conn)),
            hidden: Arc::default(),
        })
    }

    /// Runs `f` with the connection locked.
    pub(crate) fn with<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        // See the type docs: a poisoned lock holds no partial transaction.
        let mut guard = lock(&self.conn);
        f(&mut guard)
    }

    /// Runs `f` inside one transaction, committing when it returns `Ok`.
    pub(crate) fn in_tx<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.with(|conn| {
            let tx = conn.transaction()?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })
    }

    /// Leaves these videos out of searches, counts, continue watching,
    /// recently added, history and [`sources`](Self::sources) (hidden
    /// source ids only) until changed. Nothing is stored; lookups by id or
    /// location still find them.
    pub fn set_hidden(&self, hidden: Hidden) {
        *write(&self.hidden) = hidden;
    }

    /// What [`set_hidden`](Self::set_hidden) hides.
    pub fn hidden(&self) -> Hidden {
        read(&self.hidden).clone()
    }

    /// Schema version of the open database (`PRAGMA user_version`).
    pub fn schema_version(&self) -> Result<i64> {
        self.with(|c| schema::version(c))
    }

    // -- reads --------------------------------------------------------------

    pub fn get(&self, id: MediaId) -> Result<Option<MediaRecord>> {
        self.with(|c| get_conn(c, id))
    }

    /// One record by path or URL.
    pub fn get_by_location(&self, location: &str) -> Result<Option<MediaRecord>> {
        self.with(|c| get_by_location_conn(c, location))
    }

    /// Partly watched videos, most recently played first: resume position
    /// above zero and before [`WATCHED_FRACTION`] of the duration.
    pub fn continue_watching(&self, limit: usize) -> Result<Vec<MediaRecord>> {
        let (hide, mut args) = self.hidden_and();
        args.push(Value::Integer(limit_i64(limit)));
        self.with(|c| {
            query_records(
                c,
                &format!(
                    "WHERE {hide} m.missing = 0 AND {}
                     ORDER BY m.last_played DESC, m.id DESC LIMIT ?",
                    partly_watched_sql()
                ),
                params_from_iter(args),
            )
        })
    }

    /// Newest videos first, missing ones excluded.
    pub fn recently_added(&self, limit: usize) -> Result<Vec<MediaRecord>> {
        let (hide, mut args) = self.hidden_and();
        args.push(Value::Integer(limit_i64(limit)));
        self.with(|c| {
            query_records(
                c,
                &format!("WHERE {hide} m.missing = 0 ORDER BY m.added_at DESC, m.id DESC LIMIT ?"),
                params_from_iter(args),
            )
        })
    }

    /// The hidden-source condition followed by `AND`, and its parameters;
    /// empty when nothing is hidden.
    fn hidden_and(&self) -> (String, Vec<Value>) {
        match hidden_sql("m.", &self.hidden()) {
            Some((sql, args)) => (format!("{sql} AND"), args),
            None => (String::new(), Vec::new()),
        }
    }

    /// Markers of a video ordered by time (source chapters and user
    /// bookmarks).
    pub fn markers(&self, id: MediaId) -> Result<Vec<Marker>> {
        self.with(|c| markers_conn(c, id))
    }

    /// Every tag on videos that aren't hidden, with how many of them carry
    /// it, sorted by name.
    pub fn all_tags(&self) -> Result<Vec<TagCount>> {
        let (shown, args) = match hidden_sql("m.", &self.hidden()) {
            Some((sql, args)) => (format!("WHERE {sql}"), args),
            None => (String::new(), Vec::new()),
        };
        self.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT t.name, COUNT(*) FROM tags t
                 JOIN media_tags mt ON mt.tag_id = t.id
                 JOIN media m ON m.id = mt.media_id {shown}
                 GROUP BY t.id ORDER BY t.name COLLATE NOCASE"
            ))?;
            let rows = stmt.query_map(params_from_iter(args), |r| {
                Ok(TagCount {
                    name: r.get(0)?,
                    count: r.get::<_, i64>(1)?.max(0) as usize,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Distinct source ids with their number of (non-missing) videos.
    pub fn sources(&self) -> Result<Vec<(String, usize)>> {
        let hidden = self.hidden().sources;
        let all: Vec<(String, usize)> = self.with(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT source_id, SUM(missing = 0) FROM media GROUP BY source_id ORDER BY source_id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)?.max(0) as usize))
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })?;
        Ok(all
            .into_iter()
            .filter(|(s, _)| !hidden.contains(s))
            .collect())
    }

    /// Watch history, newest session first.
    pub fn history(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let (hide, mut args) = match hidden_sql("", &self.hidden()) {
            Some((sql, args)) => (
                format!("WHERE media_id NOT IN (SELECT id FROM media WHERE NOT ({sql}))"),
                args,
            ),
            None => (String::new(), Vec::new()),
        };
        args.push(Value::Integer(limit_i64(limit)));
        self.with(|c| {
            let mut stmt = c.prepare_cached(&format!(
                "SELECT id, media_id, started_at, updated_at, position, completed FROM history {hide}
                 ORDER BY started_at DESC, id DESC LIMIT ?"
            ))?;
            let rows = stmt.query_map(params_from_iter(args), |r| {
                Ok(HistoryEntry {
                    id: SessionId(r.get(0)?),
                    media_id: MediaId(r.get(1)?),
                    started_at: r.get(2)?,
                    updated_at: r.get(3)?,
                    position: r.get(4)?,
                    completed: r.get(5)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    // -- indexing -----------------------------------------------------------

    /// Inserts or refreshes one video, keeping all user data. A changed size
    /// or mtime clears probed metadata and thumbnails so the worker redoes
    /// them; a missing row comes back.
    pub fn upsert(&self, entry: &MediaUpsert) -> Result<UpsertOutcome> {
        self.in_tx(|c| upsert_conn(c, entry))
    }

    /// Indexes a video entry from a remote source (DeoVR feed, SMB,
    /// WebDAV...). Non-video entries are rejected.
    pub fn upsert_entry(&self, source_id: &str, entry: &Entry) -> Result<UpsertOutcome> {
        let u = MediaUpsert::from_entry(source_id, entry)?;
        self.upsert(&u)
    }

    /// Indexes many entries from one listing in a single transaction,
    /// skipping directories and side files.
    pub fn upsert_entries(&self, source_id: &str, entries: &[Entry]) -> Result<Vec<UpsertOutcome>> {
        let ups = entries
            .iter()
            .filter(|e| e.kind == EntryKind::Video)
            .map(|e| MediaUpsert::from_entry(source_id, e))
            .collect::<Result<Vec<_>>>()?;
        self.in_tx(|c| ups.iter().map(|u| upsert_conn(c, u)).collect())
    }

    /// Deletes a row and everything attached to it (tags, markers, history,
    /// playlist entries). Only for explicit user requests: scans never
    /// delete, they mark rows missing.
    pub fn remove_media(&self, id: MediaId) -> Result<()> {
        self.in_tx(|c| {
            let n = c.execute("DELETE FROM media WHERE id = ?1", [id.0])?;
            affected(n, id)?;
            prune_tags(c)
        })
    }

    /// Deletes rows marked missing, optionally only from one source.
    /// Returns how many were removed. Explicit user action only.
    pub fn purge_missing(&self, source_id: Option<&str>) -> Result<usize> {
        self.in_tx(|c| {
            let n = c.execute(
                "DELETE FROM media WHERE missing = 1 AND (?1 IS NULL OR source_id = ?1)",
                [source_id],
            )?;
            prune_tags(c)?;
            Ok(n)
        })
    }

    // -- user data ----------------------------------------------------------

    /// Sets the 0–5 star rating (0 clears it).
    pub fn set_rating(&self, id: MediaId, rating: u8) -> Result<()> {
        if rating > 5 {
            return Err(Error::InvalidArgument(format!(
                "rating {rating} is outside 0..=5"
            )));
        }
        self.with(|c| {
            let n = c.execute(
                "UPDATE media SET rating = ?2 WHERE id = ?1",
                params![id.0, rating],
            )?;
            affected(n, id)
        })
    }

    /// Marks or unmarks a favourite.
    pub fn set_favorite(&self, id: MediaId, favorite: bool) -> Result<()> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE media SET favorite = ?2 WHERE id = ?1",
                params![id.0, favorite],
            )?;
            affected(n, id)
        })
    }

    /// Replaces the tags of a video. Names are trimmed and deduplicated
    /// case-insensitively; tags no video uses any more are dropped.
    pub fn set_tags<S: AsRef<str>>(&self, id: MediaId, tags: &[S]) -> Result<()> {
        self.in_tx(|c| set_tags_conn(c, id, tags))
    }

    /// Adds one tag (no-op if present, case-insensitively).
    pub fn add_tag(&self, id: MediaId, tag: &str) -> Result<()> {
        self.in_tx(|c| {
            let mut tags = get_conn(c, id)?.ok_or(Error::MediaNotFound(id))?.tags;
            tags.push(tag.to_string());
            set_tags_conn(c, id, &tags)
        })
    }

    /// Removes one tag (case-insensitive match).
    pub fn remove_tag(&self, id: MediaId, tag: &str) -> Result<()> {
        self.in_tx(|c| {
            let mut tags = get_conn(c, id)?.ok_or(Error::MediaNotFound(id))?.tags;
            let tag = tag.trim().to_lowercase();
            tags.retain(|t| t.to_lowercase() != tag);
            set_tags_conn(c, id, &tags)
        })
    }

    /// Forces a format for this file, or clears the override with `None`.
    pub fn set_user_format(&self, id: MediaId, format: Option<VideoFormat>) -> Result<()> {
        self.in_tx(|c| set_user_format_conn(c, id, format.as_ref()))
    }

    /// Stores the format found when the video was opened, unless the one
    /// stored has stronger evidence (a probe found metadata, or the source
    /// declared it). Server videos are probed only when the worker gets to
    /// them, so until then this is all that corrects their format.
    pub fn set_detected(&self, id: MediaId, detected: DetectedFormat) -> Result<()> {
        self.in_tx(|c| {
            let rec = get_conn(c, id)?.ok_or(Error::MediaNotFound(id))?;
            if detected.evidence > rec.detected.evidence || detected == rec.detected {
                return Ok(());
            }
            let (kind, stereo) = effective_columns(&detected.format, rec.user_format.as_ref());
            c.execute(
                "UPDATE media SET detected_format = ?2, detected_evidence = ?3,
                     projection_kind = ?4, stereo = ?5
                 WHERE id = ?1",
                params![
                    id.0,
                    json(&detected.format)?,
                    evidence_str(detected.evidence),
                    kind,
                    stereo
                ],
            )?;
            Ok(())
        })
    }

    /// Every video's title and location, hidden ones too: the names a
    /// crash report must not show.
    pub fn names(&self) -> Result<Vec<(String, String)>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT title, location FROM media")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Stores per-video picture corrections, or resets them with `None`.
    pub fn set_view_settings(&self, id: MediaId, settings: Option<&ViewSettings>) -> Result<()> {
        let value = json_opt(settings)?;
        self.with(|c| {
            let n = c.execute(
                "UPDATE media SET view_settings = ?2 WHERE id = ?1",
                params![id.0, value],
            )?;
            affected(n, id)
        })
    }

    /// Stores the audio and subtitles picked for a video.
    pub fn set_tracks(&self, id: MediaId, tracks: &PickedTracks) -> Result<()> {
        let value = json_opt(Some(tracks).filter(|t| **t != PickedTracks::default()))?;
        self.with(|c| {
            let n = c.execute(
                "UPDATE media SET tracks = ?2 WHERE id = ?1",
                params![id.0, value],
            )?;
            affected(n, id)
        })
    }

    /// Stores keyframed settings for the video's timeline.
    pub fn set_keyframes(&self, id: MediaId, keyframes: &Keyframes) -> Result<()> {
        let value = json(keyframes)?;
        self.with(|c| {
            let n = c.execute(
                "UPDATE media SET keyframes = ?2 WHERE id = ?1",
                params![id.0, value],
            )?;
            affected(n, id)
        })
    }

    /// Adds a user bookmark at `time` seconds.
    pub fn add_marker(&self, id: MediaId, time: f64, name: &str) -> Result<MarkerId> {
        let time = finite(time, "marker time")?.max(0.0);
        self.with(|c| {
            ensure_media(c, id)?;
            c.execute(
                "INSERT INTO markers (media_id, time, name, from_source) VALUES (?1, ?2, ?3, 0)",
                params![id.0, time, name],
            )?;
            Ok(MarkerId(c.last_insert_rowid()))
        })
    }

    /// Renames or moves a marker.
    pub fn update_marker(&self, marker: MarkerId, time: f64, name: &str) -> Result<()> {
        let time = finite(time, "marker time")?.max(0.0);
        self.with(|c| {
            let n = c.execute(
                "UPDATE markers SET time = ?2, name = ?3 WHERE id = ?1",
                params![marker.0, time, name],
            )?;
            if n == 0 {
                return Err(Error::InvalidArgument(format!("marker {marker} not found")));
            }
            Ok(())
        })
    }

    /// Deletes a marker. Returns whether it existed.
    pub fn remove_marker(&self, marker: MarkerId) -> Result<bool> {
        self.with(|c| Ok(c.execute("DELETE FROM markers WHERE id = ?1", [marker.0])? > 0))
    }

    /// Replaces all user markers of a video (source chapters are kept).
    pub fn set_markers(&self, id: MediaId, markers: &[(f64, String)]) -> Result<()> {
        self.in_tx(|c| set_user_markers_conn(c, id, markers))
    }

    // -- playback -----------------------------------------------------------

    /// Starts a playback session: adds a history row and sets
    /// `last_played`. Report progress with [`Library::update_playback`].
    pub fn start_playback(&self, id: MediaId) -> Result<SessionId> {
        self.in_tx(|c| {
            let t = now();
            let n = c.execute(
                "UPDATE media SET last_played = ?2 WHERE id = ?1",
                params![id.0, t],
            )?;
            affected(n, id)?;
            c.execute(
                "INSERT INTO history (media_id, started_at, updated_at, position)
                 VALUES (?1, ?2, ?2, 0)",
                params![id.0, t],
            )?;
            Ok(SessionId(c.last_insert_rowid()))
        })
    }

    /// Records progress for a session. Updates the resume position,
    /// `last_played` and the history row. Playback that finishes or passes
    /// [`WATCHED_FRACTION`] of the duration counts as watched: the resume
    /// position goes back to 0 and the play count goes up, once per session.
    pub fn update_playback(
        &self,
        session: SessionId,
        position: f64,
        finished: bool,
    ) -> Result<PlaybackUpdate> {
        let position = finite(position, "position")?.max(0.0);
        self.in_tx(|c| {
            let row: Option<(i64, bool, Option<f64>)> = c
                .query_row(
                    "SELECT h.media_id, h.completed, m.duration FROM history h
                     JOIN media m ON m.id = h.media_id WHERE h.id = ?1",
                    [session.0],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((media_id, completed, duration)) = row else {
                return Err(Error::InvalidArgument(format!(
                    "playback session {session} not found"
                )));
            };
            let watched =
                finished || duration.is_some_and(|d| d > 0.0 && position >= d * WATCHED_FRACTION);
            let counted_now = watched && !completed;
            let resume = if watched { 0.0 } else { position };
            let t = now();
            c.execute(
                "UPDATE media SET resume_position = ?2, last_played = ?3,
                     play_count = play_count + ?4 WHERE id = ?1",
                params![media_id, resume, t, counted_now as i64],
            )?;
            c.execute(
                "UPDATE history SET position = ?2, updated_at = ?3, completed = completed OR ?4
                 WHERE id = ?1",
                params![session.0, position, t, watched],
            )?;
            let play_count: i64 = c.query_row(
                "SELECT play_count FROM media WHERE id = ?1",
                [media_id],
                |r| r.get(0),
            )?;
            Ok(PlaybackUpdate {
                resume_position: resume,
                counted_now,
                play_count: play_count.clamp(0, u32::MAX as i64) as u32,
            })
        })
    }

    /// Sets the resume position directly (0 clears it).
    pub fn set_resume_position(&self, id: MediaId, position: f64) -> Result<()> {
        let position = finite(position, "position")?.max(0.0);
        self.with(|c| {
            let n = c.execute(
                "UPDATE media SET resume_position = ?2 WHERE id = ?1",
                params![id.0, position],
            )?;
            affected(n, id)
        })
    }

    /// Takes a video out of the watch history: its sessions, last playback,
    /// resume position and play count go. Ratings, favourites, tags,
    /// bookmarks and adjustments are kept.
    pub fn remove_from_history(&self, id: MediaId) -> Result<()> {
        self.in_tx(|c| {
            let n = c.execute(
                "UPDATE media SET last_played = NULL, resume_position = 0, play_count = 0
                 WHERE id = ?1",
                [id.0],
            )?;
            affected(n, id)?;
            c.execute("DELETE FROM history WHERE media_id = ?1", [id.0])?;
            Ok(())
        })
    }

    /// Deletes the whole watch history: every session, and every video's
    /// last playback, resume position and play count. Ratings, favourites,
    /// tags, bookmarks and adjustments are kept.
    pub fn clear_history(&self) -> Result<()> {
        self.in_tx(|c| {
            c.execute("DELETE FROM history", [])?;
            c.execute(
                "UPDATE media SET last_played = NULL, resume_position = 0, play_count = 0",
                [],
            )?;
            Ok(())
        })
    }

    /// Deletes the videos of `source_id` that were never played and carry
    /// nothing the user set (favourite, rating, tags, format, adjustments,
    /// keyframes, bookmarks, playlists), for a source being removed.
    /// Returns how many were deleted.
    pub fn forget_unwatched(&self, source_id: &str) -> Result<usize> {
        self.in_tx(|c| {
            let n = c.execute(
                "DELETE FROM media WHERE id IN (
                     SELECT m.id FROM media m
                     WHERE m.last_played IS NULL AND m.favorite = 0 AND m.rating = 0
                       AND m.user_format IS NULL AND m.view_settings IS NULL
                       AND COALESCE(json_array_length(m.keyframes, '$.frames'), 0) = 0
                       AND NOT EXISTS (SELECT 1 FROM media_tags t WHERE t.media_id = m.id)
                       AND NOT EXISTS (SELECT 1 FROM markers k
                                       WHERE k.media_id = m.id AND k.from_source = 0)
                       AND NOT EXISTS (SELECT 1 FROM playlist_items p WHERE p.media_id = m.id)
                       AND NOT EXISTS (SELECT 1 FROM history h WHERE h.media_id = m.id)
                       AND m.source_id = ?1)",
                [source_id],
            )?;
            prune_tags(c)?;
            Ok(n)
        })
    }

    /// What a video's source says about it now: its title, description,
    /// thumbnail and tags, each when given. The rest of the video is left
    /// alone.
    pub fn update_details(&self, id: MediaId, details: &SourceDetails<'_>) -> Result<()> {
        self.in_tx(|c| {
            c.execute(
                "UPDATE media SET title = COALESCE(?2, title),
                     description = COALESCE(?3, description),
                     thumbnail_url = COALESCE(?4, thumbnail_url)
                 WHERE id = ?1",
                params![
                    id.0,
                    details.title,
                    details.description,
                    details.thumbnail_url
                ],
            )?;
            if let Some(tags) = details.tags {
                set_tags_conn(c, id, tags)?;
            }
            Ok(())
        })
    }

    /// Leaves the videos of `source_id` without a source, for a library
    /// whose folder changed: its next scan takes back those still in it.
    /// Returns how many there were.
    pub fn detach_source(&self, source_id: &str) -> Result<usize> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE media SET source_id = '' WHERE source_id = ?1",
                [source_id],
            )?)
        })
    }

    // -- metadata worker support ---------------------------------------------

    /// Videos the metadata worker still has to probe: not missing, not
    /// failed, not on a card that is out, and not yet probed, or without any
    /// picture when it makes `thumbnails`. Oldest first.
    pub fn pending_metadata(&self, limit: usize, thumbnails: bool) -> Result<Vec<MediaRecord>> {
        let sql = format!("WHERE {} ORDER BY m.id LIMIT ?1", pending_sql(thumbnails));
        self.with(|c| query_records(c, &sql, [limit_i64(limit)]))
    }

    /// Number of rows [`Library::pending_metadata`] would return without a
    /// limit.
    pub fn pending_metadata_count(&self, thumbnails: bool) -> Result<usize> {
        let sql = format!(
            "SELECT COUNT(*) FROM media m WHERE {}",
            pending_sql(thumbnails)
        );
        self.with(|c| {
            let n: i64 = c.query_row(&sql, [], |r| r.get(0))?;
            Ok(n.max(0) as usize)
        })
    }

    /// Every picture file the videos use (worker use: what else is in its
    /// folder can go).
    pub(crate) fn picture_files(&self) -> Result<HashSet<PathBuf>> {
        self.with(|c| {
            Ok(c.prepare(
                "SELECT thumbnail FROM media WHERE thumbnail IS NOT NULL
                 UNION SELECT resume_thumbnail FROM media WHERE resume_thumbnail IS NOT NULL",
            )?
            .query_map([], |r| r.get::<_, String>(0))?
            .map(|p| p.map(PathBuf::from))
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Partly watched videos whose frame at the resume position is still
    /// to be made, most recently played first.
    pub fn pending_resume_thumbnails(&self, limit: usize) -> Result<Vec<MediaRecord>> {
        let sql = format!(
            "WHERE m.missing = 0 AND {}
               AND (m.resume_thumbnail_at IS NULL OR m.resume_thumbnail_at <> m.resume_position){}
             ORDER BY m.last_played DESC, m.id DESC LIMIT ?1",
            partly_watched_sql(),
            on_inserted_cards()
        );
        self.with(|c| query_records(c, &sql, [limit_i64(limit)]))
    }

    /// Stores the frame made for `rec` at its resume position; `None` when
    /// it could not be made, so it is not tried again for that position.
    pub(crate) fn store_resume_thumbnail(
        &self,
        rec: &MediaRecord,
        path: Option<&Path>,
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE media SET resume_thumbnail = ?2, resume_thumbnail_at = ?3 WHERE id = ?1",
                params![
                    rec.id.0,
                    path.map(|p| p.to_string_lossy().into_owned()),
                    rec.resume_position
                ],
            )?;
            Ok(())
        })
    }

    /// Forgets the frames of videos no longer partly watched, or `all` of
    /// them, and returns their files, for the worker to delete.
    pub(crate) fn take_resume_thumbnails(&self, all: bool) -> Result<Vec<PathBuf>> {
        let held = "(m.resume_thumbnail IS NOT NULL OR m.resume_thumbnail_at IS NOT NULL)";
        let stale = if all {
            held.to_string()
        } else {
            format!("NOT {} AND {held}", partly_watched_sql())
        };
        self.in_tx(|c| {
            let paths = c
                .prepare(&format!(
                    "SELECT m.resume_thumbnail FROM media m
                     WHERE {stale} AND m.resume_thumbnail IS NOT NULL"
                ))?
                .query_map([], |r| r.get::<_, String>(0))?
                .map(|p| p.map(PathBuf::from))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            c.execute(
                &format!(
                    "UPDATE media SET resume_thumbnail = NULL, resume_thumbnail_at = NULL
                     WHERE id IN (SELECT m.id FROM media m WHERE {stale})"
                ),
                [],
            )?;
            Ok(paths)
        })
    }

    /// Clears recorded probe failures, of `source_id`'s videos when given,
    /// so the worker tries those videos again. Returns how many were reset.
    pub fn retry_failed_probes(&self, source_id: Option<&str>) -> Result<usize> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE media SET probe_error = NULL
                 WHERE probe_error IS NOT NULL AND (?1 IS NULL OR source_id = ?1)",
                [source_id],
            )?)
        })
    }

    /// Stores the results of probing `probed` (worker use), unless the video
    /// changed while it was probed: a rescan found a new file, or it was
    /// removed. Then nothing is stored, it stays pending, and this returns
    /// false.
    pub(crate) fn store_probe(&self, probed: &MediaRecord, probe: &StoredProbe) -> Result<bool> {
        self.in_tx(|c| {
            let Some(rec) = get_conn(c, probed.id)? else {
                return Ok(false);
            };
            if (&rec.location, rec.size, rec.mtime) != (&probed.location, probed.size, probed.mtime)
            {
                return Ok(false);
            }
            // A stronger detection that came meanwhile (a feed's) stays.
            let detected = if probe.detected.evidence <= rec.detected.evidence {
                probe.detected
            } else {
                rec.detected
            };
            let (kind, stereo) = effective_columns(&detected.format, rec.user_format.as_ref());
            c.execute(
                "UPDATE media SET duration = COALESCE(?2, duration), width = ?3, height = ?4,
                     video_codec = ?5, detected_format = ?6, detected_evidence = ?7,
                     projection_kind = ?8, stereo = ?9, thumbnail = ?10, probe_error = ?11,
                     probed = 1
                 WHERE id = ?1",
                params![
                    probed.id.0,
                    probe.duration,
                    probe.width,
                    probe.height,
                    probe.video_codec,
                    json(&detected.format)?,
                    evidence_str(detected.evidence),
                    kind,
                    stereo,
                    probe
                        .thumbnail
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    probe.error,
                ],
            )?;
            Ok(true)
        })
    }

    /// Records a probe failure unless its file or location changed meanwhile.
    pub(crate) fn store_probe_error(&self, probed: &MediaRecord, error: &str) -> Result<bool> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE media SET probe_error = ?2 WHERE id = ?1
                     AND location = ?3 AND size IS ?4 AND mtime IS ?5",
                params![
                    probed.id.0,
                    error,
                    probed.location,
                    size_i64(probed.size),
                    probed.mtime
                ],
            )? != 0)
        })
    }
}

/// What the worker learned about one video.
pub(crate) struct StoredProbe {
    pub duration: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub video_codec: Option<String>,
    pub detected: DetectedFormat,
    pub thumbnail: Option<PathBuf>,
    pub error: Option<String>,
}

pub(crate) fn limit_i64(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

/// SQL leaving out videos on cards that are not inserted
/// ([`fp_core::volume`]). Card IDs hold only letters, digits and dashes.
fn on_inserted_cards() -> String {
    let scheme = fp_core::volume::SCHEME;
    let mut sql = format!(" AND (m.location NOT LIKE '{scheme}%'");
    for id in fp_core::volume::mounted().into_iter().filter_map(|v| v.id) {
        sql += &format!(" OR m.location = '{scheme}{id}' OR m.location LIKE '{scheme}{id}/%'");
    }
    sql.push(')');
    sql
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::format::Evidence;
    use fp_core::{Projection, StereoLayout};

    fn lib() -> Library {
        Library::open_in_memory().unwrap()
    }

    fn add(lib: &Library, loc: &str) -> MediaId {
        lib.upsert(&MediaUpsert::new(loc, "local")).unwrap().id
    }

    #[test]
    fn open_file_database_uses_wal_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/lib.sqlite3");
        let id = {
            let lib = Library::open(&path).unwrap();
            let mode: String = lib
                .with(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?))
                .unwrap();
            assert_eq!(mode.to_lowercase(), "wal");
            let id = add(&lib, "/v/a_180_LR.mp4");
            lib.set_rating(id, 4).unwrap();
            id
        };
        let lib = Library::open(&path).unwrap();
        assert_eq!(lib.schema_version().unwrap(), schema::SCHEMA_VERSION);
        assert_eq!(lib.get(id).unwrap().unwrap().rating, 4);
    }

    #[test]
    fn upsert_new_row_detects_format_from_name() {
        let lib = lib();
        let out = lib
            .upsert(&MediaUpsert::new("/v/Scene_180_LR.mp4", "local"))
            .unwrap();
        assert_eq!(out.status, UpsertStatus::Added);
        let r = lib.get(out.id).unwrap().unwrap();
        assert_eq!(r.title, "Scene_180_LR");
        assert_eq!(r.file_name(), "Scene_180_LR.mp4");
        assert_eq!(r.detected.evidence, Evidence::FileName);
        assert_eq!(r.detected.format.projection, Projection::EQUIRECT_180);
        assert_eq!(r.effective_format(), r.detected);
        assert!(r.tags.is_empty());
        assert_eq!(r.keyframes, Keyframes::default());
        assert!(!r.probed);
        assert_eq!(lib.get_by_location("/v/Scene_180_LR.mp4").unwrap(), Some(r));
        assert_eq!(lib.get_by_location("/nope").unwrap(), None);
    }

    #[test]
    fn source_identity_folders_and_queries_do_not_become_filename_format_hints() {
        let key = "VR_180_LR/Plain.mp4?projection=360_TB";
        let location = format!(
            "source://source/server/http/{}",
            percent_encoding::utf8_percent_encode(key, percent_encoding::NON_ALPHANUMERIC)
        );
        let expected = MediaUpsert::new("Plain.mp4", "local");
        let upsert = MediaUpsert::new(&location, "server");
        assert_eq!(upsert.title, expected.title);
        assert_eq!(upsert.detected, expected.detected);
        let lib = lib();
        let id = lib.upsert(&upsert).unwrap().id;
        assert_eq!(lib.get(id).unwrap().unwrap().file_name(), "Plain.mp4");
    }

    #[test]
    fn reupsert_preserves_user_data() {
        let lib = lib();
        let mut u = MediaUpsert::new("/v/a.mp4", "local");
        u.size = Some(100);
        u.mtime = Some(10);
        let id = lib.upsert(&u).unwrap().id;

        lib.set_rating(id, 5).unwrap();
        lib.set_favorite(id, true).unwrap();
        lib.set_tags(id, &["Beach", "sunset"]).unwrap();
        let user = VideoFormat::new(Projection::EQUIRECT_360, StereoLayout::TopBottom);
        lib.set_user_format(id, Some(user)).unwrap();
        let vs = ViewSettings {
            zoom: 1.3,
            ..Default::default()
        };
        lib.set_view_settings(id, Some(&vs)).unwrap();
        let mut kf = Keyframes::default();
        kf.insert(5.0, vs);
        lib.set_keyframes(id, &kf).unwrap();
        lib.add_marker(id, 12.0, "good bit").unwrap();
        lib.set_resume_position(id, 33.0).unwrap();
        let tracks = PickedTracks {
            audio: Some(2),
            subtitles: Some(crate::PickedSubtitles::File("/v/a.ar.srt".into())),
        };
        lib.set_tracks(id, &tracks).unwrap();

        // Same file again: unchanged.
        assert_eq!(lib.upsert(&u).unwrap().status, UpsertStatus::Unchanged);
        // File changed on disk and gained a script.
        u.size = Some(200);
        u.scripts = vec!["/v/a.funscript".into()];
        let out = lib.upsert(&u).unwrap();
        assert_eq!(out.status, UpsertStatus::Updated);
        assert_eq!(out.id, id);

        let r = lib.get(id).unwrap().unwrap();
        assert_eq!(r.size, Some(200));
        assert_eq!(r.scripts, vec!["/v/a.funscript".to_string()]);
        assert_eq!(r.rating, 5);
        assert!(r.favorite);
        assert_eq!(r.tags, vec!["Beach".to_string(), "sunset".to_string()]);
        assert_eq!(r.user_format, Some(user));
        assert_eq!(r.effective_format().evidence, Evidence::User);
        assert_eq!(r.effective_format().format, user);
        assert_eq!(r.view_settings, Some(vs));
        assert_eq!(r.keyframes, kf);
        assert_eq!(r.resume_position, 33.0);
        assert_eq!(r.tracks, tracks);
        assert_eq!(lib.markers(id).unwrap().len(), 1);
    }

    #[test]
    fn picked_tracks_are_kept_and_cleared() {
        let lib = lib();
        let id = lib
            .upsert(&MediaUpsert::new("/v/a.mkv", "local"))
            .unwrap()
            .id;
        assert_eq!(
            lib.get(id).unwrap().unwrap().tracks,
            PickedTracks::default()
        );
        let off = PickedTracks {
            audio: None,
            subtitles: Some(crate::PickedSubtitles::Off),
        };
        lib.set_tracks(id, &off).unwrap();
        assert_eq!(lib.get(id).unwrap().unwrap().tracks, off);
        // Clearing the history keeps them, as it keeps adjustments.
        lib.clear_history().unwrap();
        assert_eq!(lib.get(id).unwrap().unwrap().tracks, off);
        lib.set_tracks(id, &PickedTracks::default()).unwrap();
        let stored: Option<String> = lib
            .with(|c| {
                Ok(
                    c.query_row("SELECT tracks FROM media WHERE id = ?1", [id.0], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(stored, None, "nothing picked is no value");
    }

    #[test]
    fn content_change_resets_probe_but_unchanged_keeps_stronger_detection() {
        let lib = lib();
        let mut u = MediaUpsert::new("/v/clip.mp4", "local");
        u.size = Some(1);
        u.mtime = Some(1);
        let id = lib.upsert(&u).unwrap().id;
        let detected = DetectedFormat {
            format: VideoFormat::new(Projection::EQUIRECT_360, StereoLayout::Mono),
            evidence: Evidence::Metadata,
        };
        let stored = lib
            .store_probe(
                &lib.get(id).unwrap().unwrap(),
                &StoredProbe {
                    duration: Some(60.0),
                    width: Some(4096),
                    height: Some(2048),
                    video_codec: Some("hevc".into()),
                    detected,
                    thumbnail: Some("/cache/1.jpg".into()),
                    error: None,
                },
            )
            .unwrap();
        assert!(stored);
        // Rescan with the same size/mtime: name says nothing, metadata kept.
        assert_eq!(lib.upsert(&u).unwrap().status, UpsertStatus::Unchanged);
        let r = lib.get(id).unwrap().unwrap();
        assert_eq!(r.detected, detected);
        assert_eq!(r.duration, Some(60.0));
        assert!(r.probed);
        // Modified file: metadata cleared for re-probe.
        u.mtime = Some(2);
        lib.upsert(&u).unwrap();
        let r = lib.get(id).unwrap().unwrap();
        assert_eq!(r.detected.evidence, Evidence::Default);
        assert_eq!((r.duration, r.width, r.thumbnail), (None, None, None));
        assert!(!r.probed);
    }

    #[test]
    fn opening_a_video_corrects_a_guessed_format() {
        let lib = lib();
        // A server's video whose name says nothing: the fallback, a guess.
        let u = MediaUpsert::new("mb:http://jf:8096#item=abc", "jf");
        let id = lib.upsert(&u).unwrap().id;
        assert_eq!(
            lib.get(id).unwrap().unwrap().detected.evidence,
            Evidence::Default
        );
        // Opened, its 16:9 picture says flat.
        let flat = DetectedFormat {
            format: VideoFormat::default(),
            evidence: Evidence::AspectRatio,
        };
        lib.set_detected(id, flat).unwrap();
        let r = lib.get(id).unwrap().unwrap();
        assert_eq!(r.detected, flat);
        let flat_only = crate::Query {
            projections: vec![crate::ProjectionKind::Flat],
            ..Default::default()
        };
        assert_eq!(lib.search(&flat_only).unwrap().len(), 1);
        // A weaker guess doesn't undo it.
        let guess = DetectedFormat {
            format: VideoFormat::FALLBACK,
            evidence: Evidence::Default,
        };
        lib.set_detected(id, guess).unwrap();
        assert_eq!(lib.get(id).unwrap().unwrap().detected, flat);
    }

    #[test]
    fn upsert_entry_from_remote_source() {
        let lib = lib();
        let mut e = Entry::new("Trip.mp4", "http://srv/deovr/trip.mp4", EntryKind::Video);
        e.format = Some(VideoFormat::new(
            Projection::fisheye(200.0),
            StereoLayout::SideBySide,
        ));
        e.duration = Some(1200.0);
        e.thumbnail_url = Some("http://srv/t.jpg".into());
        e.scripts = vec!["http://srv/trip.funscript".into()];
        e.markers = vec![(60.0, "Intro".into()), (10.0, "Start".into())];
        let out = lib.upsert_entry("feed", &e).unwrap();
        let r = lib.get(out.id).unwrap().unwrap();
        assert_eq!(r.title, "Trip");
        assert_eq!(r.source_id, "feed");
        // A title from a server keeps its dots.
        let titled = Entry::new("Dr. Strangelove", "mb:http://jf#item=1", EntryKind::Video);
        let id = lib.upsert_entry("jf", &titled).unwrap().id;
        assert_eq!(lib.get(id).unwrap().unwrap().title, "Dr. Strangelove");
        assert_eq!(r.detected.evidence, Evidence::Metadata);
        assert_eq!(r.duration, Some(1200.0));
        assert_eq!(r.thumbnail_url.as_deref(), Some("http://srv/t.jpg"));
        let m = lib.markers(out.id).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].name, "Start");
        assert!(m.iter().all(|m| m.from_source));

        // User marker survives a refresh that changes the source chapters.
        lib.add_marker(out.id, 30.0, "mine").unwrap();
        e.markers = vec![(5.0, "New".into())];
        assert_eq!(
            lib.upsert_entry("feed", &e).unwrap().status,
            UpsertStatus::Updated
        );
        let names: Vec<String> = lib
            .markers(out.id)
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, vec!["New".to_string(), "mine".to_string()]);
        assert_eq!(
            lib.upsert_entry("feed", &e).unwrap().status,
            UpsertStatus::Unchanged
        );

        let dir = Entry::new("d", "http://srv/d/", EntryKind::Directory);
        assert!(matches!(
            lib.upsert_entry("feed", &dir),
            Err(Error::InvalidArgument(_))
        ));
        let outs = lib.upsert_entries("feed", &[dir, e]).unwrap();
        assert_eq!(outs.len(), 1);
    }

    #[test]
    fn tags_are_normalised_and_pruned() {
        let lib = lib();
        let a = add(&lib, "/a.mp4");
        let b = add(&lib, "/b.mp4");
        lib.set_tags(a, &[" Outdoor ", "outdoor", "", "POV"])
            .unwrap();
        lib.add_tag(b, "outdoor").unwrap();
        lib.add_tag(b, "OUTDOOR").unwrap();
        assert_eq!(lib.get(a).unwrap().unwrap().tags, vec!["Outdoor", "POV"]);
        assert_eq!(lib.get(b).unwrap().unwrap().tags, vec!["Outdoor"]);
        let tags = lib.all_tags().unwrap();
        assert_eq!(
            tags,
            vec![
                TagCount {
                    name: "Outdoor".into(),
                    count: 2
                },
                TagCount {
                    name: "POV".into(),
                    count: 1
                }
            ]
        );
        lib.remove_tag(a, "pov").unwrap();
        assert_eq!(lib.all_tags().unwrap().len(), 1);
        assert!(matches!(
            lib.set_tags(MediaId(999), &["x"]),
            Err(Error::MediaNotFound(_))
        ));
    }

    #[test]
    fn validation_and_not_found() {
        let lib = lib();
        let a = add(&lib, "/a.mp4");
        assert!(matches!(
            lib.set_rating(a, 6),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            lib.set_rating(MediaId(42), 3),
            Err(Error::MediaNotFound(MediaId(42)))
        ));
        assert!(matches!(
            lib.add_marker(a, f64::NAN, "x"),
            Err(Error::InvalidArgument(_))
        ));
        assert!(lib.upsert(&MediaUpsert::new("", "local")).is_err());
    }

    #[test]
    fn user_format_changes_effective_columns() {
        let lib = lib();
        let a = add(&lib, "/a.mp4");
        lib.set_user_format(
            a,
            Some(VideoFormat::new(
                Projection::EQUIRECT_180,
                StereoLayout::SideBySide,
            )),
        )
        .unwrap();
        let (kind, stereo): (String, String) = lib
            .with(|c| {
                Ok(c.query_row(
                    "SELECT projection_kind, stereo FROM media WHERE id = ?1",
                    [a.0],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(
            (kind.as_str(), stereo.as_str()),
            ("equirect180", "side_by_side")
        );
        lib.set_user_format(a, None).unwrap();
        let r = lib.get(a).unwrap().unwrap();
        assert_eq!(r.user_format, None);
        assert_eq!(r.effective_format().evidence, Evidence::Default);
    }

    #[test]
    fn formats_saved_before_masks_take_the_mask_from_the_name() {
        let lib = lib();
        let a = add(&lib, "/card/Scene_4000p_FISHEYE190_alpha.mp4");
        let b = add(&lib, "/card/Scene_4000p_FISHEYE190.mp4");
        let old = r#"{"projection":{"kind":"fisheye","fov":190.0},"stereo":"side_by_side","eyes_swapped":false}"#;
        let set = |id: MediaId, json: &str| {
            lib.with(|c| {
                Ok(c.execute(
                    "UPDATE media SET user_format = ?2 WHERE id = ?1",
                    rusqlite::params![id.0, json],
                )?)
            })
            .unwrap();
        };
        set(a, old);
        set(b, old);
        assert!(
            lib.get(a)
                .unwrap()
                .unwrap()
                .user_format
                .unwrap()
                .alpha_packed
        );
        assert!(
            !lib.get(b)
                .unwrap()
                .unwrap()
                .user_format
                .unwrap()
                .alpha_packed
        );
        // Turned off since: stays off.
        let mut off = lib.get(a).unwrap().unwrap().user_format.unwrap();
        off.alpha_packed = false;
        lib.set_user_format(a, Some(off)).unwrap();
        assert!(
            !lib.get(a)
                .unwrap()
                .unwrap()
                .user_format
                .unwrap()
                .alpha_packed
        );
    }

    #[test]
    fn markers_crud() {
        let lib = lib();
        let a = add(&lib, "/a.mp4");
        let m1 = lib.add_marker(a, 20.0, "b").unwrap();
        lib.add_marker(a, 10.0, "a").unwrap();
        lib.update_marker(m1, 5.0, "first").unwrap();
        let ms = lib.markers(a).unwrap();
        assert_eq!(ms[0].name, "first");
        assert!(lib.remove_marker(m1).unwrap());
        assert!(!lib.remove_marker(m1).unwrap());
        lib.set_markers(a, &[(1.0, "x".into()), (2.0, "y".into())])
            .unwrap();
        assert_eq!(lib.markers(a).unwrap().len(), 2);
    }

    #[test]
    fn playback_progress_counts_once_per_session() {
        let lib = lib();
        let mut u = MediaUpsert::new("/a.mp4", "local");
        u.duration = Some(100.0);
        let a = lib.upsert(&u).unwrap().id;
        let s = lib.start_playback(a).unwrap();
        let p = lib.update_playback(s, 50.0, false).unwrap();
        assert_eq!(
            (p.resume_position, p.counted_now, p.play_count),
            (50.0, false, 0)
        );
        assert_eq!(lib.continue_watching(10).unwrap().len(), 1);
        let p = lib.update_playback(s, 95.0, false).unwrap();
        assert!(p.counted_now);
        assert_eq!(p.play_count, 1);
        // Near the end: watched, so it starts over and is no longer
        // "continue watching".
        assert_eq!(p.resume_position, 0.0);
        assert!(lib.continue_watching(10).unwrap().is_empty());
        // Back before the mark, it resumes there again.
        assert_eq!(
            lib.update_playback(s, 60.0, false).unwrap().resume_position,
            60.0
        );
        // Further reports and finishing in the same session do not recount.
        let p = lib.update_playback(s, 100.0, true).unwrap();
        assert_eq!(
            (p.resume_position, p.counted_now, p.play_count),
            (0.0, false, 1)
        );
        // A second session that finishes counts again.
        let s2 = lib.start_playback(a).unwrap();
        assert!(lib.update_playback(s2, 3.0, true).unwrap().counted_now);
        let r = lib.get(a).unwrap().unwrap();
        assert_eq!(r.play_count, 2);
        assert!(r.last_played.is_some());
        let h = lib.history(10).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].id, s2);
        assert!(h.iter().all(|e| e.completed));
        assert!(lib.update_playback(SessionId(999), 1.0, false).is_err());
        lib.set_favorite(a, true).unwrap();
        lib.clear_history().unwrap();
        assert!(lib.history(10).unwrap().is_empty());
        let r = lib.get(a).unwrap().unwrap();
        assert_eq!((r.last_played, r.play_count, r.favorite), (None, 0, true));
    }

    #[test]
    fn removing_one_video_from_history() {
        let lib = lib();
        let mut u = MediaUpsert::new("/a.mp4", "local");
        u.duration = Some(100.0);
        let a = lib.upsert(&u).unwrap().id;
        let b = add(&lib, "/b.mp4");
        for id in [a, b] {
            let s = lib.start_playback(id).unwrap();
            lib.update_playback(s, 40.0, false).unwrap();
        }
        lib.set_rating(a, 4).unwrap();
        lib.remove_from_history(a).unwrap();
        let r = lib.get(a).unwrap().unwrap();
        assert_eq!((r.last_played, r.resume_position, r.rating), (None, 0.0, 4));
        let watched: Vec<MediaId> = lib
            .search(&crate::Query {
                watched_only: true,
                ..Default::default()
            })
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(watched, vec![b]);
        assert!(lib.history(10).unwrap().iter().all(|h| h.media_id == b));
        assert!(lib.remove_from_history(MediaId(999)).is_err());
    }

    #[test]
    fn forgetting_the_unwatched_videos_of_a_source() {
        let lib = lib();
        let local = add(&lib, "/v/local.mp4");
        let up = |loc: &str, src: &str| lib.upsert(&MediaUpsert::new(loc, src)).unwrap().id;
        let plain = up("smb://nas/v/plain.mp4", "nas");
        let watched = up("smb://nas/v/watched.mp4", "nas");
        let fav = up("smb://nas/v/fav.mp4", "nas");
        let tagged = up("smb://nas/v/tagged.mp4", "nas");
        let marked = up("smb://nas/v/marked.mp4", "nas");
        let other = up("http://feed/3", "feed");
        lib.start_playback(watched).unwrap();
        lib.set_favorite(fav, true).unwrap();
        lib.set_tags(tagged, &["keep"]).unwrap();
        lib.add_marker(marked, 3.0, "mine").unwrap();

        assert_eq!(lib.forget_unwatched("nas").unwrap(), 1);
        assert!(lib.get(plain).unwrap().is_none());
        for id in [local, watched, fav, tagged, marked, other] {
            assert!(lib.get(id).unwrap().is_some(), "{id} was deleted");
        }
        assert_eq!(lib.detach_source("nas").unwrap(), 4);
        assert_eq!(lib.get(watched).unwrap().unwrap().source_id, "");
        assert_eq!(lib.get(other).unwrap().unwrap().source_id, "feed");
    }

    #[test]
    fn descriptions_are_kept_and_kept_current() {
        let lib = lib();
        let mut e = Entry::new("Beach", "http://feed/1", EntryKind::Video);
        e.description = Some("Sun and sand.".into());
        let id = lib.upsert_entry("feed", &e).unwrap().id;
        let get = |id| lib.get(id).unwrap().unwrap();
        assert_eq!(get(id).description.as_deref(), Some("Sun and sand."));
        // An entry without one (a listing) keeps what was stored.
        e.description = None;
        lib.upsert_entry("feed", &e).unwrap();
        assert_eq!(get(id).description.as_deref(), Some("Sun and sand."));
        let details = SourceDetails {
            title: Some("Beach day"),
            description: Some("Sun, sand and sea."),
            ..Default::default()
        };
        lib.update_details(id, &details).unwrap();
        let r = get(id);
        assert_eq!(r.title, "Beach day");
        assert_eq!(r.description.as_deref(), Some("Sun, sand and sea."));
        lib.update_details(id, &SourceDetails::default()).unwrap();
        assert_eq!(get(id).title, "Beach day", "nothing given, nothing changed");
    }

    #[test]
    fn a_sources_tags_are_kept_and_locked_ones_left_out_of_the_list() {
        let lib = lib();
        let mut e = Entry::new("Beach", "http://feed/1", EntryKind::Video);
        e.tags = vec!["Outdoor".into(), "POV".into()];
        let id = lib.upsert_entry("feed", &e).unwrap().id;
        assert_eq!(lib.get(id).unwrap().unwrap().tags, ["Outdoor", "POV"]);
        // A listing without them keeps them; new ones replace them.
        e.tags.clear();
        lib.upsert_entry("feed", &e).unwrap();
        assert_eq!(lib.get(id).unwrap().unwrap().tags, ["Outdoor", "POV"]);
        let tags = vec!["Outdoor".to_string(), "Sunset".to_string()];
        let details = SourceDetails {
            tags: Some(&tags),
            ..Default::default()
        };
        lib.update_details(id, &details).unwrap();
        assert_eq!(lib.get(id).unwrap().unwrap().tags, ["Outdoor", "Sunset"]);
        // A scan leaves a file's tags alone.
        let mine = add(&lib, "/v/mine.mp4");
        lib.set_tags(mine, &["Outdoor"]).unwrap();
        lib.upsert(&MediaUpsert::new("/v/mine.mp4", "local"))
            .unwrap();
        assert_eq!(lib.get(mine).unwrap().unwrap().tags, ["Outdoor"]);

        let counts = |lib: &Library| -> Vec<(String, usize)> {
            lib.all_tags()
                .unwrap()
                .into_iter()
                .map(|t| (t.name, t.count))
                .collect()
        };
        assert_eq!(
            counts(&lib),
            [("Outdoor".to_string(), 2), ("Sunset".to_string(), 1)]
        );
        lib.set_hidden(Hidden {
            sources: vec!["feed".into()],
        });
        assert_eq!(counts(&lib), [("Outdoor".to_string(), 1)]);
    }

    #[test]
    fn videos_on_cards_that_are_out_wait_for_their_thumbnails() {
        let lib = lib();
        // No card has this ID.
        add(&lib, "volume:0000-0000/Clips/a.mp4");
        let here = add(&lib, "/v/b.mp4");
        let pending = lib.pending_metadata(10, true).unwrap();
        assert_eq!(pending.iter().map(|r| r.id).collect::<Vec<_>>(), [here]);
        assert_eq!(lib.pending_metadata_count(true).unwrap(), 1);
    }

    #[test]
    fn hidden_sources_are_left_out_of_listings() {
        let lib = lib();
        let mine = add(&lib, "/mine.mp4");
        let nas = lib
            .upsert(&MediaUpsert::new("smb://nas/v/x.mp4", "nas"))
            .unwrap()
            .id;
        for id in [mine, nas] {
            let s = lib.start_playback(id).unwrap();
            lib.update_playback(s, 5.0, false).unwrap();
        }
        let ids = |r: Vec<MediaRecord>| r.into_iter().map(|r| r.id).collect::<Vec<_>>();
        let all = crate::Query::default();
        assert_eq!(lib.count(&all).unwrap(), 2);

        lib.set_hidden(Hidden {
            sources: vec!["nas".into()],
        });
        assert_eq!(ids(lib.search(&all).unwrap()), vec![mine]);
        assert_eq!(lib.count(&all).unwrap(), 1);
        assert_eq!(lib.count(&crate::Query::text("x")).unwrap(), 0);
        assert_eq!(ids(lib.continue_watching(10).unwrap()), vec![mine]);
        assert_eq!(ids(lib.recently_added(10).unwrap()), vec![mine]);
        let history: Vec<MediaId> = lib
            .history(10)
            .unwrap()
            .iter()
            .map(|h| h.media_id)
            .collect();
        assert_eq!(history, vec![mine]);
        assert!(lib.sources().unwrap().iter().all(|(s, _)| s != "nas"));
        // Still reachable directly (what is playing keeps saving progress).
        assert!(lib.get(nas).unwrap().is_some());
        // Clones share the setting.
        assert_eq!(lib.clone().count(&all).unwrap(), 1);

        lib.set_hidden(Hidden::default());
        assert_eq!(lib.count(&all).unwrap(), 2);
        assert_eq!(lib.history(10).unwrap().len(), 2);
        assert_eq!(lib.continue_watching(10).unwrap().len(), 2);
    }

    #[test]
    fn continue_watching_and_recently_added_order() {
        let lib = lib();
        let a = add(&lib, "/a.mp4");
        let b = add(&lib, "/b.mp4");
        let c = add(&lib, "/c.mp4");
        // Unknown duration still counts as resumable.
        let sa = lib.start_playback(a).unwrap();
        lib.update_playback(sa, 10.0, false).unwrap();
        let sb = lib.start_playback(b).unwrap();
        lib.update_playback(sb, 20.0, false).unwrap();
        // Same second: id breaks the tie, newest first.
        let ids: Vec<MediaId> = lib
            .continue_watching(10)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        assert!(ids.contains(&a) && ids.contains(&b) && !ids.contains(&c));
        let recent: Vec<MediaId> = lib
            .recently_added(2)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(recent, vec![c, b]);
    }

    #[test]
    fn remove_and_purge() {
        let lib = lib();
        let a = add(&lib, "/a.mp4");
        let b = add(&lib, "/b.mp4");
        lib.set_tags(a, &["t"]).unwrap();
        lib.remove_media(a).unwrap();
        assert!(lib.get(a).unwrap().is_none());
        assert!(lib.all_tags().unwrap().is_empty());
        lib.with(|c| {
            c.execute("UPDATE media SET missing = 1 WHERE id = ?1", [b.0])?;
            Ok(())
        })
        .unwrap();
        assert_eq!(lib.purge_missing(Some("other")).unwrap(), 0);
        assert_eq!(lib.purge_missing(None).unwrap(), 1);
        assert_eq!(lib.sources().unwrap(), vec![]);
    }

    #[test]
    fn shared_between_threads() {
        let lib = lib();
        let handles: Vec<_> = (0..4)
            .map(|t| {
                let lib = lib.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        let id = lib
                            .upsert(&MediaUpsert::new(format!("/t{t}/v{i}.mp4"), "local"))
                            .unwrap()
                            .id;
                        lib.set_rating(id, (i % 6) as u8).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(lib.sources().unwrap(), vec![("local".to_string(), 100)]);
    }
}
