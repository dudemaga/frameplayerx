//! Plex Media Server: account-selected connections, stable source-owned
//! locations and authenticated reads through the existing HTTP byte source.

pub mod auth;
mod catalog;
mod playback;
mod transcode;

use crate::catalog::{Catalog, CatalogItem};
use crate::config::{PlexConfig, SourceConfig, SourceKind};
use crate::error::{Error, Result};
use crate::http::{HttpClient, HttpOptions, HttpReply};
use crate::json::{get, text};
use crate::urlutil::{encode_form_value, redact, same_origin};
use crate::{Sidecars, Source, WatchEvent, WatchSession};
use fp_core::source::{ByteSource, Entry};
use fp_core::sync::lock;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use url::Url;

use auth::{PlexAuthClient, PlexConnection};

const JSON_LIMIT: u64 = 32 << 20;
const PAGE_LIMIT: usize = 200;
/// The longest listing read whole.
const LISTING_LIMIT: usize = 100_000;
/// How long a connection that failed is tried after the others.
const FAILED_FOR: Duration = Duration::from_secs(60);
/// How long a video's details are reused: opening one reads them several
/// times, each a round trip to the server.
const METADATA_FRESH: Duration = Duration::from_secs(10);
/// The most a poster and a backdrop are asked for, as on Jellyfin.
const POSTER: (u32, u32) = (480, 720);
const BACKDROP: (u32, u32) = (1280, 720);

/// Whether an address names Plex's web app rather than a media server.
pub fn is_web_app(address: &str) -> bool {
    Url::parse(&crate::urlutil::with_http_scheme(address.trim()))
        .ok()
        .is_some_and(|u| u.host_str() == Some("app.plex.tv"))
}

/// The exact configured source owning a persisted Plex media or image
/// location. This never accepts ordinary network URLs.
pub fn owner(location: &str) -> Option<String> {
    let url = Url::parse(location).ok()?;
    if url.scheme() != "plex" || url.host_str() != Some("source") {
        return None;
    }
    let mut parts = url.path_segments()?;
    percent_encoding::percent_decode_str(parts.next()?)
        .decode_utf8()
        .ok()
        .map(std::borrow::Cow::into_owned)
}

/// Whether `location` is a Plex video, whose last path segment is the
/// server's ID for it rather than a name.
pub(crate) fn is_item(location: &str) -> bool {
    Url::parse(location).is_ok_and(|url| {
        url.scheme() == "plex"
            && url.host_str() == Some("source")
            && url
                .path_segments()
                .is_some_and(|mut p| p.nth(2) == Some("item"))
    })
}

#[derive(Clone)]
struct Endpoint {
    base: String,
    client: HttpClient,
    relay: bool,
}

/// A configured server as one Plex principal. Construction does no I/O.
pub struct PlexSource {
    config: Mutex<PlexConfig>,
    endpoint: Mutex<Option<Endpoint>>,
    failed_connections: Mutex<HashMap<String, Instant>>,
    connecting: Mutex<()>,
    refreshing: Mutex<()>,
    auth_api: PlexAuthClient,
    changed: AtomicBool,
    versions: Mutex<HashMap<String, String>>,
    /// Recent details, and when they were read. Watch sessions are made from
    /// them without network I/O.
    metadata: Mutex<HashMap<String, (Instant, Value)>>,
    facets: Mutex<HashMap<String, String>>,
    encoding: Arc<Mutex<Option<String>>>,
    id: String,
    name: String,
}

impl PlexSource {
    pub fn new(config: &PlexConfig) -> Result<Self> {
        let url = server_url(&config.url)?;
        if config.id.is_empty() || config.id.contains(['/', '?', '#']) {
            return Err(Error::Config(
                "Plex source needs a stable identifier".into(),
            ));
        }
        let mut config = config.clone();
        config.url = url;
        Ok(Self {
            id: config.id.clone(),
            name: config.name.clone(),
            config: Mutex::new(config),
            endpoint: Mutex::new(None),
            failed_connections: Default::default(),
            connecting: Mutex::new(()),
            refreshing: Mutex::new(()),
            auth_api: PlexAuthClient::new(),
            changed: AtomicBool::new(false),
            versions: Default::default(),
            metadata: Default::default(),
            facets: Default::default(),
            encoding: Default::default(),
        })
    }

    /// Tests credentials and server identity, returning the chosen connection
    /// and completed identity for the source form to save.
    pub fn probe(&self) -> Result<PlexConfig> {
        self.client()?;
        self.libraries()?;
        Ok(lock(&self.config).clone())
    }

    fn options(config: &PlexConfig) -> HttpOptions {
        HttpOptions {
            insecure_tls: config.insecure_tls,
            connect_timeout: Duration::from_secs(3),
            max_retries: 1,
            ..Default::default()
        }
    }

    fn make_client(config: &PlexConfig, base: &str) -> HttpClient {
        let client_id = config
            .auth
            .as_ref()
            .map_or(config.id.as_str(), |a| a.client_identifier.as_str());
        HttpClient::new_with_headers(
            Self::options(config),
            vec![
                ("X-Plex-Token".into(), config.access_token.clone()),
                ("X-Plex-Client-Identifier".into(), client_id.to_string()),
                ("X-Plex-Product".into(), "FramePlayerX".into()),
                ("X-Plex-Version".into(), env!("CARGO_PKG_VERSION").into()),
                ("X-Plex-Device".into(), "Steam Frame".into()),
                ("X-Plex-Platform".into(), "Linux".into()),
                ("X-Plex-Pms-Api-Version".into(), "1.0".into()),
            ],
            base,
        )
    }

    fn refresh(&self, force: bool) -> Result<()> {
        let _guard = lock(&self.refreshing);
        let before = lock(&self.config).clone();
        let Some(auth) = &before.auth else {
            return Ok(());
        };
        let now = unix_now();
        if !force
            && auth
                .expires_at
                .is_none_or(|at| at > now.saturating_add(120))
        {
            return Ok(());
        }
        // A pasted token has no registered device key, so cannot be refreshed.
        if auth.device_key.is_none() {
            return if force {
                Err(Error::SignedOut(self.name.clone()))
            } else {
                Ok(())
            };
        }
        let credentials = self.auth_api.refresh(auth)?;
        let resources = self.auth_api.resources(&credentials)?;
        let resource = resources
            .into_iter()
            .find(|r| r.machine_identifier == before.machine_identifier)
            .ok_or_else(|| {
                Error::Auth("Plex account no longer has access to this server".into())
            })?;
        let mut config = lock(&self.config);
        config.access_token = resource
            .access_token
            .unwrap_or_else(|| credentials.access_token.clone());
        config.auth = Some(credentials);
        config.connections = resource.connections;
        *lock(&self.endpoint) = None;
        lock(&self.failed_connections).clear();
        self.changed.store(true, Ordering::Release);
        Ok(())
    }

    fn client(&self) -> Result<Endpoint> {
        self.refresh(false)?;
        match self.connect_client() {
            Err(Error::SignedOut(_)) => {
                self.refresh(true)?;
                self.connect_client()
            }
            result => result,
        }
    }

    fn connect_client(&self) -> Result<Endpoint> {
        if let Some(endpoint) = lock(&self.endpoint).clone() {
            return Ok(endpoint);
        }
        let _guard = lock(&self.connecting);
        if let Some(endpoint) = lock(&self.endpoint).clone() {
            return Ok(endpoint);
        }
        let config = lock(&self.config).clone();
        if config.access_token.is_empty() {
            return Err(Error::Auth("Plex server needs an access token".into()));
        }
        let mut last = Error::Http {
            url: redact(&config.url),
            message: "no reachable Plex server connection".into(),
        };
        let mut candidates = connections(&config);
        // One that just failed goes last rather than out: it may be the only
        // one, and the server may be back already.
        candidates.sort_by_key(|c| {
            lock(&self.failed_connections)
                .get(c.uri.trim_end_matches('/'))
                .is_some_and(|at| at.elapsed() < FAILED_FOR)
        });
        for candidate in candidates {
            let base = match server_url(&candidate.uri) {
                Ok(base) => base,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            // Verify the expected machine before sending its credential.
            let identity = address_fallback(
                HttpClient::new(Self::options(&config), None).scoped_to(&base),
                &base,
                &candidate,
            )
            .request(
                "GET",
                &format!("{base}/identity"),
                &[("Accept", "application/json")],
                None,
                JSON_LIMIT,
            )
            .and_then(|r| r.error_for_status(&base))
            .and_then(|r| serde_json::from_slice::<Value>(&r.body).map_err(Error::from));
            let identity = match identity {
                Ok(v) => v,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            let Some(machine) = text(container(&identity), "machineIdentifier") else {
                last = Error::parse("Plex identity", "missing machine identifier");
                continue;
            };
            if !config.machine_identifier.is_empty() && machine != config.machine_identifier {
                last =
                    Error::Auth("Plex server identity does not match the selected server".into());
                continue;
            }
            let endpoint = Endpoint {
                client: address_fallback(Self::make_client(&config, &base), &base, &candidate),
                base: base.clone(),
                relay: candidate.relay,
            };
            let response = endpoint
                .client
                .request(
                    "GET",
                    &format!("{base}/"),
                    &[("Accept", "application/json")],
                    None,
                    JSON_LIMIT,
                )
                .and_then(|r| {
                    if matches!(r.status, 401 | 498) && config.auth.is_some() {
                        Err(Error::SignedOut(self.name.clone()))
                    } else {
                        r.error_for_status(&base)
                    }
                });
            if let Err(e) = response {
                if matches!(e, Error::SignedOut(_)) {
                    return Err(e);
                }
                last = e;
                continue;
            }
            {
                let mut current = lock(&self.config);
                if current.url != base || current.machine_identifier != machine {
                    current.url = base;
                    current.machine_identifier = machine;
                    self.changed.store(true, Ordering::Release);
                }
            }
            *lock(&self.endpoint) = Some(endpoint.clone());
            return Ok(endpoint);
        }
        Err(last)
    }

    fn request(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> Result<HttpReply> {
        let mut renewed = false;
        let mut reconnected = false;
        loop {
            let endpoint = self.client()?;
            let url = resource_url(&endpoint.base, path)?;
            let reply = endpoint
                .client
                .request(method, &url, headers, None, JSON_LIMIT);
            if !renewed
                && reply.as_ref().is_ok_and(|r| matches!(r.status, 401 | 498))
                && lock(&self.config).auth.is_some()
            {
                self.refresh(true)?;
                renewed = true;
                continue;
            }
            // A cached LAN address can disappear when the headset leaves
            // home. Retry reads through another identity-checked connection.
            let unavailable = match &reply {
                Ok(r) => matches!(r.status, 502..=504),
                Err(
                    Error::Http { .. }
                    | Error::Io(_)
                    | Error::HttpStatus {
                        status: 502..=504, ..
                    },
                ) => true,
                _ => false,
            };
            if method == "GET" && unavailable && !reconnected {
                lock(&self.failed_connections).insert(endpoint.base.clone(), Instant::now());
                *lock(&self.endpoint) = None;
                reconnected = true;
                continue;
            }
            return reply?.error_for_status(&url);
        }
    }

    fn json(&self, path: &str) -> Result<Value> {
        let response = self.request("GET", path, &[("Accept", "application/json")])?;
        serde_json::from_slice(&response.body).map_err(Error::from)
    }

    fn raw_item(&self, id: &str) -> Result<Value> {
        if let Some((read, item)) = lock(&self.metadata).get(id)
            && read.elapsed() < METADATA_FRESH
        {
            return Ok(item.clone());
        }
        let v = self.json(&format!(
            "/library/metadata/{}?includeChapters=1",
            encode_form_value(id)
        ))?;
        let item = crate::json::array(container(&v), "Metadata")
            .first()
            .cloned()
            .ok_or_else(|| Error::NotFound("Plex item is no longer available".into()))?;
        let mut metadata = lock(&self.metadata);
        if metadata.len() >= 128 {
            metadata.clear();
        }
        metadata.insert(id.into(), (Instant::now(), item.clone()));
        Ok(item)
    }

    fn location(&self, kind: &str, id: &str, media: Option<&str>) -> String {
        let config = lock(&self.config);
        let mut url = Url::parse("plex://source/").expect("a constant URL");
        // Identifiers are carried as path segments rather than interpolated
        // into a network host: arbitrary source IDs remain exact.
        url.path_segments_mut()
            .expect("a hierarchical URL")
            .extend([
                config.id.as_str(),
                config.machine_identifier.as_str(),
                kind,
                id,
            ]);
        if let Some(media) = media {
            url.query_pairs_mut().append_pair("media", media);
        }
        url.to_string()
    }

    fn asset(&self, kind: &str, path: &str) -> Option<String> {
        if !safe_resource(path) {
            return None;
        }
        let mut url = Url::parse(&self.location(kind, "resource", None)).ok()?;
        url.query_pairs_mut().append_pair("path", path);
        Some(url.to_string())
    }

    /// A picture as the server scales it to fit `(width, height)`: the
    /// originals can be larger than the image cache takes.
    fn image(&self, path: &str, (width, height): (u32, u32)) -> Option<String> {
        if !safe_resource(path) {
            return None;
        }
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("width", &width.to_string())
            .append_pair("height", &height.to_string())
            .append_pair("upscale", "0")
            .append_pair("url", path);
        self.asset("image", &format!("/photo/:/transcode?{}", query.finish()))
    }

    fn parse_location(&self, location: &str) -> Result<Location> {
        let url = Url::parse(location)
            .map_err(|_| Error::invalid(location, "not a Plex source location"))?;
        if url.scheme() != "plex" || url.host_str() != Some("source") {
            return Err(Error::invalid(location, "not a Plex source location"));
        }
        let parts: Vec<String> = url
            .path_segments()
            .into_iter()
            .flatten()
            .map(|s| {
                percent_encoding::percent_decode_str(s)
                    .decode_utf8()
                    .map(std::borrow::Cow::into_owned)
            })
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| Error::invalid(location, "invalid Plex location encoding"))?;
        let config = lock(&self.config);
        if parts.len() != 4 || parts[0] != config.id || parts[1] != config.machine_identifier {
            return Err(Error::invalid(
                location,
                "Plex location belongs to another source or server",
            ));
        }
        let mut media = None;
        let mut path = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "media" => media = Some(value.into_owned()),
                "path" => path = Some(value.into_owned()),
                _ => return Err(Error::invalid(location, "unknown Plex location field")),
            }
        }
        Ok(Location {
            kind: parts[2].clone(),
            id: parts[3].clone(),
            media,
            path,
        })
    }
}

#[derive(Clone)]
struct Location {
    kind: String,
    id: String,
    media: Option<String>,
    path: Option<String>,
}

impl Source for PlexSource {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> SourceKind {
        SourceKind::Plex
    }
    fn describe(&self) -> String {
        format!("Plex {}", redact(&lock(&self.config).url))
    }

    fn list(&self, location: Option<&str>) -> Result<Vec<Entry>> {
        match location {
            None => Ok(self
                .libraries()?
                .iter()
                .map(CatalogItem::to_entry)
                .collect()),
            Some(location) => {
                let location = self.parse_location(location)?;
                let parent = if location.kind == "section" {
                    format!("section:{}", location.id)
                } else {
                    location.id
                };
                let items = self.whole_listing(crate::catalog::CatalogQuery {
                    parent: Some(parent),
                    ..Default::default()
                })?;
                Ok(items.iter().map(CatalogItem::to_entry).collect())
            }
        }
    }

    fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>> {
        self.open_resource(location, None)
    }
    fn open_playback(&self, location: &str, session: &WatchSession) -> Result<Arc<dyn ByteSource>> {
        self.open_resource(location, Some(session))
    }
    fn picture_request(&self, location: &str) -> Option<(HttpClient, String)> {
        let location = self.parse_location(location).ok()?;
        if location.kind != "image" {
            return None;
        }
        let endpoint = self.client().ok()?;
        let url = resource_url(&endpoint.base, &location.path?).ok()?;
        Some((endpoint.client, url))
    }
    fn details(&self, entry: &Entry) -> Result<Entry> {
        self.entry_details(entry)
    }
    fn sidecars(&self, video: &Entry) -> Result<Sidecars> {
        Ok(Sidecars::from_entry(&self.entry_details(video)?))
    }
    fn watch_session(&self, video: &Entry) -> Option<WatchSession> {
        self.make_session(video).ok()
    }
    fn report_watch(&self, session: &WatchSession, event: WatchEvent) -> Result<()> {
        self.timeline(session, event)
    }
    fn playback_tracks(&self, video: &Entry) -> Result<Vec<crate::PlaybackTrack>> {
        self.source_tracks(video)
    }
    fn transcode_targets(&self, video: &Entry) -> Vec<crate::TranscodeTarget> {
        self.targets(video).unwrap_or_default()
    }
    fn open_transcode(
        &self,
        location: &str,
        target: &str,
        start: f64,
    ) -> Result<Arc<dyn ByteSource>> {
        self.encode(location, target, start, None)
    }
    fn open_transcode_session(
        &self,
        location: &str,
        target: &str,
        start: f64,
        session: &WatchSession,
    ) -> Result<Arc<dyn ByteSource>> {
        self.encode(location, target, start, Some(session))
    }
    fn as_catalog(&self) -> Option<&dyn Catalog> {
        Some(self)
    }
    fn updated_config(&self) -> Option<SourceConfig> {
        self.changed
            .swap(false, Ordering::AcqRel)
            .then(|| SourceConfig::Plex(lock(&self.config).clone()))
    }
}

impl fmt::Debug for PlexSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlexSource")
            .field("config", &*lock(&self.config))
            .finish_non_exhaustive()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
fn container(value: &Value) -> &Value {
    get(value, "MediaContainer").unwrap_or(value)
}

fn server_url(raw: &str) -> Result<String> {
    let url = Url::parse(raw).map_err(|_| Error::invalid(raw, "expected a Plex server URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::invalid(
            raw,
            "expected an HTTP(S) Plex server URL without credentials or query",
        ));
    }
    if url
        .host_str()
        .is_some_and(|h| h.eq_ignore_ascii_case("app.plex.tv"))
    {
        return Err(Error::Config(
            "app.plex.tv is the Plex web app; use Link with code to select your server".into(),
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn safe_resource(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains(['\\', '\r', '\n'])
        && !path.contains("..")
        && !path.to_ascii_lowercase().contains("x-plex-token")
}

fn resource_url(base: &str, path: &str) -> Result<String> {
    if !safe_resource(path) {
        return Err(Error::invalid(
            path,
            "Plex resource must stay on its server",
        ));
    }
    let url = format!("{base}{path}");
    if !same_origin(base, &url) {
        return Err(Error::invalid(
            path,
            "Plex resource must stay on its server",
        ));
    }
    Ok(url)
}

fn connections(config: &PlexConfig) -> Vec<PlexConnection> {
    let mut choices = config.connections.clone();
    if !choices
        .iter()
        .any(|c| c.uri.trim_end_matches('/') == config.url)
    {
        let local = Url::parse(&config.url).is_ok_and(|u| private_address(&u));
        choices.push(PlexConnection {
            uri: config.url.clone(),
            address: None,
            port: None,
            local,
            relay: false,
            protocol: Url::parse(&config.url)
                .ok()
                .map_or_else(String::new, |u| u.scheme().into()),
        });
    }
    // The token goes with every request. An address someone typed is theirs
    // to choose, but one plex.tv lists has to be encrypted or on the LAN.
    let typed = config.connections.is_empty();
    choices.retain(|c| {
        (!config.local_only || c.local)
            && (config.allow_relay || !c.relay)
            && (typed
                || Url::parse(&c.uri).is_ok_and(|u| u.scheme() == "https" || private_address(&u)))
    });
    choices.sort_by_key(|c| {
        (
            c.relay,
            !c.local,
            c.protocol != "https",
            c.uri != config.url,
        )
    });
    choices.dedup_by(|a, b| a.uri == b.uri);
    choices
}

fn private_address(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
        }
        Some(url::Host::Domain(host)) => {
            host == "localhost" || host.ends_with(".local") || !host.contains('.')
        }
        None => false,
    }
}

fn address_fallback(client: HttpClient, base: &str, connection: &PlexConnection) -> HttpClient {
    let Ok(url) = Url::parse(base) else {
        return client;
    };
    if connection.relay
        || !url
            .host_str()
            .is_some_and(|host| host.ends_with(".plex.direct"))
    {
        return client;
    }
    let Some((address, port)) = connection
        .address
        .zip(connection.port.or(url.port_or_known_default()))
    else {
        log::info!(
            "Plex connection {} has no advertised IP address for fallback",
            redact(base)
        );
        return client;
    };
    // Connect to this advertised endpoint without changing its TLS identity.
    client.with_address_fallback(std::net::SocketAddr::new(address, port))
}

#[cfg(test)]
mod tests;
