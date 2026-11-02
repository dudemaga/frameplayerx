//! Source configuration, credentials, and their on-disk storage.
//!
//! Credentials live inside [`SourceConfig`] and are written in clear JSON to
//! a file only the user can read (mode 0600, folder 0700): SteamOS gaming
//! mode has no secret service to keep them in.

use crate::error::{Error, Result};
use crate::urlutil::redact;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// User name and password for a network source.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// User name; for SMB may be `DOMAIN\user` or `user@domain`.
    pub username: String,
    /// Password, stored in clear in the 0600 config file.
    #[serde(default)]
    pub password: String,
}

impl Credentials {
    /// Builds credentials from a user name and password.
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Credentials {
        Credentials {
            username: username.into(),
            password: password.into(),
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"***")
            .finish()
    }
}

/// Which kind of source a configuration or [`crate::Source`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A local folder (internal storage, microSD, USB drive).
    Local,
    /// A folder on the headset or a card, scanned for videos.
    Library,
    /// An HTTP(S) server with autoindex directory listings.
    Http,
    #[serde(rename = "webdav")]
    WebDav,
    /// A DLNA/UPnP media server.
    Dlna,
    /// A DeoVR JSON feed.
    #[serde(rename = "deovr")]
    DeoVr,
    /// A HereSphere JSON API.
    #[serde(rename = "heresphere")]
    HereSphere,
    /// An SMB2/3 share.
    Smb,
    /// A Jellyfin or Emby media server (MediaBrowser API).
    #[serde(rename = "mediabrowser")]
    MediaBrowser,
    /// A Plex Media Server.
    Plex,
}

impl SourceKind {
    /// Short label for the UI.
    pub fn label(&self) -> &'static str {
        match self {
            SourceKind::Local => "Local folder",
            SourceKind::Library => "Library",
            SourceKind::Http => "HTTP",
            SourceKind::WebDav => "WebDAV",
            SourceKind::Dlna => "DLNA",
            SourceKind::DeoVr => "DeoVR feed",
            SourceKind::HereSphere => "HereSphere API",
            SourceKind::Smb => "SMB share",
            SourceKind::MediaBrowser => "Jellyfin/Emby",
            SourceKind::Plex => "Plex",
        }
    }
}

/// A local folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalConfig {
    /// Stable identifier chosen by the app.
    pub id: String,
    pub name: String,
    /// Folder shown at the root of the source.
    pub root: PathBuf,
}

/// A library: a folder on the headset or on a microSD card or USB drive,
/// scanned for videos.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LibraryConfig {
    /// Stable identifier chosen by the app.
    pub id: String,
    pub name: String,
    /// The folder: a path on the headset, or on [`Self::card`] when set.
    pub folder: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<CardRef>,
}

/// A microSD card or USB drive, by its filesystem's ID
/// ([`fp_core::volume`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardRef {
    pub id: String,
    /// What it was mounted as when the library was made, to name it while
    /// it is out.
    pub label: String,
}

impl LibraryConfig {
    /// Where its videos are: the folder's path, or its location on the
    /// card.
    pub fn root(&self) -> String {
        let folder = self.folder.to_string_lossy();
        match &self.card {
            Some(c) if folder.is_empty() => format!("{}{}", fp_core::volume::SCHEME, c.id),
            Some(c) => format!("{}{}/{folder}", fp_core::volume::SCHEME, c.id),
            None => folder.into_owned(),
        }
    }
}

/// An HTTP(S) autoindex server or a WebDAV share.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpConfig {
    /// Stable identifier chosen by the app.
    pub id: String,
    pub name: String,
    /// Root folder URL. WebDAV also accepts `webdav(s)://` and `dav(s)://`.
    pub url: String,
    /// HTTP Basic credentials.
    #[serde(default)]
    pub credentials: Option<Credentials>,
    /// Accept self-signed or otherwise invalid TLS certificates (common on
    /// home NAS boxes).
    #[serde(default)]
    pub insecure_tls: bool,
}

/// A DLNA/UPnP media server.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct DlnaConfig {
    /// Stable identifier chosen by the app.
    pub id: String,
    /// Display name (usually the device's friendly name).
    pub name: String,
    /// Device description URL (the SSDP `LOCATION`).
    pub location: String,
    /// ContentDirectory control URL, when already known from discovery.
    /// Fetched from the device description when absent.
    #[serde(default)]
    pub control_url: Option<String>,
    /// ContentDirectory service type, e.g.
    /// `urn:schemas-upnp-org:service:ContentDirectory:1`.
    #[serde(default)]
    pub service_type: Option<String>,
}

/// A DeoVR JSON feed or HereSphere JSON API endpoint.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedConfig {
    /// Stable identifier chosen by the app.
    pub id: String,
    pub name: String,
    /// Feed URL, e.g. `http://server:9999/deovr` or `http://server:9999/heresphere`.
    pub url: String,
    /// Feed login (DeoVR form login / HereSphere JSON body) and HTTP Basic auth.
    #[serde(default)]
    pub credentials: Option<Credentials>,
    /// Accept invalid TLS certificates.
    #[serde(default)]
    pub insecure_tls: bool,
    /// Highest video height to pick among the encodings a scene offers
    /// (e.g. 2880 to skip 8K files). `None` picks the best.
    #[serde(default)]
    pub max_height: Option<u32>,
}

/// An SMB2/3 share.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SmbConfig {
    /// Stable identifier chosen by the app.
    pub id: String,
    pub name: String,
    /// Server host name or IP address.
    pub host: String,
    /// TCP port; 445 when absent.
    #[serde(default)]
    pub port: Option<u16>,
    pub share: String,
    /// Folder inside the share shown at the root of the source
    /// (`/`-separated, empty for the share root).
    #[serde(default)]
    pub path: String,
    /// Login; guest access when absent.
    #[serde(default)]
    pub credentials: Option<Credentials>,
}

/// A Jellyfin or Emby media server (MediaBrowser API).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaBrowserConfig {
    /// Stable identifier chosen by the app.
    pub id: String,
    pub name: String,
    /// Server base URL, e.g. `http://jellyfin:8096`.
    pub url: String,
    /// User credentials (username + password).
    #[serde(default)]
    pub credentials: Option<Credentials>,
    /// Accept invalid TLS certificates.
    #[serde(default)]
    pub insecure_tls: bool,
    /// Highest video height to select among media sources.
    #[serde(default)]
    pub max_height: Option<u32>,
    /// Persisted access token from a previous session.
    #[serde(default)]
    pub access_token: Option<String>,
    /// Persisted user ID from a previous session.
    #[serde(default)]
    pub user_id: Option<String>,
}

/// One Plex server as one signed-in user. Locations use the source and
/// machine identifiers, so changing its network address preserves history.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PlexConfig {
    pub id: String,
    pub name: String,
    /// The preferred connection, or a manually supplied server address.
    pub url: String,
    /// The server's stable identity; filled when a manual source is tested.
    #[serde(default)]
    pub machine_identifier: String,
    /// The selected server's credential, which may differ from the account's.
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub connections: Vec<crate::plex::auth::PlexConnection>,
    /// Account device credentials, when linked rather than entered manually.
    #[serde(default)]
    pub auth: Option<crate::plex::auth::PlexCredentials>,
    #[serde(default)]
    pub insecure_tls: bool,
    #[serde(default)]
    pub max_height: Option<u32>,
    #[serde(default)]
    pub local_only: bool,
    #[serde(default = "plex_relay_default")]
    pub allow_relay: bool,
}

fn plex_relay_default() -> bool {
    true
}

impl fmt::Debug for PlexConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlexConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("url", &redact(&self.url))
            .field("machine_identifier", &self.machine_identifier)
            .field("access_token", &"***")
            .field("auth", &self.auth.as_ref().map(|_| "***"))
            .finish_non_exhaustive()
    }
}

fn debug_with_url(
    f: &mut fmt::Formatter<'_>,
    ty: &str,
    id: &str,
    name: &str,
    url: &str,
    creds: &Option<Credentials>,
) -> fmt::Result {
    f.debug_struct(ty)
        .field("id", &id)
        .field("name", &name)
        .field("url", &redact(url))
        .field("credentials", creds)
        .finish_non_exhaustive()
}

impl fmt::Debug for MediaBrowserConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_with_url(
            f,
            "MediaBrowserConfig",
            &self.id,
            &self.name,
            &self.url,
            &self.credentials,
        )
    }
}

impl fmt::Debug for HttpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_with_url(
            f,
            "HttpConfig",
            &self.id,
            &self.name,
            &self.url,
            &self.credentials,
        )
    }
}

impl fmt::Debug for FeedConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_with_url(
            f,
            "FeedConfig",
            &self.id,
            &self.name,
            &self.url,
            &self.credentials,
        )
    }
}

impl fmt::Debug for DlnaConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DlnaConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("location", &redact(&self.location))
            .finish_non_exhaustive()
    }
}

/// Everything needed to rebuild a source, including credentials.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceConfig {
    Local(LocalConfig),
    Library(LibraryConfig),
    /// An HTTP(S) server with autoindex listings.
    Http(HttpConfig),
    #[serde(rename = "webdav")]
    WebDav(HttpConfig),
    /// A DLNA/UPnP media server.
    Dlna(DlnaConfig),
    /// A DeoVR JSON feed.
    #[serde(rename = "deovr")]
    DeoVr(FeedConfig),
    /// A HereSphere JSON API.
    #[serde(rename = "heresphere")]
    HereSphere(FeedConfig),
    /// An SMB2/3 share.
    Smb(SmbConfig),
    /// A Jellyfin or Emby media server.
    #[serde(rename = "mediabrowser")]
    MediaBrowser(MediaBrowserConfig),
    Plex(PlexConfig),
}

impl SourceConfig {
    /// Identifier of the configured source.
    pub fn id(&self) -> &str {
        match self {
            SourceConfig::Local(c) => &c.id,
            SourceConfig::Library(c) => &c.id,
            SourceConfig::Http(c) | SourceConfig::WebDav(c) => &c.id,
            SourceConfig::Dlna(c) => &c.id,
            SourceConfig::DeoVr(c) | SourceConfig::HereSphere(c) => &c.id,
            SourceConfig::Smb(c) => &c.id,
            SourceConfig::MediaBrowser(c) => &c.id,
            SourceConfig::Plex(c) => &c.id,
        }
    }

    /// Display name of the configured source.
    pub fn name(&self) -> &str {
        match self {
            SourceConfig::Local(c) => &c.name,
            SourceConfig::Library(c) => &c.name,
            SourceConfig::Http(c) | SourceConfig::WebDav(c) => &c.name,
            SourceConfig::Dlna(c) => &c.name,
            SourceConfig::DeoVr(c) | SourceConfig::HereSphere(c) => &c.name,
            SourceConfig::Smb(c) => &c.name,
            SourceConfig::MediaBrowser(c) => &c.name,
            SourceConfig::Plex(c) => &c.name,
        }
    }

    /// Kind of source this configuration builds.
    pub fn kind(&self) -> SourceKind {
        match self {
            SourceConfig::Local(_) => SourceKind::Local,
            SourceConfig::Library(_) => SourceKind::Library,
            SourceConfig::Http(_) => SourceKind::Http,
            SourceConfig::WebDav(_) => SourceKind::WebDav,
            SourceConfig::Dlna(_) => SourceKind::Dlna,
            SourceConfig::DeoVr(_) => SourceKind::DeoVr,
            SourceConfig::HereSphere(_) => SourceKind::HereSphere,
            SourceConfig::Smb(_) => SourceKind::Smb,
            SourceConfig::MediaBrowser(_) => SourceKind::MediaBrowser,
            SourceConfig::Plex(_) => SourceKind::Plex,
        }
    }

    /// Where the source is (folder, URL, share), without secrets.
    pub fn address(&self) -> String {
        match self {
            SourceConfig::Local(c) => c.root.display().to_string(),
            SourceConfig::Library(c) => match &c.card {
                Some(card) if c.folder.as_os_str().is_empty() => card.label.clone(),
                Some(card) => format!("{}/{}", card.label, c.folder.display()),
                None => c.folder.display().to_string(),
            },
            SourceConfig::Http(c) | SourceConfig::WebDav(c) => redact(&c.url),
            SourceConfig::Dlna(c) => redact(&c.location),
            SourceConfig::DeoVr(c) | SourceConfig::HereSphere(c) => redact(&c.url),
            SourceConfig::Smb(c) => crate::smb::smb_url(&c.host, c.port, &c.share, &c.path),
            SourceConfig::MediaBrowser(c) => redact(&c.url),
            SourceConfig::Plex(c) => redact(&c.url),
        }
    }

    /// One-line description without secrets, for logs and the UI.
    pub fn describe(&self) -> String {
        format!(
            "{} \"{}\" ({})",
            self.kind().label(),
            self.name(),
            self.address()
        )
    }
}

/// On-disk layout of the sources file.
#[derive(Serialize, Deserialize)]
struct ConfigFile {
    version: u32,
    sources: Vec<SourceConfig>,
}

/// Default path of the sources file: `$XDG_CONFIG_HOME/frameplayerx/sources.json`.
pub fn default_config_path() -> PathBuf {
    fp_core::dirs::config_dir().join("sources.json")
}

/// Writes source configurations (with credentials) as JSON readable only by
/// the user: the file gets mode 0600 and a newly created folder 0700. The
/// write is atomic (temporary file + rename).
pub fn save_configs(path: &Path, configs: &[SourceConfig]) -> Result<()> {
    let mut json = serde_json::to_vec_pretty(&ConfigFile {
        version: 1,
        sources: configs.to_vec(),
    })
    .map_err(|e| Error::Config(format!("cannot serialize sources: {e}")))?;
    json.push(b'\n');
    write_private(path, &json)?;
    Ok(())
}

/// Writes `data` to `path` readable only by the user (mode 0600, a newly
/// created folder 0700), atomically (temporary file + rename).
pub(crate) fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        builder.mode(0o700);
        builder.create(dir)?;
    }
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&tmp)?;
        // `mode` only applies on creation; tighten a leftover temp file too.
        #[cfg(unix)]
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(data)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Reads source configurations written by [`save_configs`]. A missing file
/// is an empty list. A file readable by others is tightened to 0600.
pub fn load_configs(path: &Path) -> Result<Vec<SourceConfig>> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        log::warn!(
            "{} was readable by other users; restricting to 0600",
            path.display()
        );
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    // Accept the versioned object and a bare array.
    let value: serde_json::Value = serde_json::from_slice(&data)
        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
    let sources = if value.is_array() {
        serde_json::from_value(value)
    } else {
        serde_json::from_value::<ConfigFile>(value).map(|f| f.sources)
    };
    sources.map_err(|e| Error::Config(format!("{}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<SourceConfig> {
        vec![
            SourceConfig::Local(LocalConfig {
                id: "l".into(),
                name: "Videos".into(),
                root: "/home/deck/Videos".into(),
            }),
            SourceConfig::WebDav(HttpConfig {
                id: "w".into(),
                name: "NAS".into(),
                url: "webdavs://nas/vr/".into(),
                credentials: Some(Credentials::new("bob", "hunter2")),
                insecure_tls: true,
            }),
            SourceConfig::DeoVr(FeedConfig {
                id: "d".into(),
                name: "Feed".into(),
                url: "http://vr:9999/deovr".into(),
                credentials: None,
                insecure_tls: false,
                max_height: Some(2880),
            }),
            SourceConfig::Smb(SmbConfig {
                id: "s".into(),
                name: "Share".into(),
                host: "nas.local".into(),
                port: None,
                share: "media".into(),
                path: "vr".into(),
                credentials: Some(Credentials::new("bob", "hunter2")),
            }),
            SourceConfig::MediaBrowser(MediaBrowserConfig {
                id: "mb".into(),
                name: "Jellyfin".into(),
                url: "http://jellyfin:8096".into(),
                credentials: Some(Credentials::new("alice", "hunter2")),
                insecure_tls: false,
                max_height: Some(2880),
                access_token: None,
                user_id: None,
            }),
        ]
    }

    #[test]
    fn save_and_load_roundtrip_with_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/dir/sources.json");
        save_configs(&path, &sample()).unwrap();
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let dmode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dmode, 0o700);
        }
        assert_eq!(load_configs(&path).unwrap(), sample());
        // Overwrite keeps 0600 and leaves no temp file.
        save_configs(&path, &sample()[..1]).unwrap();
        assert_eq!(load_configs(&path).unwrap().len(), 1);
        assert!(!dir.path().join("sub/dir/sources.json.tmp").exists());
    }

    #[test]
    fn load_missing_is_empty_and_tightens_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("none.json");
        assert!(load_configs(&path).unwrap().is_empty());
        std::fs::write(&path, "[]").unwrap();
        #[cfg(unix)]
        {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(load_configs(&path).unwrap().is_empty());
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::write(&path, "{nope").unwrap();
        assert!(matches!(load_configs(&path), Err(Error::Config(_))));
    }

    #[test]
    fn json_shape_is_tagged() {
        let j = serde_json::to_string(&sample()[1]).unwrap();
        assert!(j.contains("\"type\":\"webdav\""), "{j}");
        let j = serde_json::to_string(&sample()[2]).unwrap();
        assert!(j.contains("\"type\":\"deovr\""), "{j}");
        let j = serde_json::to_string(&sample()[4]).unwrap();
        assert!(j.contains("\"type\":\"mediabrowser\""), "{j}");
        // Optional fields may be omitted.
        let c: SourceConfig =
            serde_json::from_str(r#"{"type":"http","id":"h","name":"H","url":"http://h/"}"#)
                .unwrap();
        assert_eq!(c.kind(), SourceKind::Http);
    }

    #[test]
    fn debug_and_describe_hide_passwords() {
        let mut configs = sample();
        configs.push(SourceConfig::Http(HttpConfig {
            id: "h".into(),
            name: "H".into(),
            url: "http://bob:hunter2@h/".into(),
            credentials: None,
            insecure_tls: false,
        }));
        for c in &configs {
            let dbg = format!("{c:?}");
            assert!(!dbg.contains("hunter2"), "{dbg}");
            assert!(!c.describe().contains("hunter2"), "{}", c.describe());
        }
        assert_eq!(configs[0].id(), "l");
        assert_eq!(configs[3].name(), "Share");
        assert!(configs[3].describe().contains("smb://nas.local/media/vr"));
    }
}
