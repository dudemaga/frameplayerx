//! Folders on SFTP servers: any machine with an SSH server, as Linux and
//! macOS desktops and most NAS boxes have.
//!
//! # Implementation choice
//!
//! This uses [`russh`](https://crates.io/crates/russh) and
//! [`russh-sftp`](https://crates.io/crates/russh-sftp), pure Rust with
//! `ring` for the cryptography. They are async, so the blocking calls here
//! run them on a small tokio runtime of their own, as SMB does. RSA is
//! in for RSA keys, which PuTTYgen makes by default. Build with
//! `--no-default-features` to leave SFTP out; [`SftpSource::new`] then fails
//! with [`Error::SftpUnavailable`].
//!
//! A source is one folder on a server (`sftp://host[:port]/path`): an empty
//! path is the user's home folder, `~/Videos` a folder in it, `/srv/media`
//! any other. Sign-in tries the chosen key file (OpenSSH, PKCS#8 or PuTTY
//! `.ppk`, with its passphrase), then the password. The server's key is trusted the first time and kept in the
//! source's configuration ([`Source::updated_config`]); a server showing
//! another one is refused with [`Error::HostKeyChanged`].
//!
//! Locations are the source ID and the path below its folder. Files are
//! read through the shared block cache, each block as several reads at
//! once: SFTP answers one read at a time, so one at a time would leave the
//! link idle for a round trip per read.

use crate::config::{SftpConfig, SourceConfig, SourceKind};
use crate::error::{Error, Result};
use crate::urlutil::{encode_segment, percent_decode};
use crate::{Source, dir_entry, file_entry, sort_entries};
use fp_core::source::{ByteSource, Entry};
use fp_core::sync::lock;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Builds `sftp://host[:port]/path` for display: `path` as configured
/// (empty, `~/…` or `/…`), its segments percent-encoded.
pub fn sftp_url(host: &str, port: Option<u16>, path: &str) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]") // IPv6 literal
    } else {
        host.to_string()
    };
    let mut out = format!("sftp://{host}");
    if let Some(p) = port.filter(|p| *p != 22) {
        out.push_str(&format!(":{p}"));
    }
    let segments: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s == "~" {
                s.to_string()
            } else {
                encode_segment(s)
            }
        })
        .collect();
    out.push('/');
    out.push_str(&segments.join("/"));
    out
}

/// What an `sftp://` address names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SftpAddress {
    /// The user in `user@host`, when given.
    pub user: Option<String>,
    /// Host name or address (IPv6 without brackets).
    pub host: String,
    pub port: Option<u16>,
    /// As [`SftpConfig::path`]: empty, `~/…` or `/…`, decoded.
    pub path: String,
}

impl SftpAddress {
    /// Parses `sftp://[user@]host[:port][/path]`; the scheme may be left
    /// out. `/~/x` and `/~` are inside the home folder, any other path is
    /// absolute, and none is the home folder.
    pub fn parse(address: &str) -> Result<SftpAddress> {
        let address = address.trim();
        let rest = match address.get(..7) {
            Some(p) if p.eq_ignore_ascii_case("sftp://") => &address[7..],
            _ if address.contains("://") => {
                return Err(Error::invalid(address, "expected an sftp:// address"));
            }
            _ => address,
        };
        let rest = rest.split(['?', '#']).next().unwrap_or("");
        let (authority, path) = match rest.split_once('/') {
            Some((a, p)) => (a, p),
            None => (rest, ""),
        };
        let (user, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(percent_decode(u)).filter(|u| !u.is_empty()), h),
            None => (None, authority),
        };
        let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
            let (h, after) = v6
                .split_once(']')
                .ok_or_else(|| Error::invalid(address, "bad IPv6 host"))?;
            (h.to_string(), after.strip_prefix(':'))
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p)),
                None => (hostport.to_string(), None),
            }
        };
        if host.is_empty() {
            return Err(Error::invalid(address, "no host"));
        }
        let port = match port.filter(|p| !p.is_empty()) {
            Some(p) => Some(
                p.parse::<u16>()
                    .map_err(|_| Error::invalid(address, "bad port"))?,
            ),
            None => None,
        };
        let segments: Vec<String> = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(percent_decode)
            .collect();
        if segments.iter().any(|s| s == "..") {
            return Err(Error::invalid(address, "path goes up a folder"));
        }
        let path = match segments.split_first() {
            None => String::new(),
            Some((first, rest)) if first == "~" => {
                if rest.is_empty() {
                    String::new()
                } else {
                    format!("~/{}", rest.join("/"))
                }
            }
            Some(_) => format!("/{}", segments.join("/")),
        };
        Ok(SftpAddress {
            user,
            host,
            port,
            path,
        })
    }
}

/// Private key files on the headset, for choosing one: in `~/.ssh` and
/// `~/Downloads`, by their first line.
pub fn key_files() -> Vec<PathBuf> {
    std::env::var_os("HOME")
        .map(|h| key_files_in(Path::new(&h)))
        .unwrap_or_default()
}

fn key_files_in(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in [home.join(".ssh"), home.join("Downloads")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_key_file(p))
            .collect();
        found.sort();
        out.extend(found);
    }
    out
}

/// Whether `path` is a small file starting as a private key does.
fn is_key_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    if file
        .metadata()
        .is_ok_and(|m| !m.is_file() || m.len() > 64 * 1024)
    {
        return false;
    }
    let mut head = [0; 64];
    let n = file.take(64).read(&mut head).unwrap_or(0);
    let head = String::from_utf8_lossy(&head[..n]);
    head.starts_with("PuTTY-User-Key-File-")
        || (head.starts_with("-----BEGIN ") && head.contains("PRIVATE KEY-----"))
}

/// Whether a private key file is password-protected, or `None` when it
/// can't be read as a key.
pub fn key_is_locked(path: &Path) -> Option<bool> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > 64 * 1024 {
        return None;
    }
    let mut text = String::new();
    file.take(64 * 1024).read_to_string(&mut text).ok()?;
    key_text_is_locked(&text)
}

fn key_text_is_locked(text: &str) -> Option<bool> {
    use base64::Engine;
    let text = text.trim_start();
    if text.starts_with("PuTTY-User-Key-File-") {
        return text
            .lines()
            .find_map(|l| l.strip_prefix("Encryption:"))
            .map(|cipher| cipher.trim() != "none");
    }
    let mut lines = text.lines().map(str::trim_end);
    match lines.next()? {
        "-----BEGIN OPENSSH PRIVATE KEY-----" => {
            // The cipher's name follows the format's magic.
            let body: String = lines.take_while(|l| !l.starts_with("-----END ")).collect();
            let raw = base64::engine::general_purpose::STANDARD
                .decode(body)
                .ok()?;
            let rest = raw.strip_prefix(b"openssh-key-v1\0")?;
            let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
            Some(rest.get(4..4 + len)? != b"none")
        }
        "-----BEGIN ENCRYPTED PRIVATE KEY-----" => Some(true),
        // PKCS#1 and SEC1 keys are locked the old OpenSSL way.
        begin if begin.starts_with("-----BEGIN ") && begin.ends_with(" PRIVATE KEY-----") => {
            Some(text.contains("Proc-Type: 4,ENCRYPTED"))
        }
        _ => None,
    }
}

/// A folder on an SFTP server.
pub struct SftpSource {
    id: String,
    name: String,
    /// Shared with the sessions, which fill in the server's key.
    config: Arc<Mutex<SftpConfig>>,
    /// Set when the server's key was first trusted and is yet to be saved.
    changed: Arc<AtomicBool>,
    #[cfg(feature = "sftp")]
    sessions: Arc<imp::Sessions>,
}

impl std::fmt::Debug for SftpSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let config = lock(&self.config);
        f.debug_struct("SftpSource")
            .field("id", &self.id)
            .field("url", &sftp_url(&config.host, config.port, &config.path))
            .field("credentials", &config.credentials)
            .finish_non_exhaustive()
    }
}

impl SftpSource {
    /// Creates the source. No connection is made until it is used.
    pub fn new(config: &SftpConfig) -> Result<SftpSource> {
        if config.host.trim().is_empty() {
            return Err(Error::Config("SFTP source needs a host".into()));
        }
        if config
            .credentials
            .as_ref()
            .is_none_or(|c| c.username.trim().is_empty())
        {
            return Err(Error::Config("SFTP source needs a user name".into()));
        }
        if !cfg!(feature = "sftp") {
            return Err(Error::SftpUnavailable(
                "this build of FramePlayerX was compiled without SFTP support".into(),
            ));
        }
        let (id, name) = (config.id.clone(), config.name.clone());
        let config = Arc::new(Mutex::new(config.clone()));
        let changed = Arc::new(AtomicBool::new(false));
        Ok(SftpSource {
            id,
            name,
            #[cfg(feature = "sftp")]
            sessions: Arc::new(imp::Sessions::new(config.clone(), changed.clone())),
            config,
            changed,
        })
    }

    fn url(&self) -> String {
        let c = lock(&self.config);
        sftp_url(&c.host, c.port, &c.path)
    }

    /// The path below the source's folder that `location` names, as
    /// decoded segments; `None` is the folder itself.
    fn segments(&self, location: Option<&str>) -> Result<Vec<String>> {
        let Some(location) = location else {
            return Ok(Vec::new());
        };
        let value = crate::location::decode(location)
            .filter(|l| l.source_id == self.id && l.kind == "sftp")
            .ok_or_else(|| Error::invalid(location, "not owned by this SFTP source"))?;
        if value.key.starts_with('/') || value.key.contains(['?', '#', '\\']) {
            return Err(Error::invalid(
                "SFTP source resource",
                "invalid relative path",
            ));
        }
        let segments: Vec<String> = value
            .key
            .split('/')
            .filter(|s| !s.is_empty())
            .map(percent_decode)
            .collect();
        if segments
            .iter()
            .any(|s| matches!(s.as_str(), "." | "..") || s.contains(['/', '\0']))
        {
            return Err(Error::invalid(
                "SFTP source resource",
                "invalid path segment",
            ));
        }
        Ok(segments)
    }

    /// The location of `segments` below the source's folder.
    fn location(&self, segments: &[String], folder: bool) -> String {
        let mut key = segments
            .iter()
            .map(|s| encode_segment(s))
            .collect::<Vec<_>>()
            .join("/");
        if folder && !key.is_empty() {
            key.push('/');
        }
        crate::location::encode(&self.id, "sftp", &key)
    }
}

impl Source for SftpSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> SourceKind {
        SourceKind::Sftp
    }

    fn describe(&self) -> String {
        let user = lock(&self.config)
            .credentials
            .as_ref()
            .map(|c| c.username.clone())
            .unwrap_or_default();
        format!("SFTP folder {} as {user}", self.url())
    }

    fn canonical_location(&self, value: &str) -> Option<String> {
        let segments = self.segments(Some(value)).ok()?;
        Some(self.location(&segments, value.ends_with('/')))
    }

    fn list(&self, location: Option<&str>) -> Result<Vec<Entry>> {
        let segments = self.segments(location)?;
        let mut out = Vec::new();
        for item in self.list_dir(&segments)? {
            if item.name.starts_with('.') {
                continue;
            }
            let mut child = segments.clone();
            child.push(item.name.clone());
            out.push(if item.is_dir {
                dir_entry(item.name, self.location(&child, true), item.modified)
            } else {
                file_entry(
                    item.name,
                    self.location(&child, false),
                    Some(item.size),
                    item.modified,
                )
            });
        }
        sort_entries(&mut out);
        Ok(out)
    }

    fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>> {
        let segments = self.segments(Some(location))?;
        if segments.is_empty() {
            return Err(Error::invalid(
                location,
                "a folder cannot be opened as a file",
            ));
        }
        self.open_file(&segments)
    }

    fn updated_config(&self) -> Option<SourceConfig> {
        self.changed
            .swap(false, Ordering::AcqRel)
            .then(|| SourceConfig::Sftp(lock(&self.config).clone()))
    }

    fn parent(&self, location: &str) -> Option<String> {
        let mut segments = self.segments(Some(location)).ok()?;
        segments.pop()?;
        Some(self.location(&segments, true))
    }

    fn sidecars(&self, video: &Entry) -> Result<crate::Sidecars> {
        crate::sidecars_from_folder(self, video)
    }
}

/// One folder entry as the server reports it, a link as what it points at.
#[cfg_attr(not(feature = "sftp"), allow(dead_code))]
struct DirItem {
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<i64>,
}

#[cfg(not(feature = "sftp"))]
impl SftpSource {
    fn list_dir(&self, _segments: &[String]) -> Result<Vec<DirItem>> {
        Err(Error::SftpUnavailable(
            "compiled without SFTP support".into(),
        ))
    }

    fn open_file(&self, _segments: &[String]) -> Result<Arc<dyn ByteSource>> {
        Err(Error::SftpUnavailable(
            "compiled without SFTP support".into(),
        ))
    }
}

#[cfg(feature = "sftp")]
mod imp {
    //! The russh backed implementation.

    use super::{DirItem, SftpSource, sftp_url};
    use crate::cache::{CacheOptions, CachedSource, RangeFetch};
    use crate::config::SftpConfig;
    use crate::error::{Error, Result};
    use fp_core::source::{ByteSource, NetCounters};
    use fp_core::sync::lock;
    use russh::client::{self, Handle};
    use russh::keys::{PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
    use russh_sftp::client::RawSftpSession;
    use russh_sftp::client::error::Error as SftpError;
    use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;
    use tokio::task::JoinSet;

    /// How long connecting, and each request, may take.
    const TIMEOUT: Duration = Duration::from_secs(15);
    /// Reads of one file in flight at once.
    const IN_FLIGHT: usize = 16;
    /// Bytes per read when the server doesn't say how many it takes: what
    /// every server must accept.
    const SAFE_READ: u64 = 32 * 1024;
    /// The most bytes per read, however many a server takes.
    const MAX_READ: u64 = 256 * 1024;
    /// Data the server may send before waiting for the window to open: room
    /// for every read in flight at the largest size.
    const WINDOW: u32 = 16 * 1024 * 1024;

    fn runtime() -> Result<&'static tokio::runtime::Runtime> {
        static RUNTIME: OnceLock<std::result::Result<tokio::runtime::Runtime, String>> =
            OnceLock::new();
        RUNTIME
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("fp-sftp")
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| Error::Sftp(format!("cannot start SFTP: {e}")))
    }

    /// Waits for `work` on the runtime every source shares.
    fn run<T>(work: impl Future<Output = Result<T>>) -> Result<T> {
        runtime()?.block_on(work)
    }

    /// Maps an SFTP error, keeping "not found" and "access denied"
    /// distinguishable.
    fn map_err(e: SftpError, what: &str) -> Error {
        match &e {
            SftpError::Status(s) if s.status_code == StatusCode::NoSuchFile => {
                Error::NotFound(what.to_string())
            }
            SftpError::Status(s) if s.status_code == StatusCode::PermissionDenied => {
                Error::Auth(what.to_string())
            }
            _ => Error::Sftp(format!("{what}: {e}")),
        }
    }

    fn is_eof(e: &SftpError) -> bool {
        matches!(e, SftpError::Status(s) if s.status_code == StatusCode::Eof)
    }

    /// Checks the server's key against the one trusted for the source, and
    /// notes the key shown.
    struct Trust {
        trusted: Option<PublicKey>,
        shown: Arc<Mutex<Option<PublicKey>>>,
    }

    impl client::Handler for Trust {
        type Error = russh::Error;

        async fn check_server_key(
            &mut self,
            key: &PublicKeyOrCertificate,
        ) -> std::result::Result<bool, Self::Error> {
            let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
                return Ok(false);
            };
            *lock(&self.shown) = Some(key.clone());
            Ok(self
                .trusted
                .as_ref()
                .is_none_or(|t| t.key_data() == key.key_data()))
        }
    }

    /// A signed-in connection, its SFTP channel and the source's folder.
    pub(super) struct Session {
        /// Keeps the connection open.
        _ssh: Handle<Trust>,
        sftp: Arc<RawSftpSession>,
        /// The source's folder as an absolute path on the server.
        folder: String,
        /// Bytes per read.
        read_len: u64,
    }

    impl Session {
        fn path(&self, segments: &[String]) -> String {
            let mut path = self.folder.trim_end_matches('/').to_string();
            for s in segments {
                path.push('/');
                path.push_str(s);
            }
            if path.is_empty() {
                path.push('/');
            }
            path
        }
    }

    /// The source's current session, shared with its open files, so a file
    /// that loses its connection reconnects for everyone.
    pub(super) struct Sessions {
        config: Arc<Mutex<SftpConfig>>,
        changed: Arc<AtomicBool>,
        current: Mutex<Option<Arc<Session>>>,
    }

    impl Sessions {
        pub(super) fn new(config: Arc<Mutex<SftpConfig>>, changed: Arc<AtomicBool>) -> Sessions {
            Sessions {
                config,
                changed,
                current: Mutex::new(None),
            }
        }

        /// The current session, signing in if there is none.
        fn session(&self) -> Result<Arc<Session>> {
            let mut current = lock(&self.current);
            if let Some(s) = &*current {
                return Ok(s.clone());
            }
            let session = Arc::new(run(self.connect())?);
            *current = Some(session.clone());
            Ok(session)
        }

        /// Forgets `stale` unless it was already replaced, so the next call
        /// signs in again.
        fn discard(&self, stale: &Arc<Session>) {
            let mut current = lock(&self.current);
            if current.as_ref().is_some_and(|s| Arc::ptr_eq(s, stale)) {
                *current = None;
            }
        }

        /// Runs `op` with the current session; after a connection-level
        /// failure the session is dropped so the next call signs in again.
        fn with_session<T>(&self, op: impl FnOnce(&Arc<Session>) -> Result<T>) -> Result<T> {
            let session = self.session()?;
            let r = op(&session);
            if let Err(Error::Sftp(_)) = &r {
                self.discard(&session);
            }
            r
        }

        async fn connect(&self) -> Result<Session> {
            let config = lock(&self.config).clone();
            let url = sftp_url(&config.host, config.port, &config.path);
            let trusted = match &config.host_key {
                Some(k) => Some(PublicKey::from_openssh(k).map_err(|e| {
                    Error::Config(format!("{url}: the trusted server key can't be read: {e}"))
                })?),
                None => None,
            };
            let shown = Arc::new(Mutex::new(None));
            let trust = Trust {
                trusted: trusted.clone(),
                shown: shown.clone(),
            };
            let ssh_config = Arc::new(client::Config {
                inactivity_timeout: None,
                keepalive_interval: Some(TIMEOUT),
                keepalive_max: 3,
                window_size: WINDOW,
                nodelay: true,
                ..Default::default()
            });
            log::info!("connecting to {url}");
            let addr = (config.host.clone(), config.port.unwrap_or(22));
            let connected = tokio::time::timeout(TIMEOUT, client::connect(ssh_config, addr, trust))
                .await
                .map_err(|_| Error::Sftp(format!("{url}: no answer")))?;
            let mut ssh = match connected {
                Ok(ssh) => ssh,
                Err(e) => {
                    let shown = lock(&shown).clone();
                    let changed = trusted
                        .as_ref()
                        .zip(shown.as_ref())
                        .is_some_and(|(t, s)| t.key_data() != s.key_data());
                    return Err(if changed {
                        log::warn!(
                            "{url}: server key {} isn't the trusted {}",
                            shown
                                .map(|k| k.fingerprint(Default::default()).to_string())
                                .unwrap_or_default(),
                            trusted
                                .map(|k| k.fingerprint(Default::default()).to_string())
                                .unwrap_or_default()
                        );
                        Error::HostKeyChanged(url)
                    } else {
                        Error::Sftp(format!("{url}: {e}"))
                    });
                }
            };
            let user = config
                .credentials
                .as_ref()
                .map(|c| c.username.clone())
                .unwrap_or_default();
            let ssh_err = |e: russh::Error| Error::Sftp(format!("{url}: {e}"));
            let mut signed_in = false;
            if let Some(path) = &config.key_file {
                let passphrase = Some(config.key_passphrase.as_str()).filter(|p| !p.is_empty());
                let key = russh::keys::load_secret_key(path, passphrase).map_err(|e| {
                    if super::key_is_locked(path) == Some(true) {
                        Error::KeyPassphrase(path.display().to_string())
                    } else {
                        Error::Sftp(format!("key file {}: {e}", path.display()))
                    }
                })?;
                // An RSA key signs with the best hash the server takes.
                let hash = if key.algorithm().is_rsa() {
                    ssh.best_supported_rsa_hash()
                        .await
                        .map_err(ssh_err)?
                        .flatten()
                } else {
                    None
                };
                signed_in = ssh
                    .authenticate_publickey(&user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                    .await
                    .map_err(ssh_err)?
                    .success();
            }
            if !signed_in
                && let Some(c) = config
                    .credentials
                    .as_ref()
                    .filter(|c| !c.password.is_empty())
            {
                signed_in = ssh
                    .authenticate_password(&user, &c.password)
                    .await
                    .map_err(ssh_err)?
                    .success();
            }
            if !signed_in {
                return Err(Error::Auth(url));
            }
            let channel = ssh.channel_open_session().await.map_err(ssh_err)?;
            channel
                .request_subsystem(true, "sftp")
                .await
                .map_err(ssh_err)?;
            let sftp = RawSftpSession::new(channel.into_stream());
            sftp.set_timeout(TIMEOUT.as_secs());
            let version = sftp.init().await.map_err(|e| map_err(e, &url))?;
            let read_len = if version.extensions.contains_key("limits@openssh.com") {
                sftp.limits()
                    .await
                    .ok()
                    .map(|l| l.max_read_len)
                    .filter(|&l| l > 0)
                    .unwrap_or(SAFE_READ)
            } else {
                SAFE_READ
            }
            .min(MAX_READ);
            let real = |path: String| {
                let sftp = &sftp;
                let url = &url;
                async move {
                    let name = sftp.realpath(path).await.map_err(|e| map_err(e, url))?;
                    name.files
                        .into_iter()
                        .next()
                        .map(|f| f.filename)
                        .ok_or_else(|| Error::Sftp(format!("{url}: no folder name")))
                }
            };
            let folder = match config.path.as_str() {
                "" | "~" => real(".".into()).await?,
                p => match p.strip_prefix("~/") {
                    Some(inside) => {
                        let home = real(".".into()).await?;
                        real(format!("{}/{inside}", home.trim_end_matches('/'))).await?
                    }
                    None => real(p.to_string()).await?,
                },
            };
            if trusted.is_none()
                && let Some(key) = lock(&shown).clone()
            {
                log::info!(
                    "{url}: trusting the server's key {}",
                    key.fingerprint(Default::default())
                );
                lock(&self.config).host_key = key.to_openssh().ok();
                self.changed.store(true, Ordering::Release);
            }
            Ok(Session {
                _ssh: ssh,
                sftp: Arc::new(sftp),
                folder,
                read_len,
            })
        }

        fn list_dir(&self, segments: &[String]) -> Result<Vec<DirItem>> {
            self.with_session(|s| {
                let path = s.path(segments);
                run(async {
                    let handle = s
                        .sftp
                        .opendir(path.clone())
                        .await
                        .map_err(|e| map_err(e, &path))?
                        .handle;
                    let mut files = Vec::new();
                    let listed = loop {
                        match s.sftp.readdir(handle.clone()).await {
                            Ok(name) => files.extend(name.files),
                            Err(e) if is_eof(&e) => break Ok(()),
                            Err(e) => break Err(map_err(e, &path)),
                        }
                    };
                    let _ = s.sftp.close(handle).await;
                    listed?;
                    let mut out = Vec::new();
                    for f in files {
                        if f.filename == "." || f.filename == ".." {
                            continue;
                        }
                        // A link is listed as what it points at; a broken
                        // one is left out.
                        let attrs = if f.attrs.is_symlink() {
                            match s.sftp.stat(format!("{path}/{}", f.filename)).await {
                                Ok(a) => a.attrs,
                                Err(_) => continue,
                            }
                        } else {
                            f.attrs
                        };
                        out.push(DirItem {
                            is_dir: attrs.is_dir(),
                            size: attrs.size.unwrap_or(0),
                            modified: attrs.mtime.map(i64::from),
                            name: f.filename,
                        });
                    }
                    Ok(out)
                })
            })
        }

        /// Opens a file for reading on the current session.
        fn open_file(&self, segments: &[String]) -> Result<OpenFile> {
            self.with_session(|s| {
                let path = s.path(segments);
                run(async {
                    let handle = s
                        .sftp
                        .open(path.clone(), OpenFlags::READ, FileAttributes::default())
                        .await
                        .map_err(|e| map_err(e, &path))?
                        .handle;
                    let size = match s.sftp.fstat(handle.clone()).await {
                        Ok(a) => a.attrs.size,
                        Err(e) => {
                            let _ = s.sftp.close(handle).await;
                            return Err(map_err(e, &path));
                        }
                    };
                    let Some(size) = size else {
                        let _ = s.sftp.close(handle).await;
                        return Err(Error::Sftp(format!("{path}: the server gives no size")));
                    };
                    Ok(OpenFile {
                        handle: Arc::new(FileHandle {
                            session: s.clone(),
                            handle,
                        }),
                        size,
                        generation: 0,
                    })
                })
            })
        }
    }

    impl SftpSource {
        pub(super) fn list_dir(&self, segments: &[String]) -> Result<Vec<DirItem>> {
            self.sessions.list_dir(segments)
        }

        pub(super) fn open_file(&self, segments: &[String]) -> Result<Arc<dyn ByteSource>> {
            let open = self.sessions.open_file(segments)?;
            let url = format!(
                "{}/{}",
                self.url().trim_end_matches('/'),
                segments.join("/")
            );
            let net = Arc::new(NetCounters::default());
            let fetcher = SftpFetcher {
                sessions: self.sessions.clone(),
                segments: segments.to_vec(),
                size: open.size,
                open: Mutex::new(open),
                url,
                net: net.clone(),
            };
            Ok(Arc::new(CachedSource::counted(
                fetcher,
                &CacheOptions::default(),
                net,
            )))
        }
    }

    /// A file open on the server, closed when the last reader lets go.
    struct FileHandle {
        session: Arc<Session>,
        handle: String,
    }

    impl Drop for FileHandle {
        fn drop(&mut self) {
            let (sftp, handle) = (self.session.sftp.clone(), std::mem::take(&mut self.handle));
            if let Ok(rt) = runtime() {
                rt.spawn(async move {
                    let _ = sftp.close(handle).await;
                });
            }
        }
    }

    #[derive(Clone)]
    struct OpenFile {
        handle: Arc<FileHandle>,
        size: u64,
        /// Counts reopens, so threads that fail together reconnect once.
        generation: u64,
    }

    impl OpenFile {
        /// Reads `len` bytes at `offset` as reads of the session's size, up
        /// to [`IN_FLIGHT`] at once; fewer bytes only at the end of the file.
        fn read(&self, offset: u64, len: usize, url: &str) -> Result<Vec<u8>> {
            let session = &self.handle.session;
            let step = session.read_len.max(1) as usize;
            let pieces: Vec<(u64, usize)> = (0..len)
                .step_by(step)
                .map(|start| (offset + start as u64, step.min(len - start)))
                .collect();
            run(async {
                let mut got: Vec<Option<Vec<u8>>> = vec![None; pieces.len()];
                let mut reads = JoinSet::new();
                let mut next = 0;
                loop {
                    while next < pieces.len() && reads.len() < IN_FLIGHT {
                        let (at, want) = pieces[next];
                        let (sftp, handle) = (session.sftp.clone(), self.handle.handle.clone());
                        let i = next;
                        reads.spawn(async move { (i, read_piece(&sftp, &handle, at, want).await) });
                        next += 1;
                    }
                    let Some(done) = reads.join_next().await else {
                        break;
                    };
                    let (i, data) = done.map_err(|e| Error::Sftp(format!("{url}: {e}")))?;
                    got[i] = Some(data.map_err(|e| map_err(e, url))?);
                }
                let mut out = Vec::with_capacity(len);
                for (piece, (_, want)) in got.into_iter().zip(&pieces) {
                    let piece = piece.unwrap_or_default();
                    let short = piece.len() < *want;
                    out.extend(piece);
                    // The end of the file.
                    if short {
                        break;
                    }
                }
                Ok(out)
            })
        }
    }

    /// Reads `len` bytes at `offset`, asking again for what a server sends
    /// short of the end of the file.
    async fn read_piece(
        sftp: &RawSftpSession,
        handle: &str,
        mut offset: u64,
        len: usize,
    ) -> std::result::Result<Vec<u8>, SftpError> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            match sftp.read(handle, offset, (len - out.len()) as u32).await {
                Ok(d) if d.data.is_empty() => break,
                Ok(d) => {
                    offset += d.data.len() as u64;
                    out.extend(d.data);
                }
                Err(e) if is_eof(&e) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// Reads ranges of one open SFTP file.
    struct SftpFetcher {
        sessions: Arc<Sessions>,
        segments: Vec<String>,
        open: Mutex<OpenFile>,
        size: u64,
        url: String,
        net: Arc<NetCounters>,
    }

    impl SftpFetcher {
        /// Replaces the handle of generation `failed` with one on a new
        /// connection; when another thread already did, returns its handle.
        fn reopen(&self, failed: &OpenFile) -> Result<OpenFile> {
            let mut open = lock(&self.open);
            if open.generation != failed.generation {
                return Ok(open.clone());
            }
            self.net.retries.fetch_add(1, Ordering::Relaxed);
            self.sessions.discard(&failed.handle.session);
            let mut fresh = self.sessions.open_file(&self.segments)?;
            fresh.generation = failed.generation + 1;
            *open = fresh.clone();
            Ok(fresh)
        }
    }

    impl RangeFetch for SftpFetcher {
        fn fetch(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            let open = lock(&self.open).clone();
            match open.read(offset, len, &self.url) {
                Ok(data) => Ok(data),
                Err(e) => {
                    // A server that drops the connection, or a request that
                    // timed out, must not end playback: reconnect and read
                    // again, once.
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

    #[test]
    fn addresses_name_the_home_folder_a_folder_in_it_or_any_other() {
        let a = SftpAddress::parse("sftp://me@pc.local:2222/~/Movies/VR%20180").unwrap();
        assert_eq!(a.user.as_deref(), Some("me"));
        assert_eq!((a.host.as_str(), a.port), ("pc.local", Some(2222)));
        assert_eq!(a.path, "~/Movies/VR 180");
        assert_eq!(SftpAddress::parse("pc.local").unwrap().path, "");
        assert_eq!(SftpAddress::parse("sftp://pc.local/~").unwrap().path, "");
        assert_eq!(
            SftpAddress::parse("sftp://[fe80::1]/srv/media/").unwrap(),
            SftpAddress {
                user: None,
                host: "fe80::1".into(),
                port: None,
                path: "/srv/media".into(),
            }
        );
        for bad in [
            "sftp://",
            "http://pc/x",
            "sftp://pc:port/",
            "sftp://pc/a/../b",
        ] {
            assert!(SftpAddress::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn key_files_are_found_by_their_first_line() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        let downloads = home.path().join("Downloads");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::create_dir_all(&downloads).unwrap();
        let begin = "-----BEGIN OPENSSH PRIVATE KEY-----\n";
        std::fs::write(ssh.join("id_ed25519"), begin).unwrap();
        std::fs::write(ssh.join("id_ed25519.pub"), "ssh-ed25519 AAAA me@pc\n").unwrap();
        std::fs::write(ssh.join("known_hosts"), "pc ssh-ed25519 AAAA\n").unwrap();
        std::fs::write(
            downloads.join("desktop.ppk"),
            "PuTTY-User-Key-File-3: ssh-ed25519\n",
        )
        .unwrap();
        std::fs::write(downloads.join("notes.txt"), "keys").unwrap();
        assert_eq!(
            key_files_in(home.path()),
            [ssh.join("id_ed25519"), downloads.join("desktop.ppk")]
        );
    }

    #[test]
    fn a_key_file_says_whether_it_is_password_protected() {
        use base64::Engine;
        let openssh = |cipher: &str| {
            let mut raw = b"openssh-key-v1\0".to_vec();
            raw.extend((cipher.len() as u32).to_be_bytes());
            raw.extend(cipher.as_bytes());
            let body = base64::engine::general_purpose::STANDARD.encode(raw);
            format!(
                "-----BEGIN OPENSSH PRIVATE KEY-----\r\n{body}\r\n\
                 -----END OPENSSH PRIVATE KEY-----\r\n"
            )
        };
        assert_eq!(key_text_is_locked(&openssh("none")), Some(false));
        assert_eq!(key_text_is_locked(&openssh("aes256-ctr")), Some(true));
        let ppk = |cipher| format!("PuTTY-User-Key-File-3: ssh-rsa\r\nEncryption: {cipher}\r\n");
        assert_eq!(key_text_is_locked(&ppk("none")), Some(false));
        assert_eq!(key_text_is_locked(&ppk("aes256-cbc")), Some(true));
        let pem = |label: &str, headers: &str| {
            format!("-----BEGIN {label}-----\n{headers}AAAA\n-----END {label}-----\n")
        };
        assert_eq!(key_text_is_locked(&pem("PRIVATE KEY", "")), Some(false));
        assert_eq!(
            key_text_is_locked(&pem("ENCRYPTED PRIVATE KEY", "")),
            Some(true)
        );
        assert_eq!(key_text_is_locked(&pem("RSA PRIVATE KEY", "")), Some(false));
        assert_eq!(
            key_text_is_locked(&pem(
                "RSA PRIVATE KEY",
                "Proc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00\n\n"
            )),
            Some(true)
        );
        assert_eq!(key_text_is_locked("ssh-ed25519 AAAA me@pc\n"), None);
        assert_eq!(
            key_text_is_locked("-----BEGIN OPENSSH PRIVATE KEY-----\nnot base64!\n"),
            None
        );
    }

    #[test]
    fn urls_show_the_path_as_configured() {
        assert_eq!(sftp_url("pc", None, ""), "sftp://pc/");
        assert_eq!(
            sftp_url("pc", Some(22), "~/My Videos"),
            "sftp://pc/~/My%20Videos"
        );
        assert_eq!(
            sftp_url("fe80::1", Some(2222), "/srv"),
            "sftp://[fe80::1]:2222/srv"
        );
        for path in ["", "~/My Videos", "/srv/media"] {
            let url = sftp_url("pc", Some(2222), path);
            assert_eq!(SftpAddress::parse(&url).unwrap().path, path, "{url}");
        }
    }
}

/// Against an SFTP server in the test, serving a temporary folder. Its keys
/// and password are made at random for each run.
#[cfg(all(test, feature = "sftp"))]
mod server_tests {
    use super::*;
    use crate::config::Credentials;
    use russh::keys::ssh_key::{LineEnding, private::Ed25519Keypair};
    use russh::keys::{PrivateKey, PublicKey};
    use russh::server::{Auth, Msg, Session};
    use russh::{Channel, ChannelId};
    use russh_sftp::protocol::{
        Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
    };
    use std::collections::HashMap;
    use std::io::{Read, Seek, SeekFrom};
    use std::path::{Path, PathBuf};
    use std::result::Result;
    use std::time::Duration;

    fn random<const N: usize>() -> [u8; N] {
        use ring::rand::SecureRandom;
        let mut bytes = [0; N];
        ring::rand::SystemRandom::new().fill(&mut bytes).unwrap();
        bytes
    }

    fn new_key() -> PrivateKey {
        PrivateKey::from(Ed25519Keypair::from_seed(&random::<32>()))
    }

    fn new_password() -> String {
        random::<12>().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// What the server accepts.
    #[derive(Clone)]
    struct Accepts {
        user: String,
        password: String,
        key: Option<PublicKey>,
    }

    struct Ssh {
        home: PathBuf,
        accepts: Accepts,
        channels: HashMap<ChannelId, Channel<Msg>>,
    }

    impl russh::server::Handler for Ssh {
        type Error = russh::Error;

        async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
            Ok(
                if user == self.accepts.user && password == self.accepts.password {
                    Auth::Accept
                } else {
                    Auth::reject()
                },
            )
        }

        async fn auth_publickey(
            &mut self,
            user: &str,
            key: russh::server::VerifiedPublicKey<'_>,
        ) -> Result<Auth, Self::Error> {
            let known = self
                .accepts
                .key
                .as_ref()
                .is_some_and(|k| k.key_data() == key.public_key.key_data());
            Ok(if user == self.accepts.user && known {
                Auth::Accept
            } else {
                Auth::reject()
            })
        }

        async fn channel_open_session(
            &mut self,
            channel: Channel<Msg>,
            reply: russh::server::ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            self.channels.insert(channel.id(), channel);
            reply.accept().await;
            Ok(())
        }

        async fn subsystem_request(
            &mut self,
            id: ChannelId,
            name: &str,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            match self.channels.remove(&id).filter(|_| name == "sftp") {
                Some(channel) => {
                    session.channel_success(id)?;
                    let fs = Fs {
                        home: self.home.clone(),
                        open: HashMap::new(),
                        next: 0,
                    };
                    russh_sftp::server::run(channel.into_stream(), fs).await;
                }
                None => session.channel_failure(id)?,
            }
            Ok(())
        }
    }

    enum Open {
        File(std::fs::File),
        Dir(Option<Vec<File>>),
    }

    /// The server's files: the real ones under `home`.
    struct Fs {
        home: PathBuf,
        open: HashMap<String, Open>,
        next: u64,
    }

    fn ok(id: u32) -> Status {
        Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en-US".into(),
        }
    }

    fn status(e: std::io::Error) -> StatusCode {
        match e.kind() {
            std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
            std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
            _ => StatusCode::Failure,
        }
    }

    impl Fs {
        fn add(&mut self, open: Open) -> String {
            self.next += 1;
            let handle = self.next.to_string();
            self.open.insert(handle.clone(), open);
            handle
        }
    }

    impl russh_sftp::server::Handler for Fs {
        type Error = StatusCode;

        fn unimplemented(&self) -> Self::Error {
            StatusCode::OpUnsupported
        }

        async fn init(
            &mut self,
            _version: u32,
            _extensions: HashMap<String, String>,
        ) -> Result<Version, Self::Error> {
            Ok(Version::new())
        }

        async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
            let path = if path == "." {
                self.home.clone()
            } else {
                std::fs::canonicalize(&path).map_err(status)?
            };
            Ok(Name {
                id,
                files: vec![File::dummy(path.to_string_lossy())],
            })
        }

        async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
            let mut files = Vec::new();
            for e in std::fs::read_dir(&path).map_err(status)? {
                let e = e.map_err(status)?;
                let meta = std::fs::symlink_metadata(e.path()).map_err(status)?;
                files.push(File::new(e.file_name().to_string_lossy(), (&meta).into()));
            }
            Ok(Handle {
                id,
                handle: self.add(Open::Dir(Some(files))),
            })
        }

        async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
            match self.open.get_mut(&handle) {
                Some(Open::Dir(files)) => match files.take() {
                    Some(files) => Ok(Name { id, files }),
                    None => Err(StatusCode::Eof),
                },
                _ => Err(StatusCode::Failure),
            }
        }

        async fn open(
            &mut self,
            id: u32,
            filename: String,
            _flags: OpenFlags,
            _attrs: FileAttributes,
        ) -> Result<Handle, Self::Error> {
            let file = std::fs::File::open(&filename).map_err(status)?;
            Ok(Handle {
                id,
                handle: self.add(Open::File(file)),
            })
        }

        async fn read(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            len: u32,
        ) -> Result<Data, Self::Error> {
            let Some(Open::File(file)) = self.open.get_mut(&handle) else {
                return Err(StatusCode::Failure);
            };
            file.seek(SeekFrom::Start(offset)).map_err(status)?;
            let mut data = Vec::new();
            file.take(u64::from(len))
                .read_to_end(&mut data)
                .map_err(status)?;
            if data.is_empty() {
                return Err(StatusCode::Eof);
            }
            Ok(Data { id, data })
        }

        async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
            let Some(Open::File(file)) = self.open.get(&handle) else {
                return Err(StatusCode::Failure);
            };
            let meta = file.metadata().map_err(status)?;
            Ok(Attrs {
                id,
                attrs: (&meta).into(),
            })
        }

        async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            let meta = std::fs::metadata(&path).map_err(status)?;
            Ok(Attrs {
                id,
                attrs: (&meta).into(),
            })
        }

        async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
            self.open.remove(&handle);
            Ok(ok(id))
        }
    }

    /// A server on a free local port; it stops with the runtime.
    struct Server {
        _runtime: tokio::runtime::Runtime,
        port: u16,
        key: PublicKey,
    }

    fn serve(home: &Path, accepts: Accepts) -> Server {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let host = new_key();
        let key = host.public_key().clone();
        let config = Arc::new(russh::server::Config {
            keys: vec![host],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let home = std::fs::canonicalize(home).unwrap();
        runtime.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let ssh = Ssh {
                    home: home.clone(),
                    accepts: accepts.clone(),
                    channels: HashMap::new(),
                };
                let config = config.clone();
                tokio::spawn(async move {
                    if let Ok(session) = russh::server::run_stream(config, stream, ssh).await {
                        let _ = session.await;
                    }
                });
            }
        });
        Server {
            _runtime: runtime,
            port,
            key,
        }
    }

    fn config(server: &Server, path: &str, user: &str, password: &str) -> SftpConfig {
        SftpConfig {
            id: "sftp-test".into(),
            name: "Test".into(),
            host: "127.0.0.1".into(),
            port: Some(server.port),
            path: path.into(),
            credentials: Some(Credentials::new(user, password)),
            key_file: None,
            key_passphrase: String::new(),
            host_key: None,
        }
    }

    /// Signs in as `viewer` with `key_file` and no password.
    fn key_config(server: &Server, key_file: &Path) -> SftpConfig {
        SftpConfig {
            credentials: Some(Credentials::new("viewer", String::new())),
            key_file: Some(key_file.to_path_buf()),
            ..config(server, "", "viewer", &new_password())
        }
    }

    /// A home folder with a video, its subtitles, a folder, a link to it, a
    /// broken link and a hidden file; the video's bytes.
    fn home() -> (tempfile::TempDir, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        // Longer than a cache block, which then takes more reads than are
        // in flight at once.
        let video: Vec<u8> = (0..2_500_000u32).map(|i| (i * 7 + i / 3) as u8).collect();
        std::fs::write(dir.path().join("Clip_180_LR.mp4"), &video).unwrap();
        std::fs::write(
            dir.path().join("Clip_180_LR.srt"),
            b"1\n00:00:01,000 --> 00:00:02,000\nHi\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(".hidden.mp4"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("Shows")).unwrap();
        std::fs::write(dir.path().join("Shows").join("Episode.mkv"), b"episode").unwrap();
        std::os::unix::fs::symlink(dir.path().join("Shows"), dir.path().join("Linked")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("Broken")).unwrap();
        (dir, video)
    }

    fn accepts(password: &str, key: Option<PublicKey>) -> Accepts {
        Accepts {
            user: "viewer".into(),
            password: password.into(),
            key,
        }
    }

    #[test]
    fn lists_reads_and_follows_links_and_trusts_the_first_key() {
        let (dir, video) = home();
        let password = new_password();
        let server = serve(dir.path(), accepts(&password, None));
        let source = SftpSource::new(&config(&server, "", "viewer", &password)).unwrap();

        let entries = source.list(None).unwrap();
        let names: Vec<(&str, fp_core::EntryKind)> =
            entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
        assert_eq!(
            names,
            [
                ("Linked", fp_core::EntryKind::Directory),
                ("Shows", fp_core::EntryKind::Directory),
                ("Clip_180_LR.mp4", fp_core::EntryKind::Video),
                ("Clip_180_LR.srt", fp_core::EntryKind::Other),
            ]
        );
        let clip = entries
            .iter()
            .find(|e| e.name == "Clip_180_LR.mp4")
            .unwrap();
        assert_eq!(clip.size, Some(video.len() as u64));

        let file = source.open(&clip.location).unwrap();
        assert_eq!(file.size(), Some(video.len() as u64));
        let mut buf = vec![0; 1_300_000];
        for at in [0u64, 1_000_000, 2_400_000] {
            let n = file.read_at(at, &mut buf).unwrap();
            let end = (at as usize + buf.len()).min(video.len());
            assert_eq!(&buf[..n], &video[at as usize..end], "at {at}");
        }

        let linked = entries.iter().find(|e| e.name == "Linked").unwrap();
        let inside = source.list(Some(&linked.location)).unwrap();
        assert_eq!(inside[0].name, "Episode.mkv");
        assert_eq!(
            source.parent(&inside[0].location),
            Some(linked.location.clone())
        );

        let sidecars = source.sidecars(clip).unwrap();
        assert_eq!(sidecars.subtitles.len(), 1, "{sidecars:?}");

        let Some(SourceConfig::Sftp(saved)) = source.updated_config() else {
            panic!("the first key isn't kept");
        };
        let kept = PublicKey::from_openssh(saved.host_key.as_deref().unwrap()).unwrap();
        assert_eq!(kept.key_data(), server.key.key_data());
        assert!(source.updated_config().is_none(), "kept once");

        // Trusted from then on.
        let again = SftpSource::new(&saved).unwrap();
        assert_eq!(again.list(None).unwrap().len(), 4);
    }

    #[test]
    fn a_server_with_another_key_is_refused() {
        let (dir, _) = home();
        let password = new_password();
        let server = serve(dir.path(), accepts(&password, None));
        let mut cfg = config(&server, "", "viewer", &password);
        cfg.host_key = new_key().public_key().to_openssh().ok();
        let source = SftpSource::new(&cfg).unwrap();
        assert!(matches!(source.list(None), Err(Error::HostKeyChanged(_))));
        assert!(source.updated_config().is_none(), "the trusted key stays");
    }

    #[test]
    fn a_wrong_password_is_refused() {
        let (dir, _) = home();
        let server = serve(dir.path(), accepts(&new_password(), None));
        let source = SftpSource::new(&config(&server, "", "viewer", &new_password())).unwrap();
        assert!(matches!(source.list(None), Err(Error::Auth(_))));
    }

    /// `key` as an unencrypted PuTTY v3 file, MAC included.
    fn ppk(key: &PrivateKey) -> String {
        use base64::Engine;
        let string = |out: &mut Vec<u8>, bytes: &[u8]| {
            out.extend((bytes.len() as u32).to_be_bytes());
            out.extend(bytes);
        };
        let seed = key.key_data().ed25519().unwrap().private.to_bytes();
        let public = key.public_key().to_bytes().unwrap();
        let mut private = Vec::new();
        string(&mut private, &seed);
        let comment = "test";
        let mut mac_data = Vec::new();
        for field in [
            "ssh-ed25519".as_bytes(),
            b"none",
            comment.as_bytes(),
            &public,
            &private,
        ] {
            string(&mut mac_data, field);
        }
        let mac = ring::hmac::sign(
            &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &[]),
            &mac_data,
        );
        let lines = |bytes: &[u8]| {
            let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
            let lines: Vec<String> = b64
                .as_bytes()
                .chunks(64)
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .collect();
            (lines.len(), lines.join("\n"))
        };
        let (public_count, public_lines) = lines(&public);
        let (private_count, private_lines) = lines(&private);
        let hex: String = mac.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        format!(
            "PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: none\nComment: {comment}\n\
             Public-Lines: {public_count}\n{public_lines}\nPrivate-Lines: {private_count}\n\
             {private_lines}\nPrivate-MAC: {hex}\n"
        )
    }

    /// An Ed25519 key whose seed is an SSH integer of its own length (no
    /// sign byte), as the PuTTY file above writes it.
    fn ppk_ready_key() -> PrivateKey {
        loop {
            let key = new_key();
            let first = key.key_data().ed25519().unwrap().private.to_bytes()[0];
            if (1..0x80).contains(&first) {
                return key;
            }
        }
    }

    #[test]
    fn a_key_file_signs_in_without_a_password_in_openssh_or_putty_form() {
        let (dir, _) = home();
        let client = ppk_ready_key();
        let server = serve(
            dir.path(),
            accepts(&new_password(), Some(client.public_key().clone())),
        );
        let keys = tempfile::tempdir().unwrap();
        let openssh = keys.path().join("id_ed25519");
        std::fs::write(
            &openssh,
            client.to_openssh(LineEnding::LF).unwrap().as_bytes(),
        )
        .unwrap();
        let putty = keys.path().join("desktop.ppk");
        std::fs::write(&putty, ppk(&client)).unwrap();
        for file in [openssh, putty] {
            assert_eq!(key_is_locked(&file), Some(false), "{}", file.display());
            let source = SftpSource::new(&key_config(&server, &file)).unwrap();
            assert_eq!(source.list(None).unwrap().len(), 4, "{}", file.display());
        }
        // A key the server doesn't know, and no password: refused.
        let other = keys.path().join("other");
        std::fs::write(
            &other,
            new_key().to_openssh(LineEnding::LF).unwrap().as_bytes(),
        )
        .unwrap();
        let source = SftpSource::new(&key_config(&server, &other)).unwrap();
        assert!(matches!(source.list(None), Err(Error::Auth(_))));
    }

    #[test]
    fn a_password_protected_key_needs_its_passphrase() {
        use russh::keys::ssh_key::{Cipher, Kdf};
        let (dir, _) = home();
        let client = new_key();
        let server = serve(
            dir.path(),
            accepts(&new_password(), Some(client.public_key().clone())),
        );
        let passphrase = new_password();
        // One bcrypt round keeps the test quick; ssh-keygen uses 16.
        let locked = client
            .encrypt_with(
                Cipher::Aes256Ctr,
                Kdf::Bcrypt {
                    salt: random::<16>().to_vec(),
                    rounds: 1,
                },
                u32::from_ne_bytes(random()),
                &passphrase,
            )
            .unwrap();
        let keys = tempfile::tempdir().unwrap();
        let file = keys.path().join("id_ed25519");
        std::fs::write(&file, locked.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
        assert_eq!(key_is_locked(&file), Some(true));
        for (given, works) in [
            (passphrase, true),
            (new_password(), false),
            (String::new(), false),
        ] {
            let mut cfg = key_config(&server, &file);
            cfg.key_passphrase = given;
            let listed = SftpSource::new(&cfg).unwrap().list(None);
            if works {
                assert_eq!(listed.unwrap().len(), 4);
            } else {
                assert!(matches!(listed, Err(Error::KeyPassphrase(_))), "{listed:?}");
            }
        }
    }

    #[test]
    fn the_folder_can_be_inside_home_or_anywhere() {
        let (dir, _) = home();
        let password = new_password();
        let server = serve(dir.path(), accepts(&password, None));
        let real = std::fs::canonicalize(dir.path().join("Shows")).unwrap();
        for path in ["~/Shows".to_string(), real.to_string_lossy().into_owned()] {
            let source = SftpSource::new(&config(&server, &path, "viewer", &password)).unwrap();
            let entries = source.list(None).unwrap();
            assert_eq!(entries.len(), 1, "{path}");
            assert_eq!(entries[0].name, "Episode.mkv", "{path}");
            assert_eq!(
                source.parent(&entries[0].location),
                Some(source.location(&[], true))
            );
        }
    }
}
