//! Long-lived subsystems: media sources, the library and its metadata
//! worker, haptic devices and the remote-control servers.

use crate::drives::REMOVABLE_ROOT;
use crate::image_cache::ImageCache;
use crate::settings::{HapticDeviceConfig, Settings};
use fp_core::ByteSource;
use fp_core::source::FileSource;
use fp_core::sync::{read, write};
use fp_core::volume;
use fp_haptics::HapticsEngine;
use fp_library::Library;
use fp_remote::{RemoteEvent, RemoteHub};
use fp_sources::{Source, SourceConfig};
use ring::rand::SecureRandom;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

fn not_inserted() -> &'static str {
    tr!("open-not-inserted")
}

/// What changing the source list did.
pub struct SourcesChange {
    /// Sources that couldn't be set up.
    pub errors: Vec<SourceError>,
    /// Why the list couldn't be saved, when it couldn't.
    pub save_error: Option<String>,
}

/// A configured source that could not be built.
#[derive(Debug)]
pub struct SourceError {
    pub id: String,
    pub name: String,
    pub error: fp_sources::Error,
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.name, self.error)
    }
}

/// Resolves locations to readable byte sources: local paths directly,
/// everything else through the configured source that owns it.
pub struct Opener {
    sources: RwLock<HashMap<String, Arc<dyn Source>>>,
    http: fp_sources::http::HttpClient,
}

impl Opener {
    pub fn new() -> Opener {
        Opener {
            sources: RwLock::new(HashMap::new()),
            http: fp_sources::http::HttpClient::new(Default::default(), None),
        }
    }

    pub fn set_sources(&self, configs: &[SourceConfig]) -> Vec<SourceError> {
        let mut map = HashMap::new();
        let mut errors = Vec::new();
        for c in builtin_sources().iter().chain(configs) {
            match fp_sources::build(c) {
                Ok(s) => {
                    map.insert(c.id().to_string(), Arc::<dyn Source>::from(s));
                }
                Err(error) => errors.push(SourceError {
                    id: c.id().to_string(),
                    name: c.name().to_string(),
                    error,
                }),
            }
        }
        *write(&self.sources) = map;
        errors
    }

    pub fn source(&self, id: &str) -> Option<Arc<dyn Source>> {
        read(&self.sources).get(id).cloned()
    }

    /// Adds a source a test made.
    #[cfg(test)]
    pub fn insert_source(&self, source: Arc<dyn Source>) {
        write(&self.sources).insert(source.id().to_string(), source);
    }

    /// Opens `location`, preferring the source `source_id` when given.
    pub fn open_in(
        &self,
        source_id: Option<&str>,
        location: &str,
    ) -> Result<Arc<dyn ByteSource>, String> {
        if fp_sources::is_source_location(location) {
            let owner = fp_sources::location_owner(location).ok_or(tr!("open-no-source"))?;
            if source_id.is_some_and(|s| s != owner) {
                return Err(tr!("open-no-source").into());
            }
            return self
                .source(&owner)
                .ok_or(tr!("open-no-source"))?
                .open(location)
                .map_err(|e| crate::ui::errors::source(&e));
        }
        if location.starts_with(volume::SCHEME) {
            let path = volume::resolve(location).ok_or(not_inserted())?;
            return self.open_in(None, &path);
        }
        if location.starts_with('/') {
            let path = Path::new(location);
            if !path.exists() && location.starts_with(&format!("{REMOVABLE_ROOT}/")) {
                return Err(not_inserted().into());
            }
            return FileSource::open(path)
                .map(|f| Arc::new(f) as Arc<dyn ByteSource>)
                .map_err(|e| format!("{location}: {e}"));
        }
        if let Some(path) = location.strip_prefix("file://") {
            return self.open_in(None, path);
        }
        if let Some(src) = source_id.and_then(|id| self.source(id)) {
            return src
                .open(location)
                .map_err(|e| crate::ui::errors::source(&e));
        }
        // Try sources whose scheme fits, then plain HTTP.
        let sources: Vec<Arc<dyn Source>> = read(&self.sources).values().cloned().collect();
        let mut last = tr!("open-no-source").to_string();
        for s in sources {
            match s.open(location) {
                Ok(b) => return Ok(b),
                Err(e) => last = crate::ui::errors::source(&e),
            }
        }
        if fp_sources::urlutil::is_http_url(location) {
            return self
                .http
                .open(location)
                .map(|f| Arc::new(f) as Arc<dyn ByteSource>)
                .map_err(|e| crate::ui::errors::source(&e));
        }
        Err(last)
    }

    pub fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>, String> {
        self.open_in(None, location)
    }

    /// A small file (a script) whole, through the source it came from when
    /// that can open it (an SMB share, a server that wants a login), else
    /// as [`Self::fetch`].
    pub fn fetch_in(
        &self,
        source_id: Option<&str>,
        location: &str,
        limit: u64,
    ) -> Result<Vec<u8>, String> {
        if fp_sources::is_source_location(location) {
            return self
                .open_in(source_id, location)
                .and_then(|f| read_whole(&*f, limit));
        }
        if let Some(src) = source_id.and_then(|id| self.source(id))
            && !location.starts_with('/')
            && !location.starts_with(volume::SCHEME)
        {
            let read = src
                .open(location)
                .map_err(|e| crate::ui::errors::source(&e))
                .and_then(|f| read_whole(&*f, limit));
            match read {
                Ok(bytes) => return Ok(bytes),
                Err(e) if !fp_sources::urlutil::is_http_url(location) => return Err(e),
                Err(e) => log::debug!("{location} through its source: {e}"),
            }
        }
        self.fetch(location, limit)
    }

    /// A source's picture as an HTTP request, when it is one (see
    /// [`Source::picture_request`]).
    pub fn picture_request(
        &self,
        location: &str,
    ) -> Option<(fp_sources::http::HttpClient, String)> {
        let owner = fp_sources::location_owner(location)?;
        self.source(&owner)?.picture_request(location)
    }

    /// Small downloads (thumbnails, scripts) from any http(s) URL.
    pub fn fetch(&self, url: &str, limit: u64) -> Result<Vec<u8>, String> {
        if fp_sources::is_source_location(url) {
            return self.open(url).and_then(|f| read_whole(&*f, limit));
        }
        if url.starts_with(volume::SCHEME) {
            let path = volume::resolve(url).ok_or(not_inserted())?;
            return self.fetch(&path, limit);
        }
        if url.starts_with('/') {
            return std::fs::read(url).map_err(|e| e.to_string());
        }
        self.http
            .get_bytes(url, limit)
            .map_err(|e| crate::ui::errors::source(&e))
    }
}

/// All of `f`, refused past `limit` bytes.
fn read_whole(f: &dyn ByteSource, limit: u64) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 64 << 10];
    loop {
        let n = f
            .read_at(out.len() as u64, &mut chunk)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&chunk[..n]);
        if out.len() as u64 > limit {
            let mb = limit >> 20;
            return Err(tr!("error-too-large", mb = mb));
        }
    }
}

impl Default for Opener {
    fn default() -> Self {
        Opener::new()
    }
}

/// Sources that always exist: the home folder and removable drives.
pub fn builtin_sources() -> Vec<SourceConfig> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        out.push(SourceConfig::Local(fp_sources::LocalConfig {
            id: HOME_SOURCE.into(),
            name: tr!("sources-this-device").into(),
            root: home,
        }));
    }
    // Always offered, so a card inserted later shows up without a restart.
    out.push(SourceConfig::Local(fp_sources::LocalConfig {
        id: REMOVABLE_SOURCE.into(),
        name: "microSD and USB drives".into(),
        root: PathBuf::from(REMOVABLE_ROOT),
    }));
    out
}

pub const HOME_SOURCE: &str = "device-home";
pub const REMOVABLE_SOURCE: &str = "device-removable";
/// The headset's folders and drives as one place to browse (not a
/// [`Source`] of its own).
pub const LOCAL_HUB: &str = "on-device";

pub struct Services {
    device_identifier: String,
    source_save_retry: Option<std::time::Instant>,
    pub library: Library,
    pub opener: Arc<Opener>,
    pub images: Arc<ImageCache>,
    pub source_configs: Vec<SourceConfig>,
    pub worker: Option<fp_library::MetadataWorker>,
    pub worker_events: Option<std::sync::mpsc::Receiver<fp_library::WorkerEvent>>,
    /// Holds the metadata worker off (see [`crate::prober::FfmpegProber`]).
    worker_busy: Arc<std::sync::atomic::AtomicBool>,
    /// Plays scripts on the devices; only while haptics are turned on.
    haptics: Option<HapticsEngine>,
    /// The configured devices that were tried, in the order they answered.
    haptic_devices: Vec<HapticDeviceSlot>,
    /// Counts the times haptics were turned off, so a connection that was
    /// started before is not taken.
    haptics_generation: u64,
    pub remote: Option<RemoteHub>,
    pub remote_error: Option<String>,
    /// Sends watch sync reports to servers.
    pub watch: crate::watch::Reports,
}

/// A configured haptic device and how connecting it went.
pub struct HapticDeviceSlot {
    pub config: HapticDeviceConfig,
    /// Its id in the haptics engine, once connected.
    pub id: Option<fp_haptics::DeviceId>,
    /// Why it could not connect.
    pub error: Option<String>,
}

fn worker_options(settings: &Settings) -> fp_library::WorkerOptions {
    fp_library::WorkerOptions {
        thumbnails: settings.make_thumbnails,
        resume_thumbnails: settings.resume_frames,
        ..Default::default()
    }
}

/// This installation's id for servers that tell devices apart, made once
/// and kept in `path`: Jellyfin and Emby sign a device out when another
/// installation signs in with the same id.
fn device_id(path: &Path) -> String {
    if let Ok(saved) = std::fs::read_to_string(path) {
        let id = saved.trim();
        let valid = |c: char| c.is_ascii_alphanumeric() || c == '-';
        if !id.is_empty() && id.len() <= 64 && id.chars().all(valid) {
            return id.to_string();
        }
    }
    let mut bytes = [0u8; 16];
    if ring::rand::SystemRandom::new().fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        bytes = (nanos ^ u128::from(std::process::id())).to_le_bytes();
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let id = format!("frameplayerx-{hex}");
    let saved = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(path, &id));
    if let Err(e) = saved {
        log::warn!("cannot keep this device's id in {}: {e}", path.display());
    }
    id
}

/// Keep record IDs and their user data while replacing transport addresses.
/// Canonicalizers inspect configuration or cached metadata; migration does no I/O.
fn migrate_source_locations(library: &Library, opener: &Opener, configs: &[SourceConfig]) {
    for config in configs {
        let Some(source) = opener.source(config.id()) else {
            continue;
        };
        if matches!(
            source.kind(),
            fp_sources::SourceKind::Local | fp_sources::SourceKind::Library
        ) {
            continue;
        }
        match library
            .rewrite_source_locations(config.id(), |location| source.canonical_location(location))
        {
            Ok(changed) if changed > 0 => log::info!(
                "updated {changed} media locations for source {}",
                config.id()
            ),
            Ok(_) => {}
            Err(error) => log::warn!(
                "cannot update saved media locations for source {}: {error}",
                config.id()
            ),
        }
    }
}

impl Services {
    pub fn device_identifier(&self) -> &str {
        &self.device_identifier
    }

    /// Persists refreshed source credentials without rebuilding active sources.
    pub fn poll_source_updates(&mut self) {
        let mut changed = false;
        for current in &mut self.source_configs {
            if let Some(updated) = self
                .opener
                .source(current.id())
                .and_then(|s| s.updated_config())
                && updated.id() == current.id()
                && updated.kind() == current.kind()
            {
                *current = updated;
                changed = true;
            }
        }
        let retry = self
            .source_save_retry
            .is_some_and(|at| at.elapsed() >= std::time::Duration::from_secs(10));
        if changed || retry {
            match fp_sources::save_configs(&Settings::sources_path(), &self.source_configs) {
                Ok(()) => self.source_save_retry = None,
                Err(e) => {
                    log::warn!("saving refreshed source credentials: {e}");
                    self.source_save_retry = Some(std::time::Instant::now());
                }
            }
        }
    }

    pub fn new(settings: &Settings, library: Library) -> Services {
        let device_identifier = device_id(&fp_core::dirs::config_dir().join("device-id"));
        fp_sources::mediabrowser::set_device_id(device_identifier.clone());
        let opener = Arc::new(Opener::new());
        let source_configs = match fp_sources::load_configs(&Settings::sources_path()) {
            Ok(c) => c,
            Err(e) => {
                if Settings::sources_path().exists() {
                    log::warn!("sources: {e}");
                }
                Vec::new()
            }
        };
        for e in opener.set_sources(&source_configs) {
            log::warn!("source unavailable: {e}");
        }
        migrate_source_locations(&library, &opener, &source_configs);
        let mut s = Services {
            device_identifier,
            source_save_retry: None,
            library,
            opener: opener.clone(),
            images: Arc::new(
                ImageCache::new(
                    fp_core::dirs::cache_dir().join("images"),
                    settings.image_cache_mb,
                )
                .with_opener(opener.clone()),
            ),
            source_configs,
            worker: None,
            worker_events: None,
            worker_busy: Default::default(),
            haptics: None,
            haptic_devices: Vec::new(),
            haptics_generation: 0,
            remote: None,
            remote_error: None,
            watch: crate::watch::Reports::start(),
        };
        s.set_haptics(settings);
        s.start_worker(settings);
        s.apply_remote(&settings.remote);
        s
    }

    pub fn start_worker(&mut self, settings: &Settings) {
        let prober = Arc::new(crate::prober::FfmpegProber::new(
            self.opener.clone(),
            self.worker_busy.clone(),
        ));
        match fp_library::MetadataWorker::spawn(
            self.library.clone(),
            prober,
            worker_options(settings),
        ) {
            Ok((w, rx)) => {
                self.worker = Some(w);
                self.worker_events = Some(rx);
            }
            Err(e) => log::error!("metadata worker: {e}"),
        }
    }

    /// Settings › General › Thumbnails, from the worker's next pass on.
    pub fn set_worker_options(&self, settings: &Settings) {
        if let Some(w) = &self.worker {
            w.set_options(worker_options(settings));
        }
    }

    pub fn wake_worker(&self) {
        if let Some(w) = &self.worker {
            w.wake();
        }
    }

    /// Holds the metadata worker off while a video plays (or while
    /// quitting), cutting short the thumbnail it is making; it picks up
    /// where it left off once not busy.
    pub fn set_worker_busy(&self, busy: bool) {
        use std::sync::atomic::Ordering;
        if self.worker_busy.swap(busy, Ordering::AcqRel) && !busy {
            self.wake_worker();
        }
    }

    /// The configuration of source `id`, if the user added it.
    pub fn source_config(&self, id: &str) -> Option<&SourceConfig> {
        self.source_configs.iter().find(|c| c.id() == id)
    }

    /// Persists the source list (mode 0600) and rebuilds the sources. The
    /// list is used even when it can't be saved, until FramePlayerX quits.
    pub fn set_sources(&mut self, configs: Vec<SourceConfig>) -> SourcesChange {
        // Convert legacy references while the old endpoint/root is still known.
        migrate_source_locations(&self.library, &self.opener, &self.source_configs);
        let save_error = fp_sources::save_configs(&Settings::sources_path(), &configs)
            .inspect_err(|e| log::error!("saving sources: {e}"))
            .err()
            .map(|e| e.to_string());
        let errors = self.opener.set_sources(&configs);
        migrate_source_locations(&self.library, &self.opener, &configs);
        self.source_configs = configs;
        SourcesChange { errors, save_error }
    }

    /// Connects one haptic device (blocking: call from a job thread).
    pub fn connect_device(cfg: &HapticDeviceConfig) -> Result<Box<dyn fp_haptics::Device>, String> {
        use fp_haptics::{buttplug, handy, tcode};
        fn boxed<D: fp_haptics::Device + 'static>(
            r: Result<D, impl std::fmt::Display>,
        ) -> Result<Box<dyn fp_haptics::Device>, String> {
            r.map(|d| Box::new(d) as Box<dyn fp_haptics::Device>)
                .map_err(|e| e.to_string())
        }
        match cfg {
            HapticDeviceConfig::Tcode { endpoint } => {
                let endpoint = tcode::TcodeEndpoint::parse(endpoint).map_err(|e| e.to_string())?;
                boxed(tcode::TcodeDevice::connect(tcode::TcodeConfig {
                    endpoint,
                    ..Default::default()
                }))
            }
            HapticDeviceConfig::Buttplug { url } => boxed(buttplug::ButtplugDevice::connect(
                buttplug::ButtplugConfig {
                    url: url.clone(),
                    client_name: "FramePlayerX".into(),
                    ..Default::default()
                },
            )),
            HapticDeviceConfig::Handy { key } => {
                boxed(handy::HandyDevice::connect(handy::HandyConfig {
                    connection_key: key.clone(),
                    ..Default::default()
                }))
            }
        }
    }

    /// Starts the haptics engine when haptics are turned on and stops it,
    /// with every device, when they are turned off; applies their settings.
    pub fn set_haptics(&mut self, settings: &Settings) {
        if !settings.haptics_enabled {
            if self.haptics.is_some() {
                self.haptics_generation += 1;
            }
            self.remove_devices();
            self.haptics = None;
            return;
        }
        if self.haptics.is_none() {
            match HapticsEngine::with_clock(
                Arc::new(fp_haptics::engine::SystemClock),
                fp_haptics::engine::DEFAULT_TICK,
            ) {
                Ok(engine) => self.haptics = Some(engine),
                Err(e) => log::error!("haptics engine: {e}"),
            }
        }
        if let Some(h) = &self.haptics {
            h.set_settings(settings.haptics.clone());
        }
    }

    /// Changes each time haptics are turned off.
    pub fn haptics_generation(&self) -> u64 {
        self.haptics_generation
    }

    /// The haptics engine, while haptics are turned on.
    pub fn haptics(&self) -> Option<&HapticsEngine> {
        self.haptics.as_ref()
    }

    /// The connected devices' state, for the haptics settings.
    pub fn haptic_statuses(&self) -> Vec<fp_haptics::DeviceStatus> {
        self.haptics
            .as_ref()
            .map(HapticsEngine::devices)
            .unwrap_or_default()
    }

    /// Records how connecting `config` went, replacing an earlier attempt
    /// (and its device in the engine). A device that connects after
    /// haptics were turned off is let go.
    pub fn device_connected(
        &mut self,
        config: HapticDeviceConfig,
        result: Result<Box<dyn fp_haptics::Device>, String>,
    ) {
        self.remove_device(&config);
        let Some(engine) = &self.haptics else {
            return;
        };
        let (id, error) = match result {
            Ok(d) => (Some(engine.add_device(d)), None),
            Err(e) => (None, Some(e)),
        };
        self.haptic_devices
            .push(HapticDeviceSlot { config, id, error });
    }

    /// Disconnects `config`'s device and forgets how connecting it went.
    pub fn remove_device(&mut self, config: &HapticDeviceConfig) {
        self.haptic_devices.retain(|s| {
            let same = s.config == *config;
            if same && let (Some(id), Some(h)) = (s.id, &self.haptics) {
                h.remove_device(id);
            }
            !same
        });
    }

    /// Disconnects every device.
    pub fn remove_devices(&mut self) {
        for s in self.haptic_devices.drain(..) {
            if let (Some(id), Some(h)) = (s.id, &self.haptics) {
                h.remove_device(id);
            }
        }
    }

    /// How connecting `config` went; `None` while it is being tried.
    pub fn haptic_device(&self, config: &HapticDeviceConfig) -> Option<&HapticDeviceSlot> {
        self.haptic_devices.iter().find(|s| s.config == *config)
    }

    /// The web remote's pages follow the interface's language.
    pub fn set_remote_language(&self) {
        if let Some(hub) = &self.remote {
            hub.set_page_text(crate::i18n::current(), remote_page_text());
        }
    }

    /// Starts or stops the remote-control servers to match `config`.
    pub fn apply_remote(&mut self, config: &fp_remote::config::RemoteConfig) {
        if let Some(r) = self.remote.take() {
            r.stop();
        }
        self.remote_error = None;
        if !config.deovr_enabled && !config.web_enabled {
            return;
        }
        let mut cfg = config.clone();
        if cfg.token_path.is_none() {
            cfg.token_path = Some(fp_remote::config::RemoteConfig::default_token_path());
        }
        match RemoteHub::new(cfg.clone()) {
            Ok(hub) => {
                hub.set_page_text(crate::i18n::current(), remote_page_text());
                if cfg.deovr_enabled
                    && let Err(e) = hub.start_deovr_server()
                {
                    self.remote_error = Some(tr!("remote-error-deovr", error = e.to_string()));
                }
                if cfg.web_enabled
                    && let Err(e) = hub.start_web()
                {
                    self.remote_error = Some(tr!("remote-error-web", error = e.to_string()));
                }
                self.remote = Some(hub);
            }
            Err(e) => self.remote_error = Some(e.to_string()),
        }
    }

    pub fn remote_events(&self) -> Vec<RemoteEvent> {
        self.remote
            .as_ref()
            .map(|r| r.events().try_iter().collect())
            .unwrap_or_default()
    }
}

/// The web remote's text, by id; arguments stay as `{name}` for its script
/// to fill in.
fn remote_page_text() -> Vec<(String, String)> {
    let page = [
        ("remote-title", tr!("remote-title")),
        ("remote-connecting", tr!("remote-connecting")),
        ("remote-connected", tr!("remote-connected")),
        ("remote-reconnecting", tr!("remote-reconnecting")),
        ("remote-unreachable", tr!("remote-unreachable")),
        ("remote-now-playing", tr!("remote-now-playing")),
        ("remote-nothing-playing", tr!("remote-nothing-playing")),
        ("remote-nothing-hint", tr!("remote-nothing-hint")),
        ("remote-close", tr!("player-close")),
        ("remote-seek", tr!("remote-seek")),
        ("remote-previous", tr!("action-previous-video")),
        ("remote-back-10", tr!("remote-back-10")),
        ("remote-play", tr!("player-play")),
        ("remote-pause", tr!("player-pause")),
        ("remote-forward-10", tr!("remote-forward-10")),
        ("remote-next", tr!("action-next-video")),
        ("remote-mute", tr!("player-mute")),
        ("remote-unmute", tr!("player-unmute")),
        ("remote-volume", tr!("settings-volume")),
        ("remote-audio", tr!("tracks-audio")),
        ("remote-no-audio", tr!("tracks-no-audio")),
        ("remote-subtitles", tr!("settings-subtitles")),
        ("remote-subtitles-off", tr!("tracks-off")),
        ("remote-tracks", tr!("player-tracks")),
        ("remote-speed", tr!("player-speed")),
        ("remote-recenter", tr!("action-recenter")),
        ("remote-up-next", tr!("player-up-next")),
        ("remote-nothing-queued", tr!("remote-nothing-queued")),
        ("remote-loop", tr!("queue-loop")),
        ("remote-loop-video", tr!("remote-loop-video")),
        ("remote-autoplay", tr!("remote-autoplay")),
        ("remote-on", tr!("remote-on")),
        ("remote-off", tr!("tracks-off")),
        ("remote-type", tr!("remote-type")),
        ("remote-type-label", tr!("remote-type-label")),
        ("remote-send", tr!("remote-send")),
        ("remote-sent", tr!("remote-sent")),
        ("remote-unpaired-title", tr!("remote-unpaired-title")),
        ("remote-pair-heading", tr!("remote-pair-heading")),
        ("remote-pair-how", tr!("remote-pair-how")),
        ("remote-pair-code", tr!("remote-pair-code")),
        ("remote-pair", tr!("remote-pair")),
        ("remote-pair-wrong", tr!("remote-pair-wrong")),
        ("remote-pair-no-code", tr!("remote-pair-no-code")),
        ("remote-pair-stopped", tr!("remote-pair-stopped")),
        ("remote-pair-stays", tr!("remote-pair-stays")),
    ]
    .map(|(id, s)| (id.to_string(), s.to_string()));
    let with_args = [
        (
            "remote-command-failed",
            tr!("remote-command-failed", error = "{error}"),
        ),
        ("remote-not-sent", tr!("remote-not-sent", error = "{error}")),
        (
            "remote-queue-position",
            tr!("toast-queue-position", at = "{at}", of = "{of}"),
        ),
        (
            "remote-position",
            tr!(
                "remote-position",
                position = "{position}",
                duration = "{duration}"
            ),
        ),
    ]
    .map(|(id, s)| (id.to_string(), s));
    page.into_iter().chain(with_args).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSource {
        id: &'static str,
        opens: Arc<AtomicUsize>,
    }

    impl Source for CountingSource {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            self.id
        }
        fn kind(&self) -> fp_sources::SourceKind {
            fp_sources::SourceKind::Http
        }
        fn describe(&self) -> String {
            self.id.into()
        }
        fn list(&self, _: Option<&str>) -> fp_sources::Result<Vec<fp_core::Entry>> {
            Ok(Vec::new())
        }
        fn open(&self, _: &str) -> fp_sources::Result<Arc<dyn ByteSource>> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(Dribble(4)))
        }
    }

    #[test]
    fn persistent_resources_route_only_to_their_owner_for_media_and_assets() {
        let opener = Opener::new();
        let owner_calls = Arc::new(AtomicUsize::new(0));
        let other_calls = Arc::new(AtomicUsize::new(0));
        for (id, calls) in [
            ("owner", owner_calls.clone()),
            ("other", other_calls.clone()),
        ] {
            opener.insert_source(Arc::new(CountingSource { id, opens: calls }));
        }
        let location = fp_sources::location::encode("owner", "http", "videos/movie.mp4");
        assert!(opener.open(&location).is_ok());
        assert_eq!(opener.fetch(&location, 16).unwrap(), [7; 4]);
        assert_eq!(
            opener.fetch_in(Some("owner"), &location, 16).unwrap(),
            [7; 4]
        );
        assert_eq!(owner_calls.load(Ordering::SeqCst), 3);
        assert!(opener.open_in(Some("other"), &location).is_err());
        assert!(opener.fetch_in(Some("other"), &location, 16).is_err());
        assert!(
            opener
                .open(&fp_sources::location::encode(
                    "missing",
                    "http",
                    "movie.mp4"
                ))
                .is_err()
        );
        for invalid in [
            "source://elsewhere/owner/http/movie.mp4",
            "source:broken",
            "SOURCE://source/owner/http/movie.mp4?escape=1",
        ] {
            assert!(opener.open(invalid).is_err());
            assert!(opener.fetch(invalid, 16).is_err());
        }
        assert_eq!(owner_calls.load(Ordering::SeqCst), 3);
        assert_eq!(other_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn migrating_a_source_then_changing_its_address_keeps_history_and_playlists() {
        use std::io::{BufRead, BufReader, Write};
        let library = Library::open_in_memory().unwrap();
        let old_location = "http://old.example/old-root/folder/movie%20one.mp4";
        let id = library
            .upsert(&fp_library::MediaUpsert::new(old_location, "web"))
            .unwrap()
            .id;
        library.set_favorite(id, true).unwrap();
        library.set_resume_position(id, 37.0).unwrap();
        let playlist = library.create_playlist("Tonight").unwrap();
        library.add_to_playlist(playlist, &[id]).unwrap();
        let mut config = fp_sources::HttpConfig {
            id: "web".into(),
            name: "Server".into(),
            url: "http://old.example/old-root/".into(),
            credentials: None,
            insecure_tls: false,
            parallel_downloads: false,
        };
        let opener = Opener::new();
        let old = [SourceConfig::Http(config.clone())];
        assert!(opener.set_sources(&old).is_empty());
        migrate_source_locations(&library, &opener, &old);
        let stable = fp_sources::location::encode("web", "http", "folder/movie%20one.mp4");
        let saved = library.get_by_location(&stable).unwrap().unwrap();
        assert_eq!(saved.id, id);
        assert!(saved.favorite);
        assert_eq!(saved.resume_position, 37.0);
        assert_eq!(library.playlist_items(playlist).unwrap()[0].record.id, id);
        assert!(library.get_by_location(old_location).unwrap().is_none());

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        config.url = format!("http://{}/new-root/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header.is_empty() {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ntest")
                .unwrap();
            request
        });
        let current = [SourceConfig::Http(config)];
        assert!(opener.set_sources(&current).is_empty());
        migrate_source_locations(&library, &opener, &current);
        assert_eq!(opener.fetch_in(Some("web"), &stable, 16).unwrap(), b"test");
        assert!(
            server
                .join()
                .unwrap()
                .starts_with("GET /new-root/folder/movie%20one.mp4 ")
        );
        assert_eq!(library.get(id).unwrap().unwrap().location, stable);
    }

    #[test]
    fn device_id_is_made_once_and_kept() {
        let dir = std::env::temp_dir().join(format!("fpx-device-id-{}", std::process::id()));
        let path = dir.join("config").join("device-id");
        let first = device_id(&path);
        assert!(
            first.starts_with("frameplayerx-") && first.len() == 45,
            "{first}"
        );
        assert_eq!(device_id(&path), first);
        // Another installation gets its own.
        assert_ne!(device_id(&dir.join("other")), first);
        std::fs::write(&path, "not an id!\n").unwrap();
        assert_ne!(device_id(&path), "not an id!");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file of `len` bytes that hands out at most 1000 bytes a read, as
    /// network sources may.
    struct Dribble(usize);

    impl ByteSource for Dribble {
        fn size(&self) -> Option<u64> {
            Some(self.0 as u64)
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf
                .len()
                .min(1000)
                .min(self.0.saturating_sub(offset as usize));
            buf[..n].fill(7);
            Ok(n)
        }
        fn describe(&self) -> String {
            "dribble".into()
        }
    }

    #[test]
    fn small_files_are_read_whole_within_a_limit() {
        assert_eq!(
            read_whole(&Dribble(150_000), 1 << 20).unwrap().len(),
            150_000
        );
        assert_eq!(read_whole(&Dribble(0), 1 << 20).unwrap().len(), 0);
        assert!(read_whole(&Dribble(3 << 20), 1 << 20).is_err());
    }
}
