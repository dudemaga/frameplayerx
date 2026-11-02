//! SMB2/3 shares (Windows, Samba, NAS boxes).
//!
//! # Implementation choice
//!
//! This uses [`smb2`](https://crates.io/crates/smb2), a pure-Rust SMB
//! 2.0.2-3.1.1 client with NTLM authentication, signing and encryption. It
//! is async, so the blocking calls here run it on a small tokio runtime of
//! its own. It cross-compiles for `aarch64-unknown-linux-gnu` with `cargo
//! zigbuild` without any system library. Build with `--no-default-features`
//! to leave SMB out; [`SmbSource::new`] then fails with
//! [`Error::SmbUnavailable`].
//!
//! A source is one share (`smb://host/share[/folder]`) or a whole server
//! (`smb://host/`), whose root lists its shares.
//!
//! Listed locations retain the source ID and relative share/path; the SMB
//! host comes from the current configuration. Files are read through the
//! shared block cache with read-ahead.

use crate::config::{SmbConfig, SourceKind};
use crate::error::{Error, Result};
use crate::urlutil::{encode_segment, percent_decode};
use crate::{Source, dir_entry, file_entry, sort_entries};
use fp_core::source::{ByteSource, Entry};
use std::sync::Arc;

/// Builds `smb://host[:port]/share/path` with every segment
/// percent-encoded. `path` is `/`-separated and may be empty.
pub fn smb_url(host: &str, port: Option<u16>, share: &str, path: &str) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]") // IPv6 literal
    } else {
        host.to_string()
    };
    let mut out = format!("smb://{host}");
    if let Some(p) = port.filter(|p| *p != 445) {
        out.push_str(&format!(":{p}"));
    }
    out.push('/');
    out.push_str(&encode_segment(share));
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        out.push_str(&encode_segment(seg));
    }
    out
}

/// A location split into share and decoded path segments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmbLocation {
    /// Host name or address (IPv6 without brackets).
    pub host: String,
    /// TCP port, when given.
    pub port: Option<u16>,
    /// Empty for the server itself (`smb://host/`), which lists its shares.
    pub share: String,
    /// Decoded path segments inside the share.
    pub path: Vec<String>,
}

impl SmbLocation {
    /// Parses `smb://host[:port]/share/path`, or `smb://host[:port]/` for
    /// the server. Credentials in the URL are ignored (they belong in the
    /// configuration).
    pub fn parse(location: &str) -> Result<SmbLocation> {
        let rest = location
            .get(..6)
            .filter(|p| p.eq_ignore_ascii_case("smb://"))
            .map(|_| &location[6..])
            .ok_or_else(|| Error::invalid(location, "expected an smb:// URL"))?;
        let rest = rest.split(['?', '#']).next().unwrap_or("");
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, after) = v6
                .split_once(']')
                .ok_or_else(|| Error::invalid(location, "bad IPv6 host"))?;
            (h.to_string(), after.strip_prefix(':'))
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p)),
                None => (authority.to_string(), None),
            }
        };
        let port = match port {
            Some(p) => Some(
                p.parse::<u16>()
                    .map_err(|_| Error::invalid(location, "bad port"))?,
            ),
            None => None,
        };
        let mut segs = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(percent_decode);
        let share = segs.next().unwrap_or_default();
        let path: Vec<String> = segs.collect();
        if host.is_empty() {
            return Err(Error::invalid(location, "no host"));
        }
        if path.iter().any(|s| s == "..") {
            return Err(Error::invalid(location, "path escapes the share"));
        }
        Ok(SmbLocation {
            host,
            port,
            share,
            path,
        })
    }

    /// The server itself, not a share on it.
    pub fn is_server(&self) -> bool {
        self.share.is_empty()
    }

    /// Back to an `smb://` URL.
    pub fn to_url(&self) -> String {
        smb_url(&self.host, self.port, &self.share, &self.path.join("/"))
    }

    /// Path inside the share, `/`-separated (empty for the share root).
    pub fn share_path(&self) -> String {
        self.path.join("/")
    }
}

/// An SMB2/3 share, or every share on a server.
pub struct SmbSource {
    id: String,
    name: String,
    config: SmbConfig,
    root: String,
    #[cfg(feature = "smb")]
    sessions: Arc<imp::Sessions>,
}

impl std::fmt::Debug for SmbSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmbSource")
            .field("id", &self.id)
            .field("root", &self.root)
            .field("credentials", &self.config.credentials)
            .finish_non_exhaustive()
    }
}

impl SmbSource {
    /// Creates the source. No connection is made until it is used.
    pub fn new(config: &SmbConfig) -> Result<SmbSource> {
        if config.host.trim().is_empty() {
            return Err(Error::Config("SMB source needs a host".into()));
        }
        if !cfg!(feature = "smb") {
            return Err(Error::SmbUnavailable(
                "this build of FramePlayerX was compiled without SMB support".into(),
            ));
        }
        let root = smb_url(&config.host, config.port, &config.share, &config.path);
        Ok(SmbSource {
            id: config.id.clone(),
            name: config.name.clone(),
            #[cfg(feature = "smb")]
            sessions: Arc::new(imp::Sessions::new(config, &root)),
            root,
            config: config.clone(),
        })
    }

    /// URL listed by `list(None)`.
    pub fn root(&self) -> &str {
        &self.root
    }

    /// A source for the whole server (`smb://host/`).
    fn whole_server(&self) -> bool {
        self.config.share.trim().is_empty()
    }

    fn check_same_share(&self, loc: &SmbLocation) -> Result<()> {
        let same = loc.host.eq_ignore_ascii_case(&self.config.host)
            && (self.whole_server() || loc.share.eq_ignore_ascii_case(&self.config.share))
            && loc.port.unwrap_or(445) == self.config.port.unwrap_or(445);
        if same {
            Ok(())
        } else {
            Err(Error::invalid(&loc.to_url(), "not on this source's share"))
        }
    }

    fn current_url(&self, value: &str) -> Result<String> {
        if !crate::is_source_location(value) {
            return Ok(value.to_string());
        }
        let value = crate::location::decode(value)
            .filter(|l| l.source_id == self.id && l.kind == "smb")
            .ok_or_else(|| Error::invalid(value, "not owned by this SMB source"))?;
        if value.key.starts_with('/') || value.key.contains(['?', '#', '\\']) {
            return Err(Error::invalid(
                "SMB source resource",
                "invalid relative path",
            ));
        }
        let relative = SmbLocation::parse(&format!("smb://relative/{}", value.key))?;
        if matches!(relative.share.as_str(), "." | "..")
            || relative.share.contains(['/', '\\', '\0'])
            || relative
                .path
                .iter()
                .any(|s| matches!(s.as_str(), "." | "..") || s.contains(['/', '\\', '\0']))
        {
            return Err(Error::invalid(
                "SMB source resource",
                "invalid path segment",
            ));
        }
        let mut current = SmbLocation::parse(&self.root)?;
        if self.whole_server() {
            current.share = relative.share;
            current.path = relative.path;
        } else {
            if !relative.share.is_empty() {
                current.path.push(relative.share);
            }
            current.path.extend(relative.path);
        }
        let mut url = current.to_url();
        if value.key.ends_with('/') && !url.ends_with('/') {
            url.push('/');
        }
        Ok(url)
    }
}

impl Source for SmbSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> SourceKind {
        SourceKind::Smb
    }

    fn describe(&self) -> String {
        let what = if self.whole_server() {
            "SMB server"
        } else {
            "SMB share"
        };
        match &self.config.credentials {
            Some(c) => format!("{what} {} as {}", self.root, c.username),
            None => format!("{what} {} (guest)", self.root),
        }
    }

    fn canonical_location(&self, value: &str) -> Option<String> {
        let url = self.current_url(value).ok()?;
        let loc = SmbLocation::parse(&url).ok()?;
        self.check_same_share(&loc).ok()?;
        let root = SmbLocation::parse(&self.root).ok()?;
        let mut parts = Vec::new();
        if self.whole_server() {
            if !loc.share.is_empty() {
                parts.push(loc.share.as_str());
            }
            parts.extend(loc.path.iter().map(String::as_str));
        } else {
            parts.extend(
                loc.path
                    .strip_prefix(root.path.as_slice())?
                    .iter()
                    .map(String::as_str),
            );
        }
        if parts
            .iter()
            .any(|s| s.is_empty() || matches!(*s, "." | "..") || s.contains(['/', '\\', '\0']))
        {
            return None;
        }
        let mut key = parts
            .into_iter()
            .map(encode_segment)
            .collect::<Vec<_>>()
            .join("/");
        if url.ends_with('/') && !key.is_empty() {
            key.push('/');
        }
        Some(crate::location::encode(&self.id, "smb", &key))
    }

    fn list(&self, location: Option<&str>) -> Result<Vec<Entry>> {
        let loc = SmbLocation::parse(&self.current_url(location.unwrap_or(&self.root))?)?;
        self.check_same_share(&loc)?;
        if loc.is_server() {
            let mut out: Vec<Entry> = self
                .list_shares()?
                .into_iter()
                .filter(|share| self.can_open_share(share))
                .map(|share| {
                    let url = smb_url(&loc.host, loc.port, &share, "");
                    let location = self.canonical_location(&format!("{url}/")).unwrap_or(url);
                    dir_entry(share, location, None)
                })
                .collect();
            sort_entries(&mut out);
            return Ok(out);
        }
        let mut out = Vec::new();
        for item in self.list_dir(&loc)? {
            if item.name.starts_with('.') {
                continue;
            }
            let mut child = loc.clone();
            child.path.push(item.name.clone());
            let url = child.to_url();
            let url = if item.is_dir { format!("{url}/") } else { url };
            let url = self.canonical_location(&url).unwrap_or(url);
            out.push(if item.is_dir {
                dir_entry(item.name, url, item.modified)
            } else {
                file_entry(item.name, url, Some(item.size), item.modified)
            });
        }
        sort_entries(&mut out);
        Ok(out)
    }

    fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>> {
        let loc = SmbLocation::parse(&self.current_url(location)?)?;
        self.check_same_share(&loc)?;
        if loc.path.is_empty() {
            return Err(Error::invalid(
                location,
                "a share or server cannot be opened as a file",
            ));
        }
        self.open_file(&loc)
    }

    fn parent(&self, location: &str) -> Option<String> {
        let mut loc = SmbLocation::parse(&self.current_url(location).ok()?).ok()?;
        if loc.path.pop().is_none() {
            // A share's root goes up to the server's list of shares, when
            // the source is the whole server.
            if loc.is_server() || !self.whole_server() {
                return None;
            }
            loc.share.clear();
            return self.canonical_location(&loc.to_url());
        }
        self.canonical_location(&format!("{}/", loc.to_url()))
    }

    fn sidecars(&self, video: &Entry) -> Result<crate::Sidecars> {
        let mut video = video.clone();
        video.location = self.current_url(&video.location)?;
        crate::sidecars_from_folder(self, &video)
    }
}

/// One directory entry as the SMB layer reports it.
#[cfg_attr(not(feature = "smb"), allow(dead_code))]
struct DirItem {
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<i64>,
}

impl SmbSource {
    /// Whether `share` takes this source's sign-in (a guest one without a
    /// login): a server's list holds the shares that need another login,
    /// or that this one may not open, and those are left out. Tried by
    /// connecting, and the connection kept for browsing it. A share that
    /// fails any other way (the server stopped answering) stays, so opening
    /// it says why.
    fn can_open_share(&self, share: &str) -> bool {
        match self.connect_share(share) {
            Ok(()) => true,
            Err(e @ (Error::Auth(_) | Error::NotFound(_))) => {
                log::info!("{}: leaving out share {share}: {e}", self.root);
                false
            }
            Err(e) => {
                log::warn!("{}: share {share}: {e}", self.root);
                true
            }
        }
    }
}

#[cfg(not(feature = "smb"))]
impl SmbSource {
    fn connect_share(&self, _share: &str) -> Result<()> {
        Err(Error::SmbUnavailable("compiled without SMB support".into()))
    }

    fn list_shares(&self) -> Result<Vec<String>> {
        Err(Error::SmbUnavailable("compiled without SMB support".into()))
    }

    fn list_dir(&self, _loc: &SmbLocation) -> Result<Vec<DirItem>> {
        Err(Error::SmbUnavailable("compiled without SMB support".into()))
    }

    fn open_file(&self, _loc: &SmbLocation) -> Result<Arc<dyn ByteSource>> {
        Err(Error::SmbUnavailable("compiled without SMB support".into()))
    }
}

#[cfg(feature = "smb")]
mod imp {
    //! The smb2 backed implementation.

    use super::{DirItem, SmbLocation, SmbSource};
    use crate::cache::{CacheOptions, CachedSource, RangeFetch};
    use crate::config::SmbConfig;
    use crate::error::{Error, Result};
    use fp_core::source::ByteSource;
    use fp_core::sync::lock;
    use smb2::{ClientConfig, ErrorKind, FileReader, SmbClient, Tree};
    use std::collections::HashMap;
    use std::io;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, UNIX_EPOCH};

    /// How long connecting, and each request, may take.
    const TIMEOUT: Duration = Duration::from_secs(15);

    /// Waits for `work` on the runtime every source shares; its threads
    /// carry each connection's socket between calls.
    fn run<T>(what: &str, work: impl Future<Output = smb2::Result<T>>) -> Result<T> {
        static RUNTIME: OnceLock<std::result::Result<tokio::runtime::Runtime, String>> =
            OnceLock::new();
        let runtime = RUNTIME.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("fp-smb")
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
        });
        match runtime {
            Ok(rt) => rt.block_on(work).map_err(|e| map_err(e, what)),
            Err(e) => Err(Error::Smb(format!("{what}: cannot start SMB: {e}"))),
        }
    }

    /// Maps an smb2 error, keeping "not found" and "access denied"
    /// distinguishable.
    fn map_err(e: smb2::Error, what: &str) -> Error {
        match e.kind() {
            ErrorKind::AuthRequired
            | ErrorKind::AccessDenied
            | ErrorKind::SigningRequired
            | ErrorKind::GuestSessionRefused => Error::Auth(what.to_string()),
            ErrorKind::NotFound => Error::NotFound(what.to_string()),
            ErrorKind::NotADirectory => Error::invalid(what, "not a folder"),
            ErrorKind::IsADirectory => Error::invalid(what, "not a file"),
            _ => Error::Smb(format!("{what}: {e}")),
        }
    }

    /// A client signed in to the server, and the shares it has connected to
    /// (by lowercase name).
    struct Session {
        client: SmbClient,
        trees: HashMap<String, Tree>,
    }

    impl Session {
        /// The client and `share`'s tree, connecting to the share first if
        /// need be.
        fn share(&mut self, share: &str, url: &str) -> Result<(&mut SmbClient, &mut Tree)> {
            let tree = match self.trees.entry(share.to_lowercase()) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(run(url, self.client.connect_share(share))?)
                }
            };
            Ok((&mut self.client, tree))
        }
    }

    /// The source's current session, shared with its open files, so a file
    /// that loses its connection reconnects for everyone.
    pub(super) struct Sessions {
        config: SmbConfig,
        root: String,
        current: Mutex<Option<Arc<Mutex<Session>>>>,
    }

    impl Sessions {
        pub(super) fn new(config: &SmbConfig, root: &str) -> Sessions {
            Sessions {
                config: config.clone(),
                root: root.to_string(),
                current: Mutex::new(None),
            }
        }

        /// The current session, signing in if there is none.
        fn session(&self) -> Result<Arc<Mutex<Session>>> {
            let mut current = lock(&self.current);
            if let Some(s) = &*current {
                return Ok(s.clone());
            }
            let (username, password) = self.login();
            let host = &self.config.host;
            let port = self.config.port.unwrap_or(445);
            let addr = if host.contains(':') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            };
            log::info!("connecting to {}", self.root);
            let client = run(
                &self.root,
                SmbClient::connect(ClientConfig {
                    addr,
                    timeout: TIMEOUT,
                    username,
                    password,
                    // Video doesn't compress.
                    compression: false,
                    response_timeout: Some(TIMEOUT),
                    ..ClientConfig::default()
                }),
            )?;
            let session = Arc::new(Mutex::new(Session {
                client,
                trees: HashMap::new(),
            }));
            *current = Some(session.clone());
            Ok(session)
        }

        /// The user name and password to sign in with (guest without).
        fn login(&self) -> (String, String) {
            match &self.config.credentials {
                Some(c) => (c.username.clone(), c.password.clone()),
                None => ("Guest".to_string(), String::new()),
            }
        }

        /// Runs `op` with the current session; after a connection-level
        /// failure the session is dropped so the next call signs in again.
        fn with_session<T>(&self, op: impl FnOnce(&Arc<Mutex<Session>>) -> Result<T>) -> Result<T> {
            let session = self.session()?;
            let r = op(&session);
            if let Err(Error::Smb(_)) = &r {
                self.discard(&session);
            }
            r
        }

        /// Forgets `stale` unless it was already replaced, so the next call
        /// signs in again.
        fn discard(&self, stale: &Arc<Mutex<Session>>) {
            let mut current = lock(&self.current);
            if current.as_ref().is_some_and(|s| Arc::ptr_eq(s, stale)) {
                *current = None;
            }
        }

        /// The server's shares worth browsing: smb2 leaves out its own
        /// (`IPC$`, `ADMIN$` and the like) and printers.
        fn list_shares(&self) -> Result<Vec<String>> {
            self.with_session(|session| {
                let shares = run(&self.root, lock(session).client.list_shares())?;
                Ok(shares.into_iter().map(|share| share.name).collect())
            })
        }

        fn connect_share(&self, share: &str, url: &str) -> Result<()> {
            self.with_session(|session| lock(session).share(share, url).map(|_| ()))
        }

        fn list_dir(&self, loc: &SmbLocation, url: &str) -> Result<Vec<DirItem>> {
            self.with_session(|session| {
                let mut session = lock(session);
                let (client, tree) = session.share(&loc.share, url)?;
                let entries = run(url, client.list_directory(tree, &loc.share_path()))?;
                Ok(entries
                    .into_iter()
                    .filter(|e| e.name != "." && e.name != "..")
                    .map(|e| DirItem {
                        modified: e.modified.to_system_time().and_then(|t| {
                            Some(t.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64)
                        }),
                        name: e.name,
                        is_dir: e.is_directory,
                        size: e.size,
                    })
                    .collect())
            })
        }

        /// Opens a file for reading on the current session.
        fn open_file(&self, loc: &SmbLocation, url: &str) -> Result<OpenFile> {
            self.with_session(|session| {
                let mut s = lock(session);
                let (client, tree) = s.share(&loc.share, url)?;
                let reader = run(url, client.open_file_reader(tree, &loc.share_path()))?;
                Ok(OpenFile {
                    session: session.clone(),
                    reader: Arc::new(reader),
                    generation: 0,
                })
            })
        }
    }

    impl SmbSource {
        pub(super) fn list_shares(&self) -> Result<Vec<String>> {
            self.sessions.list_shares()
        }

        /// Connects to `share` (kept for later), or says why it can't.
        pub(super) fn connect_share(&self, share: &str) -> Result<()> {
            let url = super::smb_url(&self.config.host, self.config.port, share, "");
            self.sessions.connect_share(share, &url)
        }

        pub(super) fn list_dir(&self, loc: &SmbLocation) -> Result<Vec<DirItem>> {
            self.sessions.list_dir(loc, &loc.to_url())
        }

        pub(super) fn open_file(&self, loc: &SmbLocation) -> Result<Arc<dyn ByteSource>> {
            let url = loc.to_url();
            let open = self.sessions.open_file(loc, &url)?;
            let fetcher = SmbFetcher {
                sessions: self.sessions.clone(),
                size: open.reader.size(),
                loc: loc.clone(),
                open: Mutex::new(open),
                url,
            };
            Ok(Arc::new(CachedSource::new(
                fetcher,
                &CacheOptions::default(),
            )))
        }
    }

    /// A file handle and the session it was opened on. smb2 closes the
    /// handle on the server when the last reference goes, without waiting.
    #[derive(Clone)]
    struct OpenFile {
        session: Arc<Mutex<Session>>,
        reader: Arc<FileReader>,
        /// Counts reopens, so threads that fail together reconnect once.
        generation: u64,
    }

    impl OpenFile {
        fn read(&self, offset: u64, len: usize, url: &str) -> Result<Vec<u8>> {
            run(url, self.reader.read_at(offset, len as u64))
        }
    }

    /// Reads ranges of one open SMB file.
    struct SmbFetcher {
        sessions: Arc<Sessions>,
        loc: SmbLocation,
        open: Mutex<OpenFile>,
        size: u64,
        url: String,
    }

    impl SmbFetcher {
        /// Replaces the handle of generation `failed` with one on a new
        /// connection; when another thread already did, returns its handle.
        fn reopen(&self, failed: &OpenFile) -> Result<OpenFile> {
            let mut open = lock(&self.open);
            if open.generation != failed.generation {
                return Ok(open.clone());
            }
            self.sessions.discard(&failed.session);
            let mut fresh = self.sessions.open_file(&self.loc, &self.url)?;
            fresh.generation = failed.generation + 1;
            *open = fresh.clone();
            Ok(fresh)
        }
    }

    impl RangeFetch for SmbFetcher {
        fn fetch(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            let open = lock(&self.open).clone();
            match open.read(offset, len, &self.url) {
                Ok(data) => Ok(data),
                Err(e) => {
                    // A NAS that drops the connection, or a request that
                    // timed out (the connection is then unusable), must not
                    // end playback: reconnect and read again, once.
                    log::warn!("{e} at {offset}; reconnecting");
                    let fresh = self.reopen(&open)?;
                    Ok(fresh.read(offset, len, &self.url)?)
                }
            }
        }

        fn size(&self) -> Option<u64> {
            Some(self.size)
        }

        fn describe(&self) -> String {
            self.url.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Credentials;

    #[cfg(feature = "smb")]
    #[test]
    fn stable_locations_follow_the_configured_smb_host_share_and_root() {
        let original = SmbConfig {
            id: "smb-source".into(),
            name: "NAS".into(),
            host: "old".into(),
            port: Some(1445),
            share: "Videos".into(),
            path: "old-root".into(),
            credentials: None,
        };
        let before = SmbSource::new(&original).unwrap();
        let stable = before
            .canonical_location("smb://old:1445/Videos/old-root/sub/Clip%20%231.mp4")
            .unwrap();
        let mut changed = original.clone();
        changed.host = "new".into();
        changed.port = Some(2445);
        changed.share = "Moved Videos".into();
        changed.path = "new-root".into();
        let after = SmbSource::new(&changed).unwrap();
        assert_eq!(
            after.current_url(&stable).unwrap(),
            "smb://new:2445/Moved%20Videos/new-root/sub/Clip%20%231.mp4"
        );
        assert_eq!(after.canonical_location(&stable), Some(stable.clone()));
        let parent = after.parent(&stable).unwrap();
        assert_eq!(crate::location::decode(&parent).unwrap().key, "sub/");
        let root = after.parent(&parent).unwrap();
        assert!(after.parent(&root).is_none());
        for bad in [
            crate::location::encode("other", "smb", "file.mp4"),
            crate::location::encode("smb-source", "http", "file.mp4"),
            crate::location::encode("smb-source", "smb", "../file.mp4"),
            crate::location::encode("smb-source", "smb", "%2e%2e/file.mp4"),
            crate::location::encode("smb-source", "smb", "%2Foutside/file.mp4"),
        ] {
            assert!(after.current_url(&bad).is_err(), "{bad}");
        }
        assert!(
            before
                .canonical_location("smb://old:1445/Videos/old-root-other/file.mp4")
                .is_none()
        );
        assert!(
            before
                .canonical_location("smb://other:1445/Videos/old-root/file.mp4")
                .is_none()
        );
        assert!(
            before
                .canonical_location("smb://old:1445/Other/old-root/file.mp4")
                .is_none()
        );
    }

    #[cfg(feature = "smb")]
    #[test]
    fn whole_server_smb_locations_keep_the_share_in_the_key() {
        let config = SmbConfig {
            id: "server".into(),
            name: "NAS".into(),
            host: "old".into(),
            port: None,
            share: String::new(),
            path: String::new(),
            credentials: None,
        };
        let before = SmbSource::new(&config).unwrap();
        let stable = before
            .canonical_location("smb://old/My%20Share/sub/clip.mp4")
            .unwrap();
        assert_eq!(
            crate::location::decode(&stable).unwrap().key,
            "My%20Share/sub/clip.mp4"
        );
        let mut config = config;
        config.host = "new".into();
        let after = SmbSource::new(&config).unwrap();
        assert_eq!(
            after.current_url(&stable).unwrap(),
            "smb://new/My%20Share/sub/clip.mp4"
        );
    }

    #[test]
    fn urls_roundtrip() {
        assert_eq!(smb_url("nas", None, "media", ""), "smb://nas/media");
        assert_eq!(
            smb_url("nas", Some(4445), "My Share", "/vr/Clip #1.mp4"),
            "smb://nas:4445/My%20Share/vr/Clip%20%231.mp4"
        );
        assert_eq!(smb_url("fe80::1", Some(445), "s", ""), "smb://[fe80::1]/s");
        let l = SmbLocation::parse("smb://u:pw@nas:4445/My%20Share/vr/Clip%20%231.mp4").unwrap();
        assert_eq!(l.host, "nas");
        assert_eq!(l.port, Some(4445));
        assert_eq!(l.share, "My Share");
        assert_eq!(l.path, ["vr", "Clip #1.mp4"]);
        assert_eq!(l.share_path(), "vr/Clip #1.mp4");
        assert_eq!(l.to_url(), "smb://nas:4445/My%20Share/vr/Clip%20%231.mp4");
        let v6 = SmbLocation::parse("SMB://[fe80::1]:139/s/").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("fe80::1", Some(139)));
        assert!(v6.path.is_empty());
        // The server itself.
        for url in ["smb://nas", "smb://nas/", "smb://192.168.0.10/"] {
            let server = SmbLocation::parse(url).unwrap();
            assert!(server.is_server() && server.path.is_empty(), "{url}");
        }
        assert_eq!(smb_url("192.168.0.10", None, "", ""), "smb://192.168.0.10/");
        assert!(SmbLocation::parse("smb:///s").is_err());
        assert!(SmbLocation::parse("http://nas/s").is_err());
        assert!(SmbLocation::parse("smb://nas/s/../x").is_err());
        assert!(SmbLocation::parse("smb://nas:x/s").is_err());
    }

    fn config() -> SmbConfig {
        SmbConfig {
            id: "s".into(),
            name: "NAS".into(),
            host: "nas.local".into(),
            port: None,
            share: "media".into(),
            path: "vr/new".into(),
            credentials: Some(Credentials::new("bob", "hunter2")),
        }
    }

    #[test]
    fn source_basics_without_network() {
        let s = SmbSource::new(&config());
        if !cfg!(feature = "smb") {
            assert!(matches!(s, Err(Error::SmbUnavailable(_))));
            return;
        }
        let s = s.unwrap();
        assert_eq!(s.root(), "smb://nas.local/media/vr/new");
        assert!(!s.describe().contains("hunter2"));
        assert!(!format!("{s:?}").contains("hunter2"));
        assert_eq!(
            s.parent("smb://nas.local/media/vr/new/a.mp4"),
            Some(crate::location::encode("s", "smb", ""))
        );
        assert_eq!(s.parent("smb://nas.local/media"), None);
        assert!(matches!(
            s.list(Some("smb://other/media/")),
            Err(Error::InvalidLocation { .. })
        ));
        assert!(matches!(
            s.open("smb://nas.local/media"),
            Err(Error::InvalidLocation { .. })
        ));
        let mut bad = config();
        bad.host.clear();
        assert!(matches!(SmbSource::new(&bad), Err(Error::Config(_))));
    }

    #[test]
    fn a_whole_server_lists_its_shares_and_reaches_into_any() {
        let mut c = config();
        c.share.clear();
        c.path.clear();
        let s = SmbSource::new(&c);
        if !cfg!(feature = "smb") {
            assert!(matches!(s, Err(Error::SmbUnavailable(_))));
            return;
        }
        let s = s.unwrap();
        assert_eq!(s.root(), "smb://nas.local/");
        assert!(s.describe().starts_with("SMB server"));
        // Any share on it is the source's; another server isn't.
        assert!(
            s.check_same_share(&SmbLocation::parse("smb://nas.local/tv/a").unwrap())
                .is_ok()
        );
        assert!(
            s.check_same_share(&SmbLocation::parse("smb://other/tv/a").unwrap())
                .is_err()
        );
        // Up from a share's root is the list of shares; nothing above that.
        assert_eq!(
            s.parent("smb://nas.local/tv/"),
            Some(crate::location::encode("s", "smb", ""))
        );
        assert_eq!(
            s.parent("smb://nas.local/tv/shows/"),
            Some(crate::location::encode("s", "smb", "tv/"))
        );
        assert_eq!(s.parent("smb://nas.local/"), None);
        assert!(matches!(
            s.open("smb://nas.local/"),
            Err(Error::InvalidLocation { .. })
        ));
    }

    #[cfg(feature = "smb")]
    #[test]
    fn unreachable_server_is_an_error_not_a_hang() {
        let mut c = config();
        // Port 9 on localhost: connection refused straight away.
        c.host = "127.0.0.1".into();
        c.port = Some(9);
        let s = SmbSource::new(&c).unwrap();
        let err = s.list(None).unwrap_err();
        assert!(matches!(err, Error::Smb(_)), "{err:?}");
        assert!(!err.to_string().contains("hunter2"));
    }
}
