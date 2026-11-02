//! Background work and its results: opening videos, scans, updates, drives.

use super::*;

type PlexLink = Option<(
    fp_sources::plex::auth::PlexCredentials,
    Vec<fp_sources::plex::auth::PlexResource>,
)>;

pub(super) enum Job {
    Opened(u64, Box<Result<Opened, String>>),
    Listed {
        source: String,
        location: Option<String>,
        result: Result<Vec<fp_core::source::Entry>, String>,
    },
    /// How far scanning library `source` got.
    ScanStatus {
        source: String,
        status: String,
    },
    /// Library `source` scanned; `told`: someone asked, so say how it went.
    ScanDone {
        source: String,
        result: Result<fp_library::ScanReport, String>,
        told: bool,
    },
    Catalog {
        source: String,
        seq: u64,
        result: Box<Result<ui::catalog::Loaded, String>>,
    },
    /// A scene feed's listing; `cached`: the one saved last time, shown
    /// while the server answers.
    FeedLibrary {
        source: String,
        result: Result<fp_sources::FeedLibrary, String>,
        cached: bool,
    },
    /// A HereSphere scan, after its listing (`None`: the server has none).
    FeedScan {
        source: String,
        scenes: Option<Vec<fp_sources::FeedScene>>,
    },
    /// A scene's details: for its page (`full`, with media) or a card.
    FeedScene {
        source: String,
        url: String,
        full: bool,
        result: Box<Result<fp_sources::SceneInfo, String>>,
    },
    Toast(String),
    /// Nothing to show (a server took a change).
    Done,
    Dlna(Result<Vec<fp_sources::DlnaDevice>, String>),
    /// A device connected, with the haptics generation it was started in.
    Device(
        crate::settings::HapticDeviceConfig,
        u64,
        Result<Box<dyn fp_haptics::Device>, String>,
    ),
    /// Download and install progress.
    Update(UpdateStatus),
    /// The result of update check number `.0`.
    UpdateChecked(u64, UpdateStatus, Option<Box<fp_updater::Update>>),
    Mounted(String, Result<std::path::PathBuf, String>),
    /// The answer to pairing code request number `.0`.
    QuickConnectInitiated(u64, Result<fp_sources::QuickConnectState, String>),
    /// The answer to a poll with secret `.0`.
    QuickConnectPolled(String, Result<Option<(String, String)>, String>),
    PlexInitiated(u64, Result<fp_sources::plex::auth::PlexPin, String>),
    PlexPolled(u64, Box<Result<PlexLink, String>>),
    PlexConnected(u64, Box<Result<fp_sources::PlexConfig, String>>),
    /// The last run crashed: the report the user may send.
    CrashReport(Box<crate::crash::Report>),
    CrashSent(Result<(), String>),
    /// Steam was asked to open a web page.
    WebPageOpened(Result<(), String>),
    /// Subtitle file `index` of the video opened as `seq`, read.
    Subtitles {
        seq: u64,
        index: usize,
        result: Result<Vec<fp_media::Cue>, String>,
    },
}

impl App {
    /// Scans the libraries `which` picks whose folder can be reached now,
    /// each on its own (so a card that is out never marks videos missing),
    /// unless it is being scanned already. `force` makes thumbnails again;
    /// `told` says how it went.
    pub(super) fn scan_libraries(
        &mut self,
        which: impl Fn(&fp_sources::LibraryConfig) -> bool,
        force: bool,
        told: bool,
    ) {
        let targets: Vec<_> = self
            .services
            .source_configs
            .iter()
            .filter_map(|c| match c {
                fp_sources::SourceConfig::Library(l) => Some(l),
                _ => None,
            })
            .filter(|l| which(l) && !self.ui.scans.contains_key(&l.id))
            .filter_map(|l| crate::libraries::scan_target(l).map(|t| (l.id.clone(), t)))
            .collect();
        for (source, (root, location_root)) in targets {
            let lib = self.services.library.clone();
            let tx = self.jobs.sender();
            self.ui
                .scans
                .insert(source.clone(), tr!("scan-starting").into());
            self.jobs.spawn("scan", move || {
                let opts = fp_library::ScanOptions {
                    force,
                    location_root,
                    ..Default::default()
                };
                let mut last = Instant::now();
                let result = lib
                    .scan_folder(&root, &source, &opts, |p| {
                        if last.elapsed() > Duration::from_millis(300) {
                            last = Instant::now();
                            let _ = tx.send(Job::ScanStatus {
                                source: source.clone(),
                                status: tr!("scan-progress", count = p.videos_found),
                            });
                        }
                    })
                    .map_err(|e| e.to_string());
                Job::ScanDone {
                    source,
                    result,
                    told,
                }
            });
        }
    }

    fn scanned(
        &mut self,
        source: &str,
        result: Result<fp_library::ScanReport, String>,
        told: bool,
    ) {
        self.ui.scans.remove(source);
        if self.source_config(source).is_none() {
            // Removed while it was scanned.
            let _ = self.services.library.forget_unwatched(source);
            return;
        }
        let name = self.source_name(source);
        match result {
            Ok(r) => {
                log::info!(
                    "scanned {name}: {} new, {} missing, {} unreadable",
                    r.added,
                    r.missing,
                    r.errors.len()
                );
                let msg = tr!(
                    "scan-done",
                    name = name.as_str(),
                    added = r.added,
                    missing = r.missing
                );
                if told {
                    self.ui.toast(msg);
                }
            }
            Err(e) => {
                log::warn!("scanning {name}: {e}");
                if told {
                    self.ui
                        .toast(tr!("scan-failed", name = name.as_str(), error = e.as_str()));
                }
            }
        }
        self.ui.invalidate();
        self.services.wake_worker();
    }

    /// Connects every configured device, while haptics are turned on.
    pub(super) fn connect_devices(&mut self) {
        for cfg in self.settings.haptic_devices.clone() {
            self.connect_device(cfg);
        }
    }

    pub(super) fn connect_device(&mut self, cfg: crate::settings::HapticDeviceConfig) {
        if !self.settings.haptics_enabled {
            return;
        }
        let generation = self.services.haptics_generation();
        self.jobs.spawn("haptics", move || {
            let r = Services::connect_device(&cfg);
            Job::Device(cfg, generation, r)
        });
    }

    /// Offers a crash report when the last run crashed. Reading its log
    /// and hiding every name in it takes a moment with a big library.
    pub(super) fn check_last_run(&mut self) {
        let Some(_) = crate::crash::REPORT_URL else {
            return;
        };
        let old = crate::logger::log_path().with_extension("log.old");
        let Ok(log) = std::fs::read_to_string(&old) else {
            return;
        };
        let private = crate::crash::Private::gather(
            &self.services.library,
            &self.services.source_configs,
            &self.settings,
        );
        self.jobs.spawn(
            "crash-report",
            move || match crate::crash::Report::from_log(&log, &private, &crate::crash::system()) {
                Some(r) => Job::CrashReport(Box::new(r)),
                None => Job::Done,
            },
        );
    }

    /// The crash page for `log`, at once: the preview's way to show it.
    pub(crate) fn offer_crash_report(&mut self, log: &str) {
        let private = crate::crash::Private::gather(
            &self.services.library,
            &self.services.source_configs,
            &self.settings,
        );
        self.ui.crash = crate::crash::Report::from_log(log, &private, &crate::crash::system())
            .map(ui::crash::CrashPrompt::new);
    }

    /// Opens `address` in Steam's browser, which shows over the app: the
    /// headset has no other browser to hand it to.
    pub(super) fn open_web_page(&mut self, address: &str) {
        if !address.starts_with("https://") || address.chars().any(char::is_control) {
            return;
        }
        let url = format!("steam://openurl/{address}");
        self.jobs.spawn("open-web-page", move || {
            let r = std::process::Command::new("steam")
                .arg(&url)
                .status()
                .map_err(|e| e.to_string())
                .and_then(|s| {
                    s.success()
                        .then_some(())
                        .ok_or_else(|| format!("steam: {s}"))
                });
            Job::WebPageOpened(r)
        });
    }

    /// Sends the crash report the user agreed to send.
    pub(super) fn send_crash_report(&mut self) {
        let (Some(url), Some(c)) = (crate::crash::REPORT_URL, &mut self.ui.crash) else {
            return;
        };
        c.sending = true;
        c.error = None;
        let body = c.report.body();
        self.jobs.spawn("crash-send", move || {
            let http = fp_sources::http::HttpClient::new(Default::default(), None);
            let r = http
                .request(
                    "POST",
                    url,
                    &[("Content-Type", "application/json")],
                    Some(&body),
                    4096,
                )
                .map_err(|e| ui::errors::source(&e))
                .and_then(|reply| match reply.status {
                    200..=299 => Ok(()),
                    status => Err(tr!(
                        "error-http-status",
                        status = status.to_string(),
                        url = url
                    )),
                });
            Job::CrashSent(r)
        });
    }

    pub(super) fn check_updates(&mut self) {
        self.ui.update = UpdateStatus::Checking;
        self.update = None;
        self.update_check += 1;
        let seq = self.update_check;
        let channel = self.settings.update_channel;
        self.jobs.spawn("update-check", move || {
            match fp_updater::check_channel(env!("CARGO_PKG_VERSION"), channel) {
                Ok(Some(u)) => Job::UpdateChecked(
                    seq,
                    UpdateStatus::Available {
                        version: u.version.to_string(),
                        notes: u.notes.clone(),
                    },
                    Some(Box::new(u)),
                ),
                Ok(None) => Job::UpdateChecked(seq, UpdateStatus::UpToDate, None),
                Err(e) => Job::UpdateChecked(seq, UpdateStatus::Failed(e.to_string()), None),
            }
        });
    }

    pub(super) fn install_update(&mut self) {
        let Some(u) = self.update.clone() else { return };
        let tx = self.jobs.sender();
        self.ui.update = UpdateStatus::Downloading {
            done: 0,
            total: u.artifact.size,
        };
        self.jobs.spawn("update-install", move || {
            let dir = fp_core::dirs::data_dir().join("updates");
            let cancel = AtomicBool::new(false);
            let mut last = Instant::now();
            let r = std::fs::create_dir_all(&dir)
                .map_err(|e| e.to_string())
                .and_then(|_| {
                    fp_updater::download(
                        &u,
                        &dir,
                        |done, total| {
                            if last.elapsed() > Duration::from_millis(250) {
                                last = Instant::now();
                                let _ =
                                    tx.send(Job::Update(UpdateStatus::Downloading { done, total }));
                            }
                        },
                        &cancel,
                    )
                    .map_err(|e| e.to_string())
                })
                .and_then(|zip| {
                    let install = fp_updater::current_install_dir().map_err(|e| e.to_string())?;
                    fp_updater::install::install_expecting(&zip, &install, &u.version)
                        .map_err(|e| e.to_string())
                });
            match r {
                Ok(v) => Job::Update(UpdateStatus::Installed {
                    version: v.to_string(),
                }),
                Err(e) => Job::Update(UpdateStatus::Failed(e)),
            }
        });
    }

    /// Takes in finished background work and the metadata worker's progress.
    pub(super) fn poll_jobs(&mut self) {
        self.services.poll_source_updates();
        for job in self.jobs.poll() {
            self.on_job(job);
        }
        self.poll_worker();
    }

    fn on_job(&mut self, job: Job) {
        match job {
            Job::Opened(seq, r) => self.opened(seq, *r),
            Job::Listed {
                source,
                location,
                result,
            } => self.listed(&source, location.as_deref(), result),
            Job::ScanStatus { source, status } => {
                if let Some(s) = self.ui.scans.get_mut(&source) {
                    *s = status;
                }
            }
            Job::ScanDone {
                source,
                result,
                told,
            } => self.scanned(&source, result, told),
            Job::Catalog {
                source,
                seq,
                result,
            } => {
                if let Some(j) = self.ui.catalog_page(&source) {
                    j.loaded(seq, *result);
                }
            }
            Job::FeedLibrary {
                source,
                result,
                cached,
            } => {
                if let Some(f) = self.ui.feed_page(&source) {
                    f.loaded(result, cached);
                }
            }
            Job::FeedScan { source, scenes } => {
                if let Some(f) = self.ui.feed_page(&source) {
                    f.scanned(scenes);
                }
            }
            Job::FeedScene {
                source,
                url,
                full,
                result,
            } => {
                if !full {
                    self.feed_busy = self.feed_busy.saturating_sub(1);
                }
                if let Some(f) = self.ui.feed_page(&source) {
                    f.scene_loaded(&url, *result, full);
                }
                self.pump_feed();
                // A batch of details done: kept with the listing, so a search
                // finds their tags next time too.
                if self.feed_busy == 0
                    && self.feed_queue.is_empty()
                    && let Some(f) = self.ui.feed_page(&source).filter(|f| f.unsaved > 0)
                {
                    f.unsaved = 0;
                    let lib = f.library();
                    self.jobs.spawn("feed-save", move || {
                        let path = fp_sources::FeedLibrary::cache_path(&source);
                        if let Err(e) = lib.save(&path) {
                            log::warn!("saving the listing of {source}: {e}");
                        }
                        Job::Done
                    });
                }
            }
            Job::Toast(t) => self.ui.toast(t),
            Job::Done | Job::WebPageOpened(Ok(())) => {}
            Job::Subtitles { seq, index, result } => {
                // Only for the same video, and the file still picked.
                let Some(p) = self.playback.as_mut().filter(|_| seq == self.open_seq) else {
                    return;
                };
                if p.active_subtitle_file != Some(index) {
                    return;
                }
                match result {
                    Ok(cues) => p.player.set_external_subtitles(cues),
                    Err(e) => {
                        p.active_subtitle_file = None;
                        self.ui.toast(tr!("toast-subtitles", title = e.to_string()));
                    }
                }
                self.ui.invalidate();
            }
            Job::Dlna(r) => {
                self.ui.dlna_searching = false;
                match r {
                    Ok(d) => self.ui.dlna_found = d,
                    Err(e) => self
                        .ui
                        .toast(tr!("dlna-search-failed", error = e.to_string())),
                }
            }
            // A device connected before haptics were turned off and on
            // again, or that was removed or changed meanwhile, is let go.
            Job::Device(cfg, generation, _)
                if generation != self.services.haptics_generation()
                    || !self.settings.haptic_devices.contains(&cfg) =>
            {
                log::info!("haptics {}: dropped a late connection", cfg.label());
            }
            Job::Device(cfg, _, r) => {
                if let Err(e) = &r {
                    log::warn!("haptics {}: {e}", cfg.label());
                    self.ui.toast(format!("{}: {e}", cfg.label()));
                }
                self.services.device_connected(cfg, r);
            }
            Job::Mounted(device, r) => match r {
                // check_mounts announces the new drive.
                Ok(p) => log::info!("mounted /dev/{device} at {}", p.display()),
                Err(e) => {
                    log::warn!("mounting /dev/{device}: {e}");
                    self.ui
                        .toast(tr!("drive-mount-failed", error = e.to_string()));
                }
            },
            Job::Update(status) => self.ui.update = status,
            Job::UpdateChecked(seq, status, update) => {
                if seq == self.update_check {
                    if let UpdateStatus::Available { version, .. } = &status
                        && self.start_check == Some(seq)
                    {
                        self.update_notice = Some(tr!("update-notice", version = version.as_str()));
                    }
                    self.update = update.map(|u| *u);
                    self.ui.update = status;
                }
            }
            Job::QuickConnectInitiated(seq, r) => self.quick_connect_code(seq, r),
            Job::QuickConnectPolled(secret, r) => self.quick_connect_answer(&secret, r),
            Job::PlexInitiated(generation, result) => self.plex_initiated(generation, result),
            Job::PlexPolled(generation, result) => self.plex_polled(generation, *result),
            Job::PlexConnected(generation, result) => self.plex_connected(generation, *result),
            Job::CrashReport(report) => {
                self.ui.crash = Some(ui::crash::CrashPrompt::new(*report));
                self.show_browser = true;
                self.ui_visible = true;
            }
            Job::CrashSent(Ok(())) => {
                self.ui.crash = None;
                self.ui.toast(tr!("crash-sent"));
            }
            Job::CrashSent(Err(e)) => {
                if let Some(c) = &mut self.ui.crash {
                    c.sending = false;
                    c.error = Some(e);
                }
            }
            Job::WebPageOpened(Err(e)) => {
                log::warn!("opening a web page: {e}");
                self.ui.toast(tr!("error-open-page", error = e));
            }
        }
    }

    /// A video finished opening; one asked for before the latest is closed.
    fn opened(&mut self, seq: u64, r: Result<Opened, String>) {
        if seq != self.open_seq {
            if let Ok(o) = r {
                o.playback.close(&self.services.library);
            }
            return;
        }
        self.ui.opening = None;
        match r {
            // Its source was locked while it opened.
            Ok(o) if self.locked_source_of(&o.playback.request).is_some() => {
                o.playback.close(&self.services.library);
            }
            Ok(o) => {
                if let Some(old) = self.playback.take() {
                    old.close(&self.services.library);
                }
                if let Some(h) = self.services.haptics() {
                    h.load(o.scripts);
                }
                self.playback = Some(o.playback);
                if let Some(p) = &mut self.playback {
                    p.place_flat(self.offsets.main[0]);
                }
                self.ui.open_error = None;
                self.ui.adjust_open = false;
                self.ui.tracks_open = false;
                self.show_browser = false;
                self.ui_visible = true;
                self.last_activity = Instant::now();
                self.sub_key.clear();
                self.ui.invalidate();
            }
            Err(e) => {
                log::warn!("open failed: {e}");
                self.ui.open_error = Some(e);
            }
        }
    }

    /// A folder listing, for the folder on screen if it is still that one.
    fn listed(
        &mut self,
        source: &str,
        location: Option<&str>,
        result: Result<Vec<fp_core::source::Entry>, String>,
    ) {
        let Some(b) = self
            .ui
            .folder_mut()
            .filter(|b| b.source == source && b.location.as_deref() == location)
        else {
            return;
        };
        b.loading = false;
        match result {
            Ok(mut e) => {
                fp_sources::sort_entries_by(&mut e, self.settings.browse_sort);
                b.entries = e;
                b.error = None;
            }
            Err(e) => b.error = Some(e),
        }
    }

    // A Quick Connect answer for a form that has moved on (cancelled,
    // closed, asked again) is dropped.

    fn quick_connect_code(&mut self, seq: u64, r: Result<fp_sources::QuickConnectState, String>) {
        let Some(f) = self.ui.source_form.as_mut() else {
            return;
        };
        if f.quick_connect != QuickConnect::Requesting || seq != self.quick_connect_request {
            return;
        }
        f.quick_connect = match r {
            Ok(qc) => QuickConnect::Waiting {
                code: qc.code,
                secret: qc.secret,
                polled: Instant::now(),
            },
            Err(e) => QuickConnect::Failed(e),
        };
        self.panels[MAIN].request_repaint();
    }

    fn quick_connect_answer(&mut self, secret: &str, r: Result<Option<(String, String)>, String>) {
        let (token, user_id) = match r {
            Ok(Some(t)) => t,
            Ok(None) => return,
            // Asked again shortly; a code that expired stays up until
            // cancelled.
            Err(e) => {
                log::warn!("Quick Connect poll error: {e}");
                return;
            }
        };
        let Some(f) = self.ui.source_form.as_mut() else {
            return;
        };
        if !matches!(&f.quick_connect, QuickConnect::Waiting { secret: s, .. } if s == secret) {
            return;
        }
        f.quick_connect = QuickConnect::Authorized {
            token,
            user_id: Some(user_id),
        };
        self.ui.toast(tr!("quick-connect-authorized-toast"));
        self.panels[MAIN].request_repaint();
    }

    /// The metadata worker's progress, shown while it makes thumbnails.
    fn poll_worker(&mut self) {
        let Some(rx) = &self.services.worker_events else {
            return;
        };
        for ev in rx.try_iter() {
            use fp_library::WorkerEvent as E;
            let (done, total) = match ev {
                E::Processed {
                    id,
                    done,
                    total,
                    thumbnail,
                    ..
                } => {
                    // Made again under the same name (another format, a
                    // changed file): the old picture must not stay.
                    if thumbnail
                        && let Ok(Some(r)) = self.services.library.get(id)
                        && let Some(p) = r.thumbnail
                    {
                        self.thumbs.forget(&p.display().to_string());
                    }
                    (done, total)
                }
                E::Failed { done, total, .. } => (done, total),
                E::ResumeThumbnail { .. } | E::ResumeThumbnailsDeleted(_) => {
                    self.ui.invalidate();
                    continue;
                }
                E::Idle { .. } => {
                    if self.ui.worker_status.take().is_some() {
                        self.ui.invalidate();
                        self.thumbs.retry_failed();
                    }
                    continue;
                }
                E::Error(e) => {
                    log::warn!("metadata worker: {e}");
                    continue;
                }
                E::Started { .. } | E::Stopped => continue,
            };
            let doing = if self.settings.make_thumbnails {
                tr!("worker-thumbnails")
            } else {
                tr!("worker-reading")
            };
            self.ui.worker_status = Some(format!("{doing} {done}/{total}"));
            if done % 12 == 0 || done == total {
                self.ui.invalidate();
                self.thumbs.retry_failed();
            }
        }
    }

    /// Mounts cards and drives SteamOS leaves unmounted (exFAT/FAT/NTFS
    /// microSD cards, any USB drive), once per insertion.
    pub(super) fn mount_drives(&mut self) {
        let drives = crate::drives::unmounted_drives();
        // Forget drives that were removed (or mounted), so they are tried
        // again when inserted again.
        self.mount_tried
            .retain(|d| drives.iter().any(|u| &u.device == d));
        for d in drives {
            if self.mount_tried.contains(&d.device) {
                continue;
            }
            log::info!("mounting /dev/{} ({})", d.device, d.fs_type);
            self.mount_tried.push(d.device.clone());
            self.jobs.spawn("mount", move || {
                let r = crate::drives::mount_drive(&d);
                Job::Mounted(d.device, r)
            });
        }
    }

    /// Notices microSD cards and USB drives being inserted or removed.
    pub(super) fn check_mounts(&mut self) {
        if self.mounts_checked.elapsed() < Duration::from_secs(3) {
            return;
        }
        self.mounts_checked = Instant::now();
        self.mount_drives();
        let now = crate::drives::refresh_mounts();
        if now == self.mounts {
            return;
        }
        let added: Vec<_> = now
            .iter()
            .filter(|m| !self.mounts.contains(m))
            .cloned()
            .collect();
        self.mounts = now;
        if let Some(m) = added.first() {
            let name = m
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            log::info!("drive mounted: {}", m.display());
            self.ui.toast(tr!("drive-found", name = name.as_str()));
            self.scan_libraries(|l| l.card.is_some(), false, false);
        } else {
            log::info!("a drive was removed");
        }
        self.ui.invalidate();
        if let Some(b) = self
            .ui
            .folder_mut()
            .filter(|b| b.source == crate::services::REMOVABLE_SOURCE)
        {
            let loc = b.location.clone();
            self.browse(crate::services::REMOVABLE_SOURCE.into(), loc);
        }
    }

    /// Opens the video playing again from where it is, as `quality`.
    pub(super) fn play_quality(&mut self, quality: playback::Quality) {
        let Some(p) = &self.playback else {
            return;
        };
        let transcode = match &quality {
            playback::Quality::Transcode(id) => Some(id.clone()),
            _ => None,
        };
        if p.request.quality == quality && p.request.transcode == transcode {
            return;
        }
        let req = OpenRequest {
            start_at: Some(p.player.position()),
            paused: p.player.is_paused(),
            quality,
            transcode,
            ..p.request.clone()
        };
        self.open_request(req);
    }

    pub(super) fn play_source_track(
        &mut self,
        audio: Option<String>,
        subtitle: Option<Option<String>>,
    ) {
        let Some(p) = &mut self.playback else { return };
        if p.transcoding.is_none() || p.source_tracks.is_empty() {
            return;
        }
        // Kept for when this video next opens, as a pick in the player is.
        if let Some(id) = &audio {
            p.picked.audio = p
                .source_tracks
                .iter()
                .find(|t| !t.subtitle && t.id == *id)
                .and_then(|t| t.index);
        }
        if let Some(subtitle) = &subtitle {
            p.picked.subtitles =
                playback::picked_source_subtitles(subtitle.as_deref(), &p.source_tracks);
        }
        p.save_tracks(&self.services.library);
        let mut req = p.request.clone();
        req.start_at = Some(p.player.position());
        req.paused = p.player.is_paused();
        if let Some(audio) = audio {
            req.source_audio = Some(audio);
        }
        if let Some(subtitle) = subtitle {
            req.source_subtitle = Some(subtitle.unwrap_or_else(|| "0".into()));
        }
        self.open_request(req);
    }

    pub(super) fn open_request(&mut self, mut req: OpenRequest) {
        // A file on a card goes by the card's ID, as the library has it.
        req.location = fp_core::volume::stable(&req.location).into_owned();
        if let Some(e) = req.entry.as_mut() {
            e.location = fp_core::volume::stable(&e.location).into_owned();
        }
        if let Some(source) = self.locked_source_of(&req) {
            let name = self.source_name(&source);
            self.ui.toast(tr!("source-is-locked", name = name));
            self.ask_pin(&source);
            return;
        }
        // Close the current video first: it holds a hardware decoder session,
        // and with Steam holding others the new video could not get one, fell
        // back to software and stuttered ("Hardware decoder busy"). It
        // closes on a thread of its own; the open job waits for it.
        if let Some(p) = self.playback.take() {
            p.close(&self.services.library);
            if let Some(h) = self.services.haptics() {
                h.clear();
            }
            self.ui.invalidate();
        }
        self.open_seq += 1;
        self.open_started = Instant::now();
        let lib = self.services.library.clone();
        let seq = self.open_seq;
        let opener = self.services.opener.clone();
        let settings = self.settings.clone();
        let reports = self.services.watch.clone();
        self.ui.opening = Some(req.shown_title());
        self.ui.open_error = None;
        self.services.set_worker_busy(true);
        self.jobs.spawn("open", move || {
            let opened = playback::open(req, &opener, &lib, &settings, &reports);
            Job::Opened(seq, Box::new(opened))
        });
    }

    /// Gives up on a video that has been opening for [`OPEN_TIMEOUT`]: says
    /// so, and closes it if it opens after all (its answer is stale then).
    pub(super) fn give_up_slow_open(&mut self) {
        if self.ui.opening.is_none() || self.open_started.elapsed() < OPEN_TIMEOUT {
            return;
        }
        let title = self.ui.opening.take().unwrap_or_default();
        log::warn!("gave up opening {title} after {} s", OPEN_TIMEOUT.as_secs());
        self.open_seq += 1;
        let seconds = OPEN_TIMEOUT.as_secs();
        self.ui.open_error = Some(tr!(
            "open-timeout",
            title = title.as_str(),
            seconds = seconds
        ));
        self.ui.invalidate();
    }
}
