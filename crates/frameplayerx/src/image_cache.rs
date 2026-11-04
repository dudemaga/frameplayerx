//! Pictures from servers kept on the headset (`~/.cache/frameplayerx/images`)
//! up to the size set in Settings › General, so they are not downloaded
//! again on every launch. The least recently used go first.
//!
//! A picture its server marks immutable (Jellyfin's, whose address names the
//! image's version) is never checked again; any other is checked once a day
//! with a conditional request and replaced when it changed.

use fp_core::sync::lock;
use fp_sources::http::HttpClient;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a picture is shown before it is checked with its server again.
const RECHECK: Duration = Duration::from_secs(24 * 3600);
/// Largest picture taken.
const MAX_BYTES: u64 = 16 << 20;
/// How often a picture's file records that it was used, which orders
/// eviction after a restart.
const TOUCH_EVERY: Duration = Duration::from_secs(3600);

pub struct ImageCache {
    dir: PathBuf,
    opener: Option<std::sync::Arc<crate::services::Opener>>,
    http: HttpClient,
    state: Mutex<State>,
    /// Signalled when a download ends, for threads waiting on the same
    /// picture.
    fetched: Condvar,
}

#[derive(Default)]
struct State {
    limit: u64,
    /// What the folder holds, by key; read on first use.
    files: Option<HashMap<String, Held>>,
    total: u64,
    /// Pictures being downloaded.
    fetching: HashSet<String>,
}

#[derive(Clone, Copy)]
struct Held {
    size: u64,
    used: SystemTime,
    /// When `used` was last written to the file.
    touched: SystemTime,
}

/// What is known about a cached picture, kept beside it as `<key>.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Meta {
    /// Its address, without credentials.
    url: String,
    etag: Option<String>,
    last_modified: Option<String>,
    /// Unix seconds of the last download or check.
    checked: u64,
    /// The server said it never changes.
    immutable: bool,
}

impl Meta {
    fn due(&self) -> bool {
        !self.immutable && unix_now().saturating_sub(self.checked) >= RECHECK.as_secs()
    }
}

/// Removes a download claim when it ends, however it ends.
struct Claim<'a> {
    cache: &'a ImageCache,
    key: String,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        lock(&self.cache.state).fetching.remove(&self.key);
        self.cache.fetched.notify_all();
    }
}

impl ImageCache {
    pub fn new(dir: PathBuf, limit_mb: u32) -> ImageCache {
        ImageCache::with_limit(dir, mb(limit_mb))
    }

    fn with_limit(dir: PathBuf, limit: u64) -> ImageCache {
        ImageCache {
            dir,
            opener: None,
            http: HttpClient::new(Default::default(), None),
            state: Mutex::new(State {
                limit,
                ..Default::default()
            }),
            fetched: Condvar::new(),
        }
    }

    pub fn with_opener(mut self, opener: std::sync::Arc<crate::services::Opener>) -> Self {
        self.opener = Some(opener);
        self
    }

    pub fn set_limit(&self, limit_mb: u32) {
        let mut st = self.state();
        st.limit = mb(limit_mb);
        self.evict(&mut st, None);
    }

    /// The picture at `url` and whether it is due a check with its server
    /// ([`Self::refresh`]): from the cache, else downloaded into it.
    pub fn get(&self, url: &str) -> Result<(Vec<u8>, bool), String> {
        let (key, plain) = key(url);
        let _claim = loop {
            let mut st = self.state();
            while st.fetching.contains(&key) {
                st = self
                    .fetched
                    .wait(st)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if !st.files.as_ref().is_some_and(|f| f.contains_key(&key)) {
                st.fetching.insert(key.clone());
                break Claim {
                    cache: self,
                    key: key.clone(),
                };
            }
            drop(st);
            if let Some((bytes, meta)) = self.read(&key, &plain) {
                return Ok((bytes, meta.due()));
            }
            // Unreadable, or another address under the same key.
            self.remove(&mut self.state(), &key);
        };
        let bytes = self.download(url, &key, &plain, None)?;
        Ok((bytes.unwrap_or_default(), false))
    }

    /// Asks the server whether the cached picture at `url` changed, when it
    /// is due; the new picture when it did.
    pub fn refresh(&self, url: &str) -> Result<Option<Vec<u8>>, String> {
        let (key, plain) = key(url);
        match self.read_meta(&key) {
            Some(meta) if meta.url == plain && meta.due() => {
                self.download(url, &key, &plain, Some(&meta))
            }
            _ => Ok(None),
        }
    }

    /// Deletes every cached picture and returns the bytes freed.
    pub fn clear(&self) -> u64 {
        let mut st = self.state();
        let freed = st.total;
        let keys: Vec<String> = st.files.iter().flat_map(|f| f.keys().cloned()).collect();
        for k in keys {
            self.remove(&mut st, &k);
        }
        freed
    }

    /// The cached file of `url` if it is already on the headset; never
    /// downloads, so the frame loop can ask.
    pub fn cached_file(&self, url: &str) -> Option<PathBuf> {
        let path = self.path(&key(url).0, "img");
        path.exists().then_some(path)
    }

    /// The state, with the folder read on first use.
    fn state(&self) -> MutexGuard<'_, State> {
        let mut st = lock(&self.state);
        if st.files.is_none() {
            let (files, total) = scan(&self.dir);
            st.files = Some(files);
            st.total = total;
            self.evict(&mut st, None);
        }
        st
    }

    fn path(&self, key: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{key}.{ext}"))
    }

    fn read_meta(&self, key: &str) -> Option<Meta> {
        let text = std::fs::read_to_string(self.path(key, "json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// The cached picture of `plain` and what is known about it, marking it
    /// used.
    fn read(&self, key: &str, plain: &str) -> Option<(Vec<u8>, Meta)> {
        let meta = self.read_meta(key).filter(|m| m.url == plain)?;
        let bytes = std::fs::read(self.path(key, "img")).ok()?;
        let mut st = self.state();
        if let Some(h) = st.files.as_mut().and_then(|f| f.get_mut(key)) {
            let now = SystemTime::now();
            h.used = now;
            if now.duration_since(h.touched).unwrap_or_default() >= TOUCH_EVERY {
                h.touched = now;
                let _ = std::fs::File::options()
                    .write(true)
                    .open(self.path(key, "img"))
                    .and_then(|f| f.set_modified(now));
            }
        }
        Some((bytes, meta))
    }

    /// Downloads `url`, conditionally when `old` says what is cached: `None`
    /// when it is unchanged.
    fn download(
        &self,
        url: &str,
        key: &str,
        plain: &str,
        old: Option<&Meta>,
    ) -> Result<Option<Vec<u8>>, String> {
        let owned = fp_sources::is_source_location(url);
        let request = if owned {
            self.opener.as_ref().and_then(|o| o.picture_request(url))
        } else {
            None
        };
        if owned && request.is_none() {
            let bytes = self
                .opener
                .as_ref()
                .ok_or_else(|| "source unavailable".to_string())?
                .fetch(url, MAX_BYTES)?;
            let meta = Meta {
                url: plain.to_string(),
                checked: unix_now(),
                ..Default::default()
            };
            return Ok(self.keep(key, plain, old, bytes, &meta));
        }
        let (client, address) = request.unwrap_or_else(|| (self.http.clone(), url.to_string()));
        let mut headers = Vec::new();
        if let Some(m) = old {
            if let Some(e) = &m.etag {
                headers.push(("If-None-Match", e.as_str()));
            }
            if let Some(d) = &m.last_modified {
                headers.push(("If-Modified-Since", d.as_str()));
            }
        }
        let reply = client
            .request("GET", &address, &headers, None, MAX_BYTES + 1)
            .map_err(|e| e.to_string())?;
        if reply.status == 304
            && let Some(m) = old
        {
            let meta = Meta {
                checked: unix_now(),
                ..m.clone()
            };
            if let Err(e) = self.write_meta(key, &meta) {
                log::debug!("image cache {plain}: {e}");
            }
            return Ok(None);
        }
        let reply = reply.error_for_status(url).map_err(|e| e.to_string())?;
        // The client stops reading at its limit: kept, it would pass for whole.
        if reply.body.len() as u64 > MAX_BYTES {
            let mb = MAX_BYTES >> 20;
            return Err(tr!("error-too-large", mb = mb));
        }
        let meta = Meta {
            url: plain.to_string(),
            etag: reply.header("etag").map(str::to_string),
            last_modified: reply.header("last-modified").map(str::to_string),
            checked: unix_now(),
            immutable: reply
                .header("cache-control")
                .is_some_and(|c| c.to_ascii_lowercase().contains("immutable")),
        };
        Ok(self.keep(key, plain, old, reply.body, &meta))
    }

    /// Stores a picture just downloaded: `None` when a check found the one
    /// cached unchanged, as a server without validators can only show by
    /// sending it, so that it isn't decoded and drawn again.
    fn keep(
        &self,
        key: &str,
        plain: &str,
        old: Option<&Meta>,
        body: Vec<u8>,
        meta: &Meta,
    ) -> Option<Vec<u8>> {
        let unchanged =
            old.is_some() && std::fs::read(self.path(key, "img")).is_ok_and(|held| held == body);
        // Shown all the same when the folder can't take it.
        let stored = if unchanged {
            self.write_meta(key, meta)
        } else {
            self.store(key, &body, meta)
        };
        if let Err(e) = stored {
            log::debug!("image cache {plain}: {e}");
        }
        (!unchanged).then_some(body)
    }

    fn write_meta(&self, key: &str, meta: &Meta) -> std::io::Result<()> {
        let json = serde_json::to_string(meta).map_err(std::io::Error::other)?;
        write_atomic(&self.path(key, "json"), json.as_bytes())
    }

    fn store(&self, key: &str, body: &[u8], meta: &Meta) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        write_atomic(&self.path(key, "img"), body)?;
        self.write_meta(key, meta)?;
        let mut st = self.state();
        let st = &mut *st;
        let now = SystemTime::now();
        let size = body.len() as u64;
        let held = Held {
            size,
            used: now,
            touched: now,
        };
        let old = st
            .files
            .get_or_insert_default()
            .insert(key.to_string(), held);
        st.total = st.total - old.map_or(0, |h| h.size) + size;
        self.evict(st, Some(key));
        Ok(())
    }

    /// Deletes the least recently used pictures, all but `keep`, until the
    /// rest fit the limit.
    fn evict(&self, st: &mut State, keep: Option<&str>) {
        while st.total > st.limit {
            let Some(oldest) = st.files.as_ref().and_then(|f| {
                f.iter()
                    .filter(|(k, _)| Some(k.as_str()) != keep)
                    .min_by_key(|(_, h)| h.used)
                    .map(|(k, _)| k.clone())
            }) else {
                break;
            };
            self.remove(st, &oldest);
        }
    }

    fn remove(&self, st: &mut State, key: &str) {
        if let Some(h) = st.files.as_mut().and_then(|f| f.remove(key)) {
            st.total = st.total.saturating_sub(h.size);
        }
        let _ = std::fs::remove_file(self.path(key, "img"));
        let _ = std::fs::remove_file(self.path(key, "json"));
    }
}

fn mb(n: u32) -> u64 {
    u64::from(n) << 20
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

/// The cache key of `url` and the address it stands for, without
/// credentials (they stay out of the folder).
fn key(url: &str) -> (String, String) {
    let plain =
        fp_sources::urlutil::split_credentials(url).map_or_else(|_| url.to_string(), |(u, _)| u);
    // FNV-1a: stable across builds, unlike the standard library's hasher.
    let hash = plain.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    (format!("{hash:016x}"), plain)
}

/// The pictures in `dir` and their total size; leftovers of interrupted
/// writes are deleted.
fn scan(dir: &Path) -> (HashMap<String, Held>, u64) {
    let mut files = HashMap::new();
    let mut total = 0;
    let mut metas = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = e.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        match path.extension().and_then(|s| s.to_str()) {
            Some("img") => {
                let Ok(md) = e.metadata() else { continue };
                let used = md.modified().unwrap_or(UNIX_EPOCH);
                let held = Held {
                    size: md.len(),
                    used,
                    touched: used,
                };
                total += held.size;
                files.insert(stem.to_string(), held);
            }
            Some("json") => metas.push((stem.to_string(), path)),
            _ => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    for (stem, path) in metas {
        if !files.contains_key(&stem) {
            let _ = std::fs::remove_file(path);
        }
    }
    (files, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A server for `/a`, `/b`, `/c` (10 bytes each, ETag `"v1"`) and
    /// `/forever` (immutable). It answers `If-None-Match: "v1"` with 304
    /// until `changed`, then sends `/a` anew. Counts the requests it gets.
    struct Server {
        base: String,
        hits: Arc<AtomicUsize>,
        changed: Arc<std::sync::atomic::AtomicBool>,
    }

    fn server() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (h, c) = (hits.clone(), changed.clone());
        let huge = vec![0u8; MAX_BYTES as usize + 1];
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                h.fetch_add(1, Ordering::SeqCst);
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split(' ').nth(1).unwrap_or("/").to_string();
                let mut etag_matches = false;
                loop {
                    let mut l = String::new();
                    if reader.read_line(&mut l).unwrap() == 0 || l == "\r\n" {
                        break;
                    }
                    let l = l.to_ascii_lowercase();
                    etag_matches |= l.starts_with("if-none-match:") && l.contains("\"v1\"");
                }
                let changed = c.load(Ordering::SeqCst);
                let (status, etag, body): (&str, &str, &[u8]) = if path == "/huge" {
                    ("200 OK", "\"v1\"", &huge)
                } else if etag_matches && !changed {
                    ("304 Not Modified", "\"v1\"", b"")
                } else if changed && path == "/a" {
                    ("200 OK", "\"v2\"", b"AAAAANEWER")
                } else {
                    ("200 OK", "\"v1\"", b"0123456789")
                };
                let cache = if path == "/forever" {
                    "public, max-age=31536000, immutable"
                } else {
                    "public"
                };
                let mut s = stream;
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nETag: {etag}\r\nCache-Control: {cache}\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(body);
            }
        });
        Server {
            base,
            hits,
            changed,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fp-images-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Makes `url`'s cached copy a day and a bit old.
    fn age(cache: &ImageCache, url: &str) {
        let k = key(url).0;
        let mut m = cache.read_meta(&k).unwrap();
        m.checked -= RECHECK.as_secs() + 60;
        cache.write_meta(&k, &m).unwrap();
    }

    #[test]
    fn persistent_artwork_uses_its_owning_source_and_keeps_a_stable_cache_key() {
        let server = server();
        let opener = Arc::new(crate::services::Opener::new());
        assert!(
            opener
                .set_sources(&[fp_sources::SourceConfig::Http(fp_sources::HttpConfig {
                    id: "pictures".into(),
                    name: "Pictures".into(),
                    url: server.base.clone(),
                    credentials: None,
                    insecure_tls: false,
                    parallel_downloads: false,
                })])
                .is_empty()
        );
        let dir = temp_dir("owned");
        let cache = ImageCache::new(dir.clone(), 500).with_opener(opener);
        let location = fp_sources::location::encode("pictures", "http", "a");
        assert_eq!(cache.get(&location).unwrap().0, b"0123456789");
        let hits = server.hits.load(Ordering::SeqCst);
        assert_eq!(cache.get(&location).unwrap().0, b"0123456789");
        assert_eq!(server.hits.load(Ordering::SeqCst), hits);
        assert!(
            cache
                .read_meta(&key(&location).0)
                .unwrap()
                .url
                .starts_with("source://")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn picture_source(s: &Server) -> Arc<crate::services::Opener> {
        let opener = Arc::new(crate::services::Opener::new());
        assert!(
            opener
                .set_sources(&[fp_sources::SourceConfig::Http(fp_sources::HttpConfig {
                    id: "pictures".into(),
                    name: "Pictures".into(),
                    url: s.base.clone(),
                    credentials: None,
                    insecure_tls: false,
                    parallel_downloads: false,
                })])
                .is_empty()
        );
        opener
    }

    #[test]
    fn a_sources_picture_is_checked_with_its_server_not_downloaded_again() {
        let s = server();
        let dir = temp_dir("source-checked");
        let cache = ImageCache::new(dir.clone(), 500).with_opener(picture_source(&s));
        let a = fp_sources::location::encode("pictures", "http", "a");
        assert_eq!(cache.get(&a).unwrap(), (b"0123456789".to_vec(), false));
        let k = key(&a).0;
        assert_eq!(cache.read_meta(&k).unwrap().etag.as_deref(), Some("\"v1\""));

        age(&cache, &a);
        assert!(cache.get(&a).unwrap().1);
        assert_eq!(
            cache.refresh(&a).unwrap(),
            None,
            "the server answered 304 to its ETag"
        );
        assert!(!cache.get(&a).unwrap().1, "checked just now");

        age(&cache, &a);
        s.changed.store(true, Ordering::SeqCst);
        assert_eq!(cache.refresh(&a).unwrap(), Some(b"AAAAANEWER".to_vec()));
        assert_eq!(cache.get(&a).unwrap().0, b"AAAAANEWER");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_picture_too_large_is_refused_and_not_kept_cut_off() {
        let s = server();
        let dir = temp_dir("too-large");
        let cache = ImageCache::new(dir.clone(), 500).with_opener(picture_source(&s));
        for url in [
            format!("{}/huge", s.base),
            fp_sources::location::encode("pictures", "http", "huge"),
        ] {
            assert!(cache.get(&url).is_err(), "{url}");
            assert!(cache.cached_file(&url).is_none(), "{url}");
        }
        assert!(!dir.exists(), "nothing was stored");
    }

    #[test]
    fn a_sources_immutable_picture_is_never_checked_again() {
        let s = server();
        let dir = temp_dir("source-immutable");
        let cache = ImageCache::new(dir.clone(), 500).with_opener(picture_source(&s));
        let forever = fp_sources::location::encode("pictures", "http", "forever");
        cache.get(&forever).unwrap();
        assert!(cache.read_meta(&key(&forever).0).unwrap().immutable);
        age(&cache, &forever);
        let hits = s.hits.load(Ordering::SeqCst);
        assert!(!cache.get(&forever).unwrap().1, "not due, however old");
        assert_eq!(cache.refresh(&forever).unwrap(), None);
        assert_eq!(s.hits.load(Ordering::SeqCst), hits);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_check_that_finds_the_same_picture_does_not_draw_it_again() {
        // A source not over HTTP can only send the picture again.
        struct Files(PathBuf);
        impl fp_sources::Source for Files {
            fn id(&self) -> &str {
                "files"
            }
            fn name(&self) -> &str {
                "Files"
            }
            fn kind(&self) -> fp_sources::SourceKind {
                fp_sources::SourceKind::Smb
            }
            fn describe(&self) -> String {
                "files".into()
            }
            fn list(&self, _: Option<&str>) -> fp_sources::Result<Vec<fp_core::source::Entry>> {
                Ok(Vec::new())
            }
            fn open(&self, _: &str) -> fp_sources::Result<Arc<dyn fp_core::ByteSource>> {
                Ok(Arc::new(fp_core::source::FileSource::open(&self.0)?))
            }
        }
        let dir = temp_dir("same-picture");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("picture");
        std::fs::write(&file, b"same").unwrap();
        let opener = Arc::new(crate::services::Opener::new());
        opener.insert_source(Arc::new(Files(file.clone())));
        let cache = ImageCache::new(dir.join("cache"), 500).with_opener(opener);
        let location = fp_sources::location::encode("files", "smb", "picture");
        assert_eq!(cache.get(&location).unwrap().0, b"same");
        age(&cache, &location);
        assert_eq!(cache.refresh(&location).unwrap(), None);
        assert!(!cache.get(&location).unwrap().1, "checked just now");
        age(&cache, &location);
        std::fs::write(&file, b"new!").unwrap();
        assert_eq!(cache.refresh(&location).unwrap(), Some(b"new!".to_vec()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pictures_are_downloaded_once_and_checked_when_due() {
        let s = server();
        let dir = temp_dir("once");
        let cache = ImageCache::new(dir.clone(), 500);
        let a = format!("{}/a", s.base);
        assert_eq!(cache.get(&a).unwrap(), (b"0123456789".to_vec(), false));
        assert_eq!(cache.get(&a).unwrap(), (b"0123456789".to_vec(), false));
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
        assert_eq!(cache.refresh(&a).unwrap(), None, "not due");
        // After a restart it is still there.
        let cache = ImageCache::new(dir.clone(), 500);
        assert!(!cache.get(&a).unwrap().1);
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);

        // A day on: due, and unchanged on the server.
        age(&cache, &a);
        assert!(cache.get(&a).unwrap().1);
        assert_eq!(cache.refresh(&a).unwrap(), None);
        assert_eq!(s.hits.load(Ordering::SeqCst), 2);
        assert!(!cache.get(&a).unwrap().1, "checked just now");
        // A day on again, and changed: the new picture replaces the old.
        age(&cache, &a);
        s.changed.store(true, Ordering::SeqCst);
        assert_eq!(cache.refresh(&a).unwrap(), Some(b"AAAAANEWER".to_vec()));
        assert_eq!(cache.get(&a).unwrap().0, b"AAAAANEWER");

        // One the server says never changes is never due.
        let forever = format!("{}/forever", s.base);
        cache.get(&forever).unwrap();
        age(&cache, &forever);
        assert!(!cache.get(&forever).unwrap().1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_least_recently_used_go_when_full() {
        let s = server();
        let dir = temp_dir("full");
        let cache = ImageCache::with_limit(dir.clone(), 25);
        let url = |p: &str| format!("{}/{p}", s.base);
        cache.get(&url("a")).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        cache.get(&url("b")).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        // `a` is used again, so `b` is the oldest when `c` arrives.
        cache.get(&url("a")).unwrap();
        cache.get(&url("c")).unwrap();
        let held = |p: &str| cache.path(&key(&url(p)).0, "img").exists();
        assert!(held("a") && !held("b") && held("c"));
        assert_eq!(lock(&cache.state).total, 20);
        // A smaller limit takes effect at once.
        cache.set_limit(0);
        assert_eq!(lock(&cache.state).total, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clearing_deletes_every_picture() {
        let s = server();
        let dir = temp_dir("clear");
        let cache = ImageCache::new(dir.clone(), 500);
        let (a, b) = (format!("{}/a", s.base), format!("{}/b", s.base));
        cache.get(&a).unwrap();
        cache.get(&b).unwrap();
        assert_eq!(cache.clear(), 20);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        assert_eq!(lock(&cache.state).total, 0);
        // Asked for again, it is downloaded again.
        cache.get(&a).unwrap();
        assert_eq!(s.hits.load(Ordering::SeqCst), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn credentials_stay_out_of_the_folder() {
        let (k1, plain) = key("http://bob:secret@nas:8080/a.jpg");
        assert_eq!(plain, "http://nas:8080/a.jpg");
        assert_eq!(k1, key("http://nas:8080/a.jpg").0);
        assert_ne!(k1, key("http://nas:8080/b.jpg").0);
    }
}
