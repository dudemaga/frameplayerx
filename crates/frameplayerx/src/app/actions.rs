//! Carrying out what the interface asks for.

use super::*;

impl App {
    pub(crate) fn apply(&mut self, a: Action) {
        match a {
            Action::Open(req) => {
                self.queue.clear();
                self.open_request(req);
            }
            Action::OpenList(list, pos) => self.open_list(list, pos, false),
            Action::PlayAll(list, pos) => self.open_list(list, pos, true),
            Action::Queue { req, next } => self.queue_video(req, next),
            Action::QueueJump(i) => {
                if let Some(req) = self.queue.jump(i).cloned() {
                    self.show_browser = false;
                    self.open_request(req);
                }
            }
            Action::StepQueue(dir) => self.step_queue(dir),
            Action::QueueRemove(i) => self.queue.remove(i),
            Action::QueueMove(from, to) => self.queue.move_item(from, to),
            Action::QueueShuffle => self.queue.shuffle(crate::shuffle::new_seed()),
            Action::QueueClear => self.queue.clear_rest(),
            Action::ShowUpNext => {
                self.ui.screen = Screen::UpNext;
                self.ui.details = None;
                self.ui.picking = None;
                self.show_browser = true;
                self.ui_visible = true;
            }
            Action::AddToPlaylist {
                playlist,
                name,
                reqs,
            } => self.add_to_playlist(playlist, &name, &reqs),
            Action::RenamePlaylist(id, name) => {
                let r = self.services.library.rename_playlist(id, &name);
                self.library_changed("the name", |e| tr!("error-save-name", error = e), r);
            }
            Action::DeletePlaylist(id) => {
                let r = self.services.library.delete_playlist(id);
                if self.ui.playlist == Some(id) {
                    self.ui.playlist = None;
                }
                self.library_changed("the change", |e| tr!("error-save-change", error = e), r);
            }
            Action::PlaylistRemove(id, at) => {
                let r = self.services.library.remove_from_playlist(id, at);
                self.library_changed("the change", |e| tr!("error-save-change", error = e), r);
            }
            Action::PlaylistMove(id, from, to) => {
                let r = self.services.library.move_playlist_item(id, from, to);
                self.library_changed("the change", |e| tr!("error-save-change", error = e), r);
            }
            Action::TogglePause => self.toggle_pause(),
            Action::Seek(t) => {
                if let Some(p) = self.player() {
                    p.seek(t, true);
                }
            }
            Action::SeekRelative(d) => {
                if let Some(p) = self.player() {
                    p.seek_relative(d);
                }
            }
            Action::SetSpeed(s) => {
                if let Some(p) = self.player() {
                    p.set_speed(s.clamp(0.25, 3.0));
                }
            }
            Action::SetVolume(v) => {
                if let Some(p) = self.player() {
                    p.set_volume(v);
                }
                self.settings.volume = v;
            }
            Action::ClosePlayback => self.close_playback(),
            Action::SetFormat(f) => {
                if let Some(p) = &mut self.playback {
                    let was_flat = p.format.projection == Projection::Flat;
                    p.set_format(&self.services.library, f);
                    if !was_flat {
                        p.place_flat(self.offsets.main[0]);
                    }
                }
                self.ui.invalidate();
            }
            Action::SaveKey => {
                if let Some(p) = &mut self.playback {
                    p.key_changed();
                }
            }
            Action::SaveView => {
                if let Some(p) = &mut self.playback {
                    match p.save_settings(&self.services.library) {
                        Ok(true) => self.ui.toast(tr!("toast-adjustments-saved")),
                        Ok(false) => {}
                        Err(e) => self
                            .ui
                            .toast(tr!("error-save-adjustments", error = e.to_string())),
                    }
                }
            }
            Action::ResetView => self.edit_view(|p, s| {
                p.settings = s.new_video_view();
                p.keyframes = Default::default();
            }),
            Action::AddKeyframe => self.edit_view(|p, _| {
                p.keyframes.insert(p.player.position(), p.settings);
            }),
            Action::ClearKeyframes => self.edit_view(|p, _| p.keyframes = Default::default()),
            Action::AddBookmark => self.add_bookmark(),
            Action::SelectAudio(i) => {
                if let Some(p) = &mut self.playback {
                    p.player.select_audio(i);
                    p.picked.audio = Some(i);
                    p.request.source_audio = p
                        .source_tracks
                        .iter()
                        .find(|t| !t.subtitle && t.index == Some(i))
                        .map(|t| t.id.clone());
                    p.save_tracks(&self.services.library);
                }
            }
            Action::SelectSourceAudio(id) => self.play_source_track(Some(id), None),
            Action::SelectSourceSubtitles(id) => self.play_source_track(None, Some(id)),
            Action::SelectSubtitles(pick) => {
                if let crate::subtitles::Pick::File(i) = pick {
                    self.select_subtitle_file(Some(i));
                } else {
                    self.select_subtitle_file(None);
                    if let Some(p) = self.player() {
                        p.select_subtitle(pick.stream());
                    }
                }
                if let Some(p) = &mut self.playback {
                    p.picked.subtitles = crate::subtitles::saved(pick, &p.subtitle_files);
                    if !p.source_tracks.is_empty() {
                        let track = p.source_tracks.iter().find(|t| {
                            t.subtitle
                                && match pick {
                                    crate::subtitles::Pick::Stream(i) => t.index == Some(i),
                                    crate::subtitles::Pick::File(i) => p
                                        .subtitle_files
                                        .get(i)
                                        .is_some_and(|s| Some(&s.location) == t.location.as_ref()),
                                    crate::subtitles::Pick::Off => false,
                                }
                        });
                        p.request.source_subtitle = track.map(|t| t.id.clone()).or_else(|| {
                            matches!(pick, crate::subtitles::Pick::Off).then(|| "0".into())
                        });
                    }
                    p.save_tracks(&self.services.library);
                }
            }
            Action::SelectQuality(q) => self.play_quality(q),
            Action::ResetPlacement => self.reset_placement(),
            Action::SetFavorite(id, f) => {
                let r = self.services.library.set_favorite(id, f);
                self.library_changed("the favorite", |e| tr!("error-save-favorite", error = e), r);
            }
            Action::SetRating(id, r) => {
                let r = self.services.library.set_rating(id, r);
                self.library_changed("the rating", |e| tr!("error-save-rating", error = e), r);
            }
            Action::SetUserFormat(id, f) => {
                let r = self.services.library.set_user_format(id, f);
                self.library_changed("the format", |e| tr!("error-save-format", error = e), r);
                // Its pictures show the part of the frame the format picks.
                self.services.wake_worker();
            }
            Action::RemoveFromHistory(id) => {
                let r = self.services.library.remove_from_history(id);
                self.library_changed("the change", |e| tr!("error-save-change", error = e), r);
                // Its frame where it was left off goes.
                self.services.wake_worker();
            }
            Action::Rescan { source, force } => self.rescan(&source, force),
            Action::RetryFailed(source) => {
                let n = self
                    .services
                    .library
                    .retry_failed_probes(Some(&source))
                    .unwrap_or(0);
                self.services.wake_worker();
                self.ui.toast(tr!("toast-retrying", count = n));
            }
            Action::ClearHistory => self.clear_history(),
            Action::ClearImageCache => {
                // Pictures on screen load again from their servers.
                self.thumbs.forget_remote();
                let images = self.services.images.clone();
                // A full cache is thousands of files: not on this thread.
                self.jobs.spawn("clear-images", move || {
                    let mb = images.clear().div_ceil(1 << 20);
                    Job::Toast(tr!("toast-image-cache-cleared", mb = mb))
                });
            }
            Action::AddSource(cfg) => self.add_source(cfg),
            Action::UpdateSource(cfg) => self.update_source(cfg),
            Action::RemoveSource(id) => self.remove_source(&id),
            Action::UnlockSource { source, pin } => self.unlock_source(source, &pin),
            Action::LockSource(id) => self.lock_source(&id),
            Action::SetSourcePin { source, pin } => self.set_source_pin(&source, pin),
            Action::OpenSource { source, back_to } => self.open_source(source, back_to),
            Action::Browse { source, location } => self.browse(source, location),
            Action::Catalog { source, seq, load } => {
                self.catalog_job(source, seq, load, Duration::ZERO);
            }
            Action::CatalogMark {
                source,
                id,
                mark,
                on,
            } => self.mark_catalog(&source, id, mark, on),
            Action::CatalogVersion {
                source,
                location,
                version,
            } => {
                if let Some(mb) = self
                    .services
                    .opener
                    .source(&source)
                    .as_deref()
                    .and_then(|s| s.as_catalog())
                {
                    mb.set_version(&location, version.as_deref());
                }
            }
            Action::FeedLoad { source } => self.load_feed(source),
            Action::FeedFill {
                source,
                urls,
                front,
            } => self.queue_feed_scenes(&source, urls, front),
            Action::FeedScene { source, url } => self.load_feed_scene(source, url),
            Action::FeedFavorite { source, url, on } => self.mark_feed_favorite(&source, url, on),
            Action::DiscoverDlna => self.discover_dlna(),
            Action::QuickConnectInitiate {
                address,
                insecure_tls,
            } => self.quick_connect_initiate(address, insecure_tls),
            Action::QuickConnectPoll {
                address,
                secret,
                insecure_tls,
            } => self.quick_connect_poll(address, secret, insecure_tls),
            Action::AddDevice(cfg) => {
                if !self.settings.haptic_devices.contains(&cfg) {
                    self.settings.haptic_devices.push(cfg.clone());
                }
                self.connect_device(cfg);
            }
            Action::RemoveDevice(i) => {
                if i < self.settings.haptic_devices.len() {
                    let cfg = self.settings.haptic_devices.remove(i);
                    self.services.remove_device(&cfg);
                }
            }
            Action::ReconnectDevices => {
                self.services.remove_devices();
                self.connect_devices();
            }
            Action::RegenerateToken => {
                if let Some(h) = &self.services.remote
                    && let Err(e) = h.regenerate_token()
                {
                    self.ui.toast(e.to_string());
                }
            }
            Action::NewPairingCode => {
                if let Some(h) = &self.services.remote {
                    h.new_pairing_code();
                }
            }
            Action::CheckUpdates => self.check_updates(),
            Action::InstallUpdate => self.install_update(),
            Action::PlexLinkStart => self.plex_link_start(),
            Action::PlexLinkPoll { generation, pin } => self.plex_link_poll(generation, pin),
            Action::PlexConnect(config) => self.plex_connect(config),
            Action::OpenWebPage(address) => self.open_web_page(address.as_str()),
            Action::SendCrashReport => self.send_crash_report(),
            Action::DismissCrashReport => self.ui.crash = None,
            Action::Recenter => self.recenter(),
            Action::TogglePassthrough => self.settings.passthrough = !self.settings.passthrough,
            Action::ToggleFollow => self.settings.follow = !self.settings.follow,
            Action::ToggleLoopVideo => self.settings.loop_video = !self.settings.loop_video,
            Action::ToggleAutoplay => self.settings.autoplay = !self.settings.autoplay,
            Action::ToggleLoopPlaylist => {
                self.settings.loop_playlist = !self.settings.loop_playlist;
            }
            Action::ShowBrowser(show) => {
                self.show_browser = show;
                if show {
                    self.ui.invalidate();
                    self.refresh_source();
                }
            }
            Action::Quit => self.quit = true,
            Action::Restart => {
                self.quit = true;
                self.restart = true;
            }
            Action::HideControls => {
                self.ui_visible = false;
                self.show_browser = false;
            }
        }
    }

    /// The open video's player.
    pub(super) fn player(&self) -> Option<&fp_media::Player> {
        self.playback.as_ref().map(|p| &p.player)
    }

    fn open_list(&mut self, list: Vec<OpenRequest>, pos: usize, plays_through: bool) {
        if let Some(req) = list.get(pos).cloned() {
            self.queue.set(list, pos, plays_through);
            self.open_request(req);
        }
    }

    /// Play next or Add to queue. With nothing playing, either plays it:
    /// Up next is on the control bar, so a queue started otherwise would
    /// go unseen.
    fn queue_video(&mut self, req: OpenRequest, next: bool) {
        let title = ui::widgets::display_title(&req.shown_title());
        let Some(playing) = self.playback.as_ref().map(|p| p.request.clone()) else {
            self.queue.set(vec![req.clone()], 0, true);
            self.open_request(req);
            return;
        };
        self.queue.add(req, &playing, next);
        self.ui.toast(if next {
            tr!("toast-plays-next", title = title.as_str())
        } else {
            tr!("toast-added-up-next", title = title.as_str())
        });
    }

    /// Adds videos to playlist `id`, or to a new one called `name`. Videos
    /// not in the library yet get a row, as opening them does.
    fn add_to_playlist(&mut self, id: Option<PlaylistId>, name: &str, reqs: &[OpenRequest]) {
        let lib = &self.services.library;
        let media: Vec<_> = reqs.iter().filter_map(|r| media_of(lib, r)).collect();
        let result = match id {
            Some(id) => Ok(id),
            None => lib.create_playlist(name),
        }
        .and_then(|id| lib.add_to_playlist(id, &media).map(|_| id));
        match result {
            Ok(id) => {
                let name = lib
                    .playlist(id)
                    .ok()
                    .flatten()
                    .map(|p| p.name)
                    .unwrap_or_default();
                self.ui.toast(match media.len() {
                    0 => tr!("toast-playlist-made", name = name.as_str()),
                    n => tr!("toast-playlist-added", count = n, name = name.as_str()),
                });
                self.ui.picking = None;
                self.ui.playlist_name.clear();
                self.ui.invalidate();
            }
            Err(e) => self
                .ui
                .toast(tr!("error-add-to-playlist", error = e.to_string())),
        }
    }

    fn toggle_pause(&self) {
        if let Some(p) = self.player() {
            if p.state() == PlayerState::Ended {
                p.seek(0.0, false);
                p.play();
            } else {
                p.toggle_pause();
            }
        }
    }

    /// Plays the video that ended again from the start, as Loop video does;
    /// it counts as watched to the end first.
    pub(super) fn play_again(&mut self) {
        let req = {
            let Some(p) = &mut self.playback else {
                return;
            };
            p.save_progress(&self.services.library, true);
            if p.player.can_seek() {
                p.player.seek(0.0, false);
                p.player.play();
                return;
            }
            // A server that can't start transcoding at a time is asked
            // again from the beginning.
            OpenRequest {
                start_at: Some(0.0),
                ..p.request.clone()
            }
        };
        self.open_request(req);
    }

    /// Closes the video, and drops one still opening.
    pub(super) fn close_playback(&mut self) {
        if let Some(p) = self.playback.take() {
            p.close(&self.services.library);
        }
        if self.ui.opening.take().is_some() {
            self.open_seq += 1;
        }
        if let Some(h) = self.services.haptics() {
            h.clear();
        }
        self.show_browser = true;
        self.ui_visible = true;
        self.ui.invalidate();
        self.refresh_source();
    }

    /// Changes the open video's view adjustments (saved later with the
    /// rest of them).
    fn edit_view(&mut self, edit: impl FnOnce(&mut Playback, &Settings)) {
        if let Some(p) = &mut self.playback {
            edit(p, &self.settings);
            p.settings_dirty = true;
        }
    }

    fn add_bookmark(&mut self) {
        let Some(p) = &mut self.playback else { return };
        match p.add_bookmark(&self.services.library) {
            Ok(()) => self.ui.toast(tr!("toast-bookmark-added")),
            Err(e) => self.not_saved(
                "the bookmark",
                |e| tr!("error-save-bookmark", error = e),
                &e,
            ),
        }
    }

    fn select_subtitle_file(&mut self, i: Option<usize>) {
        let Some(p) = &mut self.playback else { return };
        p.player.clear_external_subtitles();
        let picked = i.and_then(|i| p.subtitle_files.get(i).map(|f| (i, f.clone())));
        p.active_subtitle_file = picked.as_ref().map(|(i, _)| *i);
        let Some((index, file)) = picked else {
            return;
        };
        // Read on a job: the file may be on a server.
        let opener = self.services.opener.clone();
        let seq = self.open_seq;
        self.jobs.spawn("subtitles", move || Job::Subtitles {
            seq,
            index,
            result: playback::read_subtitle_file(&opener, &file),
        });
    }

    fn clear_history(&mut self) {
        match self.services.library.clear_history() {
            Ok(()) => self.ui.toast(tr!("toast-history-cleared")),
            Err(e) => self
                .ui
                .toast(tr!("error-clear-history", error = e.to_string())),
        }
        self.ui.invalidate();
        // The frames where videos were left off go.
        self.services.wake_worker();
    }

    /// After a change to the library: re-query what shows it, and say so
    /// if it could not be saved.
    /// `failed` says so with the error, `what` in the log.
    fn library_changed(
        &mut self,
        what: &str,
        failed: fn(&str) -> String,
        r: fp_library::Result<()>,
    ) {
        if let Err(e) = r {
            self.not_saved(what, failed, &e);
        }
        self.ui.invalidate();
    }

    /// Tells the user a change to the library could not be saved.
    fn not_saved(&mut self, what: &str, failed: fn(&str) -> String, e: &fp_library::Error) {
        log::warn!("saving {what}: {e}");
        self.ui.toast(failed(&e.to_string()));
    }
}

/// The library's video for `req`, given a row from its entry when it has
/// none yet, as opening it does.
fn media_of(lib: &Library, req: &OpenRequest) -> Option<fp_library::MediaId> {
    let source_id = req.bound_source_id().ok()?;
    let location = fp_core::volume::stable(&req.location).into_owned();
    if let Ok(Some(r)) = lib.get_by_location(&location) {
        return Some(r.id);
    }
    let mut e = req.entry.clone().unwrap_or_else(|| {
        fp_core::Entry::new(req.shown_title(), &location, fp_core::EntryKind::Video)
    });
    e.location = location;
    lib.upsert_entry(source_id.as_deref().unwrap_or(""), &e)
        .inspect_err(|err| log::warn!("adding {} to the library: {err}", e.location))
        .ok()
        .map(|o| o.id)
}
