//! The media currently open: player, format, adjustments, scripts,
//! subtitles and markers, plus progress bookkeeping in the library.

use crate::services::Opener;
use crate::settings::Settings;
use crate::subtitles::{self, Pick};
use crate::watch::{Reports, Watch};
use fp_core::format::{self, ContainerHints, DetectedFormat, Evidence, VideoFormat};
use fp_core::source::{Entry, EntryKind, SubtitleFile};
use fp_core::sync::lock;
use fp_core::view::{Keyframes, ViewSettings};
use fp_core::{PlaybackStatus, playback::now_ms};
use fp_haptics::{Axis, Heatmap, Script};
use fp_library::{Library, MediaId, SessionId};
use fp_media::{Cue, Player, PlayerConfig, PlayerState, StreamInfo};
use fp_sources::sidecar::subtitle_file;
use fp_sources::urlutil::last_segment;
use fp_sources::{Source, TranscodeTarget};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct OpenRequest {
    pub location: String,
    pub source_id: Option<String>,
    /// The browse entry, when opened from a source (carries feed metadata).
    pub entry: Option<Entry>,
    /// The library record's title, when opened from the library.
    pub title: Option<String>,
    /// Start here instead of the saved resume position.
    pub start_at: Option<f64>,
    /// The file as it is, or transcoded by the server.
    pub quality: Quality,
    /// What to ask the server to transcode the file to
    /// ([`TranscodeTarget::id`]): what [`Quality::Transcode`] names, or
    /// what Automatic fell back to.
    pub transcode: Option<String>,
    /// Stable original server track IDs, rather than transcoded stream indexes.
    pub source_audio: Option<String>,
    /// "0" explicitly disables server subtitles.
    pub source_subtitle: Option<String>,
    /// Keep the current pause state while changing quality or server tracks.
    pub paused: bool,
}

/// Whether the file plays as it is or the server transcodes it, as the
/// Quality list in the audio and subtitles picker offers them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Quality {
    /// The file; when it can't play here, the server transcodes it to the
    /// tallest height it offers.
    #[default]
    Automatic,
    /// The file, whatever happens.
    Original,
    /// The server transcodes it to this [`TranscodeTarget::id`].
    Transcode(String),
}

impl OpenRequest {
    /// A persistent resource can only be opened through its embedded owner.
    pub(crate) fn bound_source_id(&self) -> Result<Option<String>, String> {
        if !fp_sources::is_source_location(&self.location) {
            return Ok(self.source_id.clone());
        }
        let owner = fp_sources::location_owner(&self.location).ok_or(tr!("open-no-source"))?;
        if self.source_id.as_deref().is_some_and(|id| id != owner) {
            return Err(tr!("open-no-source").into());
        }
        Ok(Some(owner))
    }

    /// What to call the video while it opens: the entry's or the record's
    /// title, else its file name.
    pub fn shown_title(&self) -> String {
        self.entry
            .as_ref()
            .map(|e| e.name.clone())
            .or_else(|| self.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| title_from_name(&last_segment(&self.location)))
    }
}

pub struct Playback {
    pub player: Player,
    pub request: OpenRequest,
    pub title: String,
    pub record_id: Option<MediaId>,
    session: Option<SessionId>,
    pub format: VideoFormat,
    pub evidence: Evidence,
    /// Format the file would have without the user's override.
    pub detected: DetectedFormat,
    pub settings: ViewSettings,
    pub keyframes: Keyframes,
    /// Adjustments differ from what is saved for this video.
    pub settings_dirty: bool,
    /// What is saved for this video (the Passthrough tab saves its part
    /// at once, the rest only with Save).
    saved_settings: ViewSettings,
    /// The chroma key changed and is not saved yet (see [`Self::save_key`]),
    /// and when it was last saved: slider drags save twice a second.
    key_pending: bool,
    key_saved_at: Instant,
    pub heatmap: Option<Heatmap>,
    pub markers: Vec<(f64, String)>,
    pub subtitle_files: Vec<SubtitleFile>,
    pub active_subtitle_file: Option<usize>,
    /// The subtitles Subtitles on / off last turned off, to turn back on.
    pub last_subtitles: Option<Pick>,
    /// The audio and subtitles picked for this video in the player (see
    /// [`Self::save_tracks`]).
    pub picked: fp_library::PickedTracks,
    pub script_count: usize,
    last_save: Instant,
    pub notice: Option<String>,
    /// Last decoding check: when, and the player's stall and frame counts.
    decode_check: (Instant, u64, u64, u64),
    /// Tells the server the video came from how watching it goes.
    watch: Option<Watch>,
    /// What the server can transcode the video to, tallest first.
    pub targets: Vec<TranscodeTarget>,
    /// What the server is transcoding it to; `None` for the file as it is.
    pub transcoding: Option<TranscodeTarget>,
    pub source_tracks: Vec<fp_sources::PlaybackTrack>,
}

/// The result of opening on a background thread.
pub struct Opened {
    pub playback: Playback,
    pub scripts: Vec<(Axis, Script)>,
}

fn title_from_name(name: &str) -> String {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    stem.replace(['_', '.'], " ").trim().to_string()
}

/// The format a source declared for a video: on the browse entry, or, when
/// opened from the library or home (no entry), as saved in its record. A
/// DeoVR scene's `_LR_ALPHA.json` name alone makes a flat screen of a
/// fisheye passthrough video: mask in the corners, no height or forward.
fn declared_format(
    entry: Option<&Entry>,
    record: Option<&fp_library::MediaRecord>,
) -> Option<VideoFormat> {
    entry.and_then(|e| e.format).or_else(|| {
        record
            .filter(|r| r.detected.evidence == Evidence::Metadata)
            .map(|r| r.detected.format)
    })
}

/// What is saved with a video: not where a flat screen was moved to, as it
/// opens where the browse panel is. A 180°/360° picture keeps its turn.
fn kept_with_video(mut s: ViewSettings, format: VideoFormat) -> ViewSettings {
    if format.projection == format::Projection::Flat {
        s.yaw = 0.0;
        s.pitch = 0.0;
    }
    s
}

/// Opens media; blocking (network probing), so run it on a job thread.
pub fn open(
    mut req: OpenRequest,
    opener: &Opener,
    library: &Library,
    settings: &Settings,
    reports: &Reports,
) -> Result<Opened, String> {
    req.source_id = req.bound_source_id()?;
    let legacy_record = (!fp_sources::is_source_location(&req.location))
        .then(|| library.get_by_location(&req.location).ok().flatten())
        .flatten()
        .filter(|record| req.source_id.as_deref() == Some(record.source_id.as_str()));
    let name = last_segment(&req.location);
    let source = req.source_id.as_deref().and_then(|id| opener.source(id));
    // Feeds may need a details call to get the stream URL and metadata.
    let entry = match (&req.entry, &source) {
        (Some(e), Some(src)) if fp_sources::is_source_location(&e.location) => {
            Some(src.details(e).map_err(|e| crate::ui::errors::source(&e))?)
        }
        (Some(e), Some(src)) => Some(src.details(e).unwrap_or_else(|_| e.clone())),
        // From the library or home: a server's details still carry the
        // video's subtitle files and chapters. Its address only names the
        // server, so it goes by the title the library has until then.
        (None, Some(src))
            if fp_sources::is_source_location(&req.location)
                || src.canonical_location(&req.location).is_some()
                || src.as_catalog().is_some() =>
        {
            let title = req.title.clone().unwrap_or_else(|| name.clone());
            let e = Entry::new(title, req.location.clone(), EntryKind::Video);
            let details = src.details(&e);
            if fp_sources::is_source_location(&req.location) {
                Some(details.map_err(|e| crate::ui::errors::source(&e))?)
            } else {
                details.ok()
            }
        }
        (e, _) => e.clone(),
    };
    if let Some(entry) = &entry
        && fp_sources::is_source_location(&entry.location)
    {
        let owner = fp_sources::location_owner(&entry.location).ok_or(tr!("open-no-source"))?;
        if req.source_id.as_deref() != Some(owner.as_str()) {
            return Err(tr!("open-no-source").into());
        }
    }
    let location = entry
        .as_ref()
        .map_or_else(|| req.location.clone(), |e| e.location.clone());
    // What messages call it: a Plex location names no file.
    let label = if name.is_empty() {
        entry
            .as_ref()
            .map_or_else(|| req.shown_title(), |e| e.name.clone())
    } else {
        name.clone()
    };

    // Library record: existing, or created for remote entries (and deleted
    // again if the video doesn't open, so nothing invisible is left behind).
    // A colliding legacy record is kept during migration rather than losing
    // its user data. Opening that history row must retain its own record ID.
    let mut record = legacy_record.or_else(|| library.get_by_location(&location).ok().flatten());
    let mut created = None;
    if let (Some(e), Some(sid)) = (&entry, &req.source_id) {
        match &record {
            None => {
                if library.upsert_entry(sid, e).is_ok() {
                    record = library.get_by_location(&location).ok().flatten();
                    created = record.as_ref().map(|r| r.id);
                }
            }
            // What its own source says now, as titles are kept current; a
            // video another source has (a library's) is left to it.
            Some(r) if &r.source_id == sid => {
                if let Ok(u) = fp_library::MediaUpsert::from_entry(sid, e)
                    && let Err(err) = library.update_details(
                        r.id,
                        &fp_library::SourceDetails {
                            title: Some(u.title.as_str()).filter(|t| !t.is_empty()),
                            description: u.description.as_deref(),
                            thumbnail_url: u.thumbnail_url.as_deref(),
                            tags: u.tags.as_deref(),
                        },
                    )
                {
                    log::warn!("updating the details of {location}: {err}");
                }
                record = library.get(r.id).ok().flatten();
            }
            Some(_) => {}
        }
    }
    let failed = |e: String| {
        if let Some(id) = created
            && let Err(err) = library.remove_media(id)
        {
            log::warn!("forgetting {location}: {err}");
        }
        e
    };

    // A server that keeps resume points knows where the video was left on
    // any device; the library only where it was left here.
    let start_at = req
        .start_at
        .or_else(|| {
            entry
                .as_ref()
                .and_then(|e| e.resume)
                .or_else(|| record.as_ref().map(|r| r.resume_position))
                .filter(|p| settings.resume && *p > 1.0)
        })
        .unwrap_or(0.0);
    // The previous video's decoder must be released first (see
    // App::open_request); a close stuck on the network is not waited out.
    if !wait_closed(CLOSE_WAIT) {
        log::warn!("previous video still closing; opening anyway");
    }
    let (script_locs, subs) = sidecars(
        &location,
        &name,
        entry.as_ref(),
        record.as_ref(),
        source.clone(),
        settings.haptics_enabled,
    );
    let picked = record
        .as_ref()
        .map(|r| r.tracks.clone())
        .unwrap_or_default();
    let source_tracks = source
        .as_ref()
        .filter(|s| s.kind() == fp_sources::SourceKind::Plex)
        .map(|src| {
            let video = entry
                .clone()
                .unwrap_or_else(|| Entry::new(name.clone(), location.clone(), EntryKind::Video));
            src.playback_tracks(&video).unwrap_or_default()
        })
        .unwrap_or_default();
    // What was picked for this video earlier, as the server's track IDs: a
    // transcode has only the tracks the server puts in it.
    if !source_tracks.is_empty() {
        let (audio, subtitle) = saved_source_tracks(&picked, &source_tracks);
        req.source_audio = req.source_audio.take().or(audio);
        req.source_subtitle = req.source_subtitle.take().or(subtitle);
    }
    let open_player = |src: Arc<dyn fp_core::ByteSource>, config: PlayerConfig| {
        let direct = config.timeline.is_none();
        let server_tracks = !direct && !source_tracks.is_empty();
        let mut pick = Pick::Off;
        Player::open(src, &name, config, |info| {
            let audio = if server_tracks {
                None
            } else {
                req.source_audio
                    .as_ref()
                    .and_then(|id| source_tracks.iter().find(|t| !t.subtitle && t.id == *id))
                    .and_then(|t| t.index)
                    .or(picked.audio)
            };
            pick = subtitles::starting(
                picked.subtitles.as_ref(),
                audio,
                settings.subtitles,
                &settings.subtitle_language,
                info,
                &subs,
                &name,
            );
            if let Some(id) = req.source_subtitle.as_deref() {
                let track = source_tracks.iter().find(|t| t.subtitle && t.id == id);
                pick = track
                    .and_then(|t| {
                        t.location
                            .as_ref()
                            .and_then(|location| subs.iter().position(|s| &s.location == location))
                            .map(Pick::File)
                            .or_else(|| t.index.map(Pick::Stream))
                    })
                    .unwrap_or(Pick::Off);
            }
            if server_tracks {
                pick = Pick::Off;
            }
            fp_media::Tracks {
                audio,
                subtitle: pick.stream(),
            }
        })
        .map(|p| (p, pick))
    };
    let mut source_session = source.as_ref().and_then(|src| {
        let video = entry
            .clone()
            .unwrap_or_else(|| Entry::new(name.clone(), location.clone(), EntryKind::Video));
        src.watch_session(&video)
    });
    if source
        .as_ref()
        .is_some_and(|s| s.kind() == fp_sources::SourceKind::Plex)
        && let Some(session) = source_session.as_mut()
    {
        if req.source_audio.is_some() {
            session.audio_stream = req.source_audio.clone();
        }
        session.subtitle_stream = Some(req.source_subtitle.clone().unwrap_or_else(|| "0".into()));
    }
    let targets = source
        .as_ref()
        .map(|src| {
            let video = entry
                .clone()
                .unwrap_or_else(|| Entry::new(name.clone(), location.clone(), EntryKind::Video));
            src.transcode_targets(&video)
        })
        .unwrap_or_default();
    let duration = entry
        .as_ref()
        .and_then(|e| e.duration)
        .or_else(|| record.as_ref().and_then(|r| r.duration))
        .unwrap_or(0.0);
    // From where it was asked to start when the server can start it there
    // (and so seek), else from the beginning.
    let transcoded = |t: &TranscodeTarget| {
        let fail = |e: String| format!("{label} ({}): {e}", t.label);
        let src = source.clone().ok_or_else(|| fail("no server".into()))?;
        let restart = restart_at(src, &location, t, source_session.clone());
        let first = if t.seekable { start_at } else { 0.0 };
        let stream = restart(first).map_err(fail)?;
        let config = PlayerConfig {
            timeline: Some(fp_media::Timeline {
                start: first,
                duration,
                restart: t.seekable.then_some(restart),
            }),
            ..player_config(settings, 0.0)
        };
        open_player(stream, config).map_err(|e| fail(crate::ui::errors::media(&e)))
    };
    let asked = req
        .transcode
        .as_ref()
        .and_then(|id| targets.iter().find(|t| &t.id == id))
        .cloned();
    let mut notice = None;
    let (player, pick, transcoding) = match asked {
        Some(t) => {
            let (p, pick) = transcoded(&t).map_err(failed)?;
            (p, pick, Some(t))
        }
        None => {
            let src = match (&source, &source_session) {
                (Some(src), Some(session)) => match src.open_playback(&location, session) {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(e)
                        if src.kind() == fp_sources::SourceKind::Plex
                            && req.quality == Quality::Automatic
                            && !targets.is_empty()
                            && matches!(e, fp_sources::Error::Unsupported(_)) =>
                    {
                        log::info!("{label}: {e}; asking Plex to transcode");
                        Ok(None)
                    }
                    Err(e) => Err(crate::ui::errors::source(&e)),
                },
                _ => opener
                    .open_in(req.source_id.as_deref(), &location)
                    .map(Some),
            }
            .map_err(failed)?;
            match src.map(|src| open_player(src, player_config(settings, start_at))) {
                Some(Ok((p, pick))) => (p, pick, None),
                Some(Err(e))
                    if e.cannot_play_here()
                        && req.quality == Quality::Automatic
                        && !targets.is_empty() =>
                {
                    let t = targets[0].clone();
                    log::warn!("{label}: {e}; the server transcodes it to {}", t.label);
                    let (p, pick) = transcoded(&t).map_err(failed)?;
                    notice = Some(tr!("transcode-cannot-play", quality = t.label.as_str()));
                    (p, pick, Some(t))
                }
                Some(Err(e)) => {
                    return Err(failed(format!("{label}: {}", crate::ui::errors::media(&e))));
                }
                None => {
                    let t = targets[0].clone();
                    let (p, pick) = transcoded(&t).map_err(failed)?;
                    notice = Some(tr!("transcode-cannot-play", quality = t.label.as_str()));
                    (p, pick, Some(t))
                }
            }
        }
    };
    // Only for this open: the request is cloned for later ones (a Loop
    // restart, Up next), which start playing.
    if std::mem::take(&mut req.paused) {
        player.pause();
    }
    let info = player.info().clone();
    if settings.hardware_decoding {
        warn_if_software(&label, &info, &player.stats());
    }
    let (resolved, detected) =
        detect_format(&name, &info, entry.as_ref(), record.as_ref(), settings);
    // A server's video is recorded with only a guess at its format until
    // it is probed; what the opened file says shows in the history at once.
    // A transcode says less than the file (servers drop its VR metadata).
    if let Some(r) = &record
        && transcoding.is_none()
        && detected.evidence != Evidence::Default
        && let Err(e) = library.set_detected(r.id, detected)
    {
        log::warn!("saving the format of {location}: {e}");
    }

    let script_source = req
        .source_id
        .clone()
        .or_else(|| record.as_ref().map(|r| r.source_id.clone()))
        .filter(|id| !id.is_empty());
    let scripts = load_scripts(opener, script_source.as_deref(), &script_locs);
    let duration_ms = (info.duration * 1000.0) as i64;
    let heatmap = scripts
        .iter()
        .find(|(a, _)| *a == Axis::L0)
        .filter(|_| duration_ms > 0)
        .map(|(_, s)| fp_haptics::heatmap_range(s, 0, duration_ms, 240));
    let active_subtitle_file = match pick {
        Pick::File(i) => read_subtitle_file(opener, &subs[i]).ok().map(|cues| {
            player.set_external_subtitles(cues);
            i
        }),
        _ => None,
    };

    let settings_v = record
        .as_ref()
        .and_then(|r| r.view_settings)
        .unwrap_or_else(|| settings.new_video_view());
    let keyframes = record
        .as_ref()
        .map(|r| r.keyframes.clone())
        .unwrap_or_default();
    let title = record
        .as_ref()
        .map(|r| r.title.clone())
        .filter(|t| !t.is_empty())
        .or_else(|| info.title.clone())
        .or_else(|| entry.as_ref().map(|e| e.name.clone()))
        .unwrap_or_else(|| title_from_name(&name));
    let markers = markers(&info, entry.as_ref(), record.as_ref(), library);
    let record_id = record.as_ref().map(|r| r.id);
    let session = record_id.and_then(|id| library.start_playback(id).ok());
    let watch = source.and_then(|src| {
        let video = entry
            .clone()
            .unwrap_or_else(|| Entry::new(title.clone(), location.clone(), EntryKind::Video));
        match source_session {
            Some(session) => Some(Watch::start_session(reports, src, session, start_at)),
            None => Watch::start(reports, src, &video, start_at),
        }
    });
    Ok(Opened {
        playback: Playback {
            player,
            request: OpenRequest {
                // A legacy row retained after a migration collision keeps
                // its own user state across quality changes and reopens.
                location: record.as_ref().map_or(location, |r| r.location.clone()),
                entry,
                transcode: transcoding.as_ref().map(|t| t.id.clone()),
                ..req
            },
            title,
            record_id,
            session,
            format: resolved.format,
            evidence: resolved.evidence,
            detected,
            settings: settings_v,
            saved_settings: settings_v,
            key_pending: false,
            key_saved_at: Instant::now(),
            keyframes,
            settings_dirty: false,
            heatmap,
            markers,
            subtitle_files: subs,
            active_subtitle_file,
            last_subtitles: None,
            picked,
            script_count: scripts.len(),
            last_save: Instant::now(),
            notice,
            decode_check: (Instant::now(), 0, 0, 0),
            watch,
            targets,
            transcoding,
            source_tracks,
        },
        scripts,
    })
}

/// Asks `source` to transcode `location` to `t` from a time (seconds).
fn restart_at(
    source: Arc<dyn Source>,
    location: &str,
    t: &TranscodeTarget,
    session: Option<fp_sources::WatchSession>,
) -> fp_media::Restart {
    let (location, id) = (location.to_string(), t.id.clone());
    Arc::new(move |at| {
        match &session {
            Some(session) => source.open_transcode_session(&location, &id, at, session),
            None => source.open_transcode(&location, &id, at),
        }
        .map_err(|e| e.to_string())
    })
}

fn player_config(settings: &Settings, start_at: f64) -> PlayerConfig {
    PlayerConfig {
        hw_decode: if settings.hardware_decoding {
            fp_media::HwDecode::Auto
        } else {
            fp_media::HwDecode::Off
        },
        audio: fp_media::audio::output::Backend::Alsa {
            device: settings.audio_device.clone(),
            latency_ms: 80,
        },
        start_at,
        volume: settings.volume,
        ..Default::default()
    }
}

/// Logs why a video the hardware could decode is decoded in software.
/// What was picked for a video, as its server's track IDs: audio, then
/// subtitles ("0" for off).
fn saved_source_tracks(
    picked: &fp_library::PickedTracks,
    tracks: &[fp_sources::PlaybackTrack],
) -> (Option<String>, Option<String>) {
    use fp_library::PickedSubtitles;
    let audio = picked
        .audio
        .and_then(|i| tracks.iter().find(|t| !t.subtitle && t.index == Some(i)))
        .map(|t| t.id.clone());
    let subtitles = tracks.iter().filter(|t| t.subtitle);
    let subtitle = match &picked.subtitles {
        None => None,
        Some(PickedSubtitles::Off) => Some("0".to_string()),
        Some(PickedSubtitles::Stream(i)) => subtitles
            .clone()
            .find(|t| t.index == Some(*i))
            .map(|t| t.id.clone()),
        Some(PickedSubtitles::File(file)) => subtitles
            .clone()
            .find(|t| t.location.as_ref() == Some(file))
            .map(|t| t.id.clone()),
    };
    (audio, subtitle)
}

/// How to keep a server's subtitle track picked while transcoding (`None` is
/// off), the way a pick in the player is kept.
pub(crate) fn picked_source_subtitles(
    id: Option<&str>,
    tracks: &[fp_sources::PlaybackTrack],
) -> Option<fp_library::PickedSubtitles> {
    use fp_library::PickedSubtitles;
    let Some(id) = id else {
        return Some(PickedSubtitles::Off);
    };
    let track = tracks.iter().find(|t| t.subtitle && t.id == id)?;
    track
        .location
        .clone()
        .map(PickedSubtitles::File)
        .or_else(|| track.index.map(PickedSubtitles::Stream))
}

fn warn_if_software(name: &str, info: &fp_media::MediaInfo, st: &fp_media::Stats) {
    if !st.hardware && info.video_stream().is_some_and(has_hardware_decoder) {
        log::warn!(
            "{name}: no hardware decoder ({}), decoding with {}; {}",
            st.hardware_error.as_deref().unwrap_or("not tried"),
            st.video_decoder,
            hardware_holders()
        );
    }
}

/// The format to play the video in, and the one detected without the
/// user's override: user override > container metadata > declared by the
/// source > file name > aspect ratio > the default chosen in Settings.
fn detect_format(
    name: &str,
    info: &fp_media::MediaInfo,
    entry: Option<&Entry>,
    record: Option<&fp_library::MediaRecord>,
    settings: &Settings,
) -> (DetectedFormat, DetectedFormat) {
    let (w, h) = info
        .video_stream()
        .map(StreamInfo::display_size)
        .unwrap_or((0, 0));
    let user = record.and_then(|r| r.user_format);
    let hints = if info.hints.projection.is_some() || info.hints.stereo.is_some() {
        info.hints
    } else if let Some(f) = declared_format(entry, record) {
        ContainerHints {
            projection: Some(f.projection),
            stereo: Some(f.stereo),
            eyes_swapped: f.eyes_swapped,
        }
    } else {
        info.hints
    };
    let unidentified = |mut d: DetectedFormat| {
        if d.evidence == Evidence::Default {
            d.format = settings.default_format;
        }
        d
    };
    let resolved = unidentified(format::resolve(user, hints, name, w, h));
    let detected = unidentified(format::resolve(None, hints, name, w, h));
    log::info!(
        "format {}{} ({}), {w}x{h}",
        resolved.format.label(),
        if resolved.format.alpha_packed {
            " with mask"
        } else {
            ""
        },
        resolved.evidence.label()
    );
    (resolved, detected)
}

/// Haptic script locations (with `with_scripts`) and subtitle files for the
/// video `name`: from its library record, its entry, then the source's
/// sidecar files; scripts of a local file not otherwise found are looked for
/// beside it and in the Interactive folder.
fn sidecars(
    location: &str,
    name: &str,
    entry: Option<&Entry>,
    record: Option<&fp_library::MediaRecord>,
    source: Option<Arc<dyn Source>>,
    with_scripts: bool,
) -> (Vec<String>, Vec<SubtitleFile>) {
    let mut scripts: Vec<String> = record.map(|r| r.scripts.clone()).unwrap_or_default();
    let mut subs = Vec::new();
    if let Some(e) = entry {
        scripts.extend(e.scripts.iter().cloned());
        match source.and_then(|src| src.sidecars(e).ok()) {
            Some(side) => {
                scripts.extend(side.scripts.into_iter().map(|s| s.location));
                subs.extend(side.subtitles);
            }
            None => subs.extend(fp_sources::Sidecars::from_entry(e).subtitles),
        }
    }
    // The record knows only where its files are; their names tell the rest.
    if let Some(r) = record {
        subs.extend(r.subtitles.iter().map(|l| subtitle_file(l, name)));
    }
    if !with_scripts {
        scripts.clear();
    } else if scripts.is_empty()
        && let Some(path) = fp_core::volume::resolve(location).filter(|p| p.starts_with('/'))
    {
        let dirs = vec![fp_core::dirs::interactive_dir()];
        scripts = fp_haptics::funscript::find_scripts(std::path::Path::new(path.as_ref()), &dirs)
            .into_iter()
            .map(|(_, p)| p.display().to_string())
            .collect();
    }
    scripts.dedup();
    let mut seen = std::collections::HashSet::new();
    subs.retain(|s| seen.insert(s.location.clone()));
    (scripts, subs)
}

fn load_scripts(
    opener: &Opener,
    source_id: Option<&str>,
    locations: &[String],
) -> Vec<(Axis, Script)> {
    let mut scripts = Vec::new();
    for loc in locations {
        match opener.fetch_in(source_id, loc, 64 << 20).and_then(|bytes| {
            fp_haptics::funscript::parse_script_file(&last_segment(loc), &bytes)
                .map_err(|e| e.to_string())
        }) {
            Ok(mut s) => scripts.append(&mut s),
            Err(e) => log::warn!("script {loc}: {e}"),
        }
    }
    scripts
}

/// A subtitle file's cues. Blocks while a server answers, so not on the
/// frame loop.
pub fn read_subtitle_file(opener: &Opener, file: &SubtitleFile) -> Result<Vec<Cue>, String> {
    let read = opener.open(&file.location).and_then(|s| {
        fp_media::subtitle::load_file(s, &last_segment(&file.location)).map_err(|e| e.to_string())
    });
    if let Err(e) = &read {
        log::warn!("subtitles {}: {e}", file.location);
    }
    read
}

/// Chapters, the source's markers and the user's bookmarks, in time order.
fn markers(
    info: &fp_media::MediaInfo,
    entry: Option<&Entry>,
    record: Option<&fp_library::MediaRecord>,
    library: &Library,
) -> Vec<(f64, String)> {
    let mut markers: Vec<(f64, String)> = info
        .chapters
        .iter()
        .map(|c| (c.start, c.title.clone()))
        .collect();
    if let Some(e) = entry {
        markers.extend(e.markers.iter().cloned());
    }
    if let Some(r) = record
        && let Ok(m) = library.markers(r.id)
    {
        markers.extend(m.into_iter().map(|m| (m.time, m.name)));
    }
    markers.sort_by(|a, b| a.0.total_cmp(&b.0));
    markers.dedup_by(|a, b| (a.0 - b.0).abs() < 0.5 && a.1 == b.1);
    markers
}

impl Playback {
    /// The subtitles showing: a file, an embedded stream, or none.
    pub fn subtitles_showing(&self) -> Pick {
        match (self.active_subtitle_file, self.player.subtitle_stream()) {
            (Some(i), _) => Pick::File(i),
            (None, Some(s)) => Pick::Stream(s),
            (None, None) => Pick::Off,
        }
    }

    pub fn status(&self) -> PlaybackStatus {
        let p = &self.player;
        PlaybackStatus {
            location: self.request.location.clone(),
            title: self.title.clone(),
            duration: p.duration(),
            position: p.position(),
            speed: p.speed(),
            playing: matches!(p.state(), PlayerState::Playing),
            sampled_at_ms: now_ms(),
        }
    }

    /// View settings at the current position (keyframes applied).
    pub fn current_settings(&self) -> ViewSettings {
        self.keyframes.at(self.player.position(), &self.settings)
    }

    /// What is shown now: [`Self::current_settings`] with the global chroma
    /// key unless this video has its own.
    pub fn shown_settings(&self, global: &ViewSettings) -> ViewSettings {
        self.current_settings().with_global_key(global)
    }

    /// Saves progress every few seconds and at the end.
    pub fn save_progress(&mut self, library: &Library, force: bool) {
        if !force && self.last_save.elapsed().as_secs_f64() < 5.0 {
            return;
        }
        self.last_save = Instant::now();
        let finished = self.player.state() == PlayerState::Ended;
        if let Some(s) = self.session
            && let Err(e) = library.update_playback(s, self.player.position(), finished)
        {
            log::warn!("saving progress: {e}");
        }
    }

    /// Every frame: tells the video's server how watching goes.
    pub fn sync_watch(&mut self) {
        if let Some(w) = &mut self.watch {
            let p = &self.player;
            let state = p.state();
            w.update(
                p.position(),
                p.is_paused() || state == PlayerState::Ended,
                p.speed(),
                state != PlayerState::Buffering,
            );
        }
    }

    /// Every few seconds: when software decoding falls behind, says why in
    /// the control bar (the picture stutters with no other explanation).
    /// `hardware_on` is the hardware decoding setting.
    pub fn watch_decoding(&mut self, hardware_on: bool) {
        if self.decode_check.0.elapsed().as_secs_f64() < DECODE_CHECK_SECS {
            return;
        }
        let st = self.player.stats();
        let (_, stalls, dropped, shown) = self.decode_check;
        self.decode_check = (
            Instant::now(),
            st.stalls,
            st.frames_dropped,
            st.frames_shown,
        );
        if st.hardware || self.notice.is_some() || self.player.is_paused() {
            return;
        }
        if !falling_behind(
            st.stalls - stalls,
            st.frames_dropped - dropped,
            st.frames_shown - shown,
        ) {
            return;
        }
        let Some(v) = self.player.info().video_stream() else {
            return;
        };
        let notice = slow_decode_notice(v, hardware_on, st.hardware_error.as_deref());
        log::info!(
            "software decoding ({}, {} {}x{} {}) behind: {} stalls, {} of {} frames dropped; \
             hardware: {}; {}",
            st.video_decoder,
            v.codec,
            v.width,
            v.height,
            if v.pixel_format.is_empty() {
                "pixel format unknown"
            } else {
                &v.pixel_format
            },
            st.stalls - stalls,
            st.frames_dropped - dropped,
            st.frames_shown - shown + st.frames_dropped - dropped,
            match (&st.hardware_error, hardware_on) {
                (Some(e), _) => e.as_str(),
                (None, false) => "off in Settings",
                (None, true) => "not tried",
            },
            hardware_holders()
        );
        self.notice = Some(notice);
    }

    pub fn set_format(&mut self, library: &Library, format: Option<VideoFormat>) {
        self.format = format.unwrap_or(self.detected.format);
        self.evidence = if format.is_some() {
            Evidence::User
        } else {
            self.detected.evidence
        };
        if let Some(id) = self.record_id
            && let Err(e) = library.set_user_format(id, format)
        {
            log::warn!("saving format: {e}");
        }
    }

    /// Puts a flat screen the way the browse panel is turned, `yaw`
    /// degrees (left positive), as it opens: upright, at eye height.
    pub fn place_flat(&mut self, yaw: f32) {
        if self.format.projection == format::Projection::Flat {
            for s in [&mut self.settings, &mut self.saved_settings] {
                [s.yaw, s.pitch] = [yaw, 0.0];
            }
        }
    }

    /// This video's format with its see-through mask turned on or off, when
    /// it is a side-by-side VR video and so could carry one. There is no
    /// screen to show the room around, so this is what the Room button does.
    pub fn mask_toggled(&self) -> Option<VideoFormat> {
        let f = self.format;
        (f.projection != format::Projection::Flat && f.stereo.is_side_by_side()).then_some(
            VideoFormat {
                alpha_packed: !f.alpha_packed,
                ..f
            },
        )
    }

    /// Saves the adjustments with the video: true when saved, false when the
    /// video isn't in the library (its notice says so). A failure keeps them
    /// unsaved, to save again.
    pub fn save_settings(&mut self, library: &Library) -> fp_library::Result<bool> {
        let Some(id) = self.record_id else {
            self.notice = Some(tr!("notice-not-in-library").into());
            return Ok(false);
        };
        library
            .set_view_settings(id, Some(&kept_with_video(self.settings, self.format)))
            .and_then(|_| library.set_keyframes(id, &self.keyframes))
            .inspect_err(|e| log::warn!("saving adjustments: {e}"))?;
        self.settings_dirty = false;
        self.saved_settings = self.settings;
        Ok(true)
    }

    /// Saves the audio and subtitles picked in the player, which the video
    /// opens with next time; what wasn't picked keeps following the
    /// settings.
    pub fn save_tracks(&self, library: &Library) {
        let Some(id) = self.record_id else { return };
        if let Err(e) = library.set_tracks(id, &self.picked) {
            log::warn!(
                "saving the tracks picked for {}: {e}",
                self.request.location
            );
        }
    }

    /// Saves this video's chroma key (the Passthrough tab) without the
    /// other, unsaved adjustments: the tab promises it is kept with the
    /// video, and toggling it every time was the result of waiting for Save.
    pub fn save_key(&mut self, library: &Library, force: bool) {
        if !self.key_pending || (!force && self.key_saved_at.elapsed().as_secs_f64() < 0.5) {
            return;
        }
        self.key_pending = false;
        self.key_saved_at = Instant::now();
        let Some(id) = self.record_id else {
            return;
        };
        let mut saved = self.saved_settings;
        saved.set_key(&self.settings);
        saved.key_own = self.settings.key_own;
        match library.set_view_settings(id, Some(&kept_with_video(saved, self.format))) {
            Ok(()) => {
                self.saved_settings = saved;
                self.settings_dirty = self.settings != saved;
            }
            Err(e) => log::warn!("saving the background removal: {e}"),
        }
    }

    /// The Passthrough tab changed the chroma key; [`Self::save_key`] saves it.
    pub fn key_changed(&mut self) {
        self.key_pending = true;
    }

    pub fn add_bookmark(&mut self, library: &Library) -> fp_library::Result<()> {
        let t = self.player.position();
        let name = tr!("bookmark-name", time = crate::ui::fmt_time(t));
        if let Some(id) = self.record_id {
            library.add_marker(id, t, &name)?;
        }
        self.markers.push((t, name));
        self.markers.sort_by(|a, b| a.0.total_cmp(&b.0));
        Ok(())
    }

    /// Saves where the video was and closes it. The player's threads are
    /// stopped and joined on a thread of their own: one can be inside a
    /// network read or a long software decode, and waiting for it here
    /// froze the headset's frame loop (quitting while a 10-bit file over
    /// SMB was buffering hung the app).
    pub fn close(mut self, library: &Library) {
        self.save_progress(library, true);
        self.save_key(library, true);
        if let Some(w) = self.watch.take() {
            // Near the end, the server counts it as watched.
            w.stop(self.player.position());
        }
        let player = self.player;
        let closing = std::thread::Builder::new()
            .name("fp-close".into())
            .spawn(move || {
                let t = Instant::now();
                drop(player);
                log::info!("video closed in {} ms", t.elapsed().as_millis());
            });
        match closing {
            Ok(h) => lock(&CLOSING).push(h),
            // The player was dropped (closed) here.
            Err(e) => log::warn!("cannot start close thread: {e}"),
        }
    }
}

/// Who in FramePlayerX holds hardware decoders, for the log when a video
/// cannot get one: sessions left by videos that never finished closing
/// keep every later video in software until a restart.
fn hardware_holders() -> String {
    let closing = lock(&CLOSING).iter().filter(|h| !h.is_finished()).count();
    format!(
        "FramePlayerX holds {} hardware decoder session(s), {closing} closed video(s) still closing",
        fp_media::hardware_sessions()
    )
}

/// Players still closing (see [`Playback::close`]).
static CLOSING: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

/// How long opening a video waits for the previous one to close.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

/// Waits up to `limit` for closing players to finish; true when none is
/// left.
pub fn wait_closed(limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    let mut closing = lock(&CLOSING);
    loop {
        let (done, open): (Vec<_>, Vec<_>) = std::mem::take(&mut *closing)
            .into_iter()
            .partition(JoinHandle::is_finished);
        for h in done {
            let _ = h.join();
        }
        *closing = open;
        if closing.is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        drop(closing);
        std::thread::sleep(Duration::from_millis(10));
        closing = lock(&CLOSING);
    }
}

const DECODE_CHECK_SECS: f64 = 10.0;

/// Over one check period: playback stalled more than once waiting for the
/// decoder, or dropped more than 5% of frames.
fn falling_behind(stalls: u64, dropped: u64, shown: u64) -> bool {
    stalls >= 2 || dropped * 20 > dropped + shown
}

/// Whether the Frame's hardware decoder takes this stream.
fn has_hardware_decoder(v: &StreamInfo) -> bool {
    hardware_decodes(v, fp_media::hevc_10bit_hardware())
}

/// 8-bit 4:2:0 HEVC, H.264 and VP9, and 10-bit 4:2:0 HEVC when `hevc_10bit`
/// (the renderer shows it, see [`fp_media::set_hevc_10bit_hardware`]).
fn hardware_decodes(v: &StreamInfo, hevc_10bit: bool) -> bool {
    let depth = v.bit_depth <= 8 || (hevc_10bit && v.codec == "hevc" && v.bit_depth == 10);
    matches!(v.codec.as_str(), "hevc" | "h264" | "vp9") && depth && v.chroma() == "4:2:0"
}

/// The stream's codec, with its bit depth and chroma when they are not
/// plain 8-bit 4:2:0 ("10-bit HEVC", "4:2:2 HEVC").
fn stream_desc(v: &StreamInfo) -> String {
    let codec = match v.codec.as_str() {
        "hevc" => "HEVC".to_string(),
        "h264" => "H.264".to_string(),
        "av1" => "AV1".to_string(),
        "vp9" => "VP9".to_string(),
        c => c.to_uppercase(),
    };
    let chroma = v.chroma();
    match (v.bit_depth > 8, chroma != "4:2:0") {
        (true, true) => format!("{}-bit {chroma} {codec}", v.bit_depth),
        (true, false) => format!("{}-bit {codec}", v.bit_depth),
        (false, true) => format!("{chroma} {codec}"),
        (false, false) => codec,
    }
}

/// Why a video decodes in software, for the stream info: the hardware
/// decoder's error when it was tried (less the "unsupported: " most start
/// with), else why it wasn't.
pub(crate) fn software_reason(
    v: &StreamInfo,
    hardware_on: bool,
    hardware_error: Option<&str>,
) -> String {
    match hardware_error {
        Some(e) => e.strip_prefix("unsupported: ").unwrap_or(e).to_string(),
        None if !has_hardware_decoder(v) => tr!("decoder-why-none", stream = stream_desc(v)),
        None if !hardware_on => tr!("decoder-why-off").to_string(),
        None => tr!("decoder-why-untried").to_string(),
    }
}

/// Why a software-decoded video stutters, and what plays smoothly instead.
/// `hardware_on` is the hardware decoding setting; `hardware_error` why the
/// hardware decoder, when tried, is not decoding.
pub(crate) fn slow_decode_notice(
    v: &StreamInfo,
    hardware_on: bool,
    hardware_error: Option<&str>,
) -> String {
    let desc = stream_desc(v);
    if !has_hardware_decoder(v) {
        return tr!("decoder-notice-none", stream = desc);
    }
    if !hardware_on {
        return tr!("decoder-notice-off", stream = desc);
    }
    // Out of decoder memory or sessions: other apps (Steam's streaming
    // among them) hold the hardware decoder.
    let busy = hardware_error.is_none_or(|e| {
        let e = e.to_ascii_lowercase();
        e.contains("memory") || e.contains("busy") || e.contains("no free")
    });
    if busy {
        tr!("decoder-notice-busy", stream = desc)
    } else {
        let size = format!("{}x{}", v.width, v.height);
        tr!("decoder-notice-refused", stream = desc, size = size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_owned_requests_bind_the_owner_without_a_library_record() {
        let location = fp_sources::location::encode("server", "mb", "item=movie&version=edition");
        let mut request = OpenRequest {
            location,
            ..Default::default()
        };
        assert_eq!(
            request.bound_source_id().unwrap().as_deref(),
            Some("server")
        );
        request.source_id = Some("another-account".into());
        assert!(request.bound_source_id().is_err());
        request.source_id = None;
        request.location = "source://wrong/server/mb/item".into();
        assert!(request.bound_source_id().is_err());
        request.location = "http://public.example/movie.mp4".into();
        assert!(request.bound_source_id().unwrap().is_none());
    }

    #[test]
    fn an_entry_cannot_change_another_sources_saved_metadata() {
        let library = Library::open_in_memory().unwrap();
        let private = fp_sources::location::encode("private", "http", "clip.mp4");
        let id = library
            .upsert_entry(
                "private",
                &Entry::new("Private title", &private, EntryKind::Video),
            )
            .unwrap()
            .id;
        let opener = Opener::new();
        opener.insert_source(Arc::new(fp_sources::LocalSource::new(
            &fp_sources::LocalConfig {
                id: "public".into(),
                name: "Public".into(),
                root: "/".into(),
            },
        )));
        let request = OpenRequest {
            location: "/public/clip.mp4".into(),
            source_id: Some("public".into()),
            entry: Some(Entry::new("Changed title", private, EntryKind::Video)),
            ..Default::default()
        };
        assert!(
            open(
                request,
                &opener,
                &library,
                &Settings::default(),
                &Reports::start()
            )
            .is_err()
        );
        assert_eq!(library.get(id).unwrap().unwrap().title, "Private title");
        assert!(library.history(10).unwrap().is_empty());
    }

    #[test]
    fn titles_from_names() {
        assert_eq!(title_from_name("My.Video_2024.mkv"), "My Video 2024");
    }

    struct RootedClip {
        root: &'static str,
        opened: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl Source for RootedClip {
        fn id(&self) -> &str {
            "rooted"
        }
        fn name(&self) -> &str {
            "Rooted clip"
        }
        fn kind(&self) -> fp_sources::SourceKind {
            fp_sources::SourceKind::Http
        }
        fn describe(&self) -> String {
            self.root.into()
        }
        fn canonical_location(&self, location: &str) -> Option<String> {
            let key = if fp_sources::is_source_location(location) {
                let reference = fp_sources::location::decode(location)?;
                if reference.source_id != self.id() || reference.kind != "http" {
                    return None;
                }
                reference.key
            } else {
                location.strip_prefix(self.root)?.into()
            };
            Some(fp_sources::location::encode(self.id(), "http", &key))
        }
        fn list(&self, _: Option<&str>) -> fp_sources::Result<Vec<Entry>> {
            Ok(Vec::new())
        }
        fn open(&self, location: &str) -> fp_sources::Result<Arc<dyn fp_core::ByteSource>> {
            let reference = fp_sources::location::decode(location)
                .filter(|r| r.source_id == self.id() && r.kind == "http" && r.key == "clip.mp4")
                .ok_or_else(|| fp_sources::Error::Unsupported("not this source's clip".into()))?;
            lock(&self.opened).push((self.root.into(), reference.key));
            let clip = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../fp-media/tests/data/h264_aac_180_LR.mp4");
            Ok(Arc::new(fp_core::source::FileSource::open(&clip)?))
        }
    }

    fn reopen_rooted_clip(legacy_collision: bool) {
        let library = Library::open_in_memory().unwrap();
        let stable = fp_sources::location::encode("rooted", "http", "clip.mp4");
        let original = "http://old.example/videos/clip.mp4";
        let canonical_id = library
            .upsert(&fp_library::MediaUpsert::new(&stable, "rooted"))
            .unwrap()
            .id;
        let canonical_view = ViewSettings {
            zoom: 1.25,
            brightness: 0.1,
            ..Default::default()
        };
        library
            .set_view_settings(canonical_id, Some(&canonical_view))
            .unwrap();
        let (expected_id, expected_view) = if legacy_collision {
            let id = library
                .upsert(&fp_library::MediaUpsert::new(original, "rooted"))
                .unwrap()
                .id;
            let view = ViewSettings {
                zoom: 1.75,
                brightness: -0.2,
                ..Default::default()
            };
            library.set_view_settings(id, Some(&view)).unwrap();
            library.set_favorite(id, true).unwrap();
            (id, view)
        } else {
            (canonical_id, canonical_view)
        };
        let playlist = library.create_playlist("Keep this version").unwrap();
        library.add_to_playlist(playlist, &[expected_id]).unwrap();
        let opened = Arc::new(Mutex::new(Vec::new()));
        let opener = Opener::new();
        opener.insert_source(Arc::new(RootedClip {
            root: "http://old.example/videos/",
            opened: opened.clone(),
        }));
        let settings = Settings {
            hardware_decoding: false,
            resume: false,
            haptics_enabled: false,
            ..Default::default()
        };
        let reports = Reports::start();
        let request = OpenRequest {
            location: original.into(),
            source_id: Some("rooted".into()),
            entry: Some(Entry::new("Clip", original, EntryKind::Video)),
            paused: true,
            ..Default::default()
        };
        let first = open(request, &opener, &library, &settings, &reports)
            .unwrap()
            .playback;
        assert_eq!(first.record_id, Some(expected_id));
        assert_eq!(first.settings, expected_view);
        assert_eq!(first.request.entry.as_ref().unwrap().location, stable);
        assert!(first.player.is_paused());
        assert!(!first.request.paused, "for this open only");
        let mut reopen = first.request.clone();
        reopen.paused = true;
        reopen.quality = Quality::Original;
        reopen.start_at = Some(0.0);
        first.close(&library);

        // Quality changes clone Playback.request. The source now has a
        // different endpoint and must receive the canonical entry, while
        // a legacy row retained after a collision must keep its own ID.
        opener.insert_source(Arc::new(RootedClip {
            root: "https://new.example/moved/",
            opened: opened.clone(),
        }));
        let second = open(reopen, &opener, &library, &settings, &reports)
            .unwrap()
            .playback;
        assert_eq!(second.record_id, Some(expected_id));
        assert_eq!(second.settings, expected_view);
        assert_eq!(second.request.entry.as_ref().unwrap().location, stable);
        assert_eq!(
            second.request.location,
            if legacy_collision { original } else { &stable }
        );
        assert_eq!(
            *lock(&opened),
            [
                ("http://old.example/videos/".into(), "clip.mp4".into()),
                ("https://new.example/moved/".into(), "clip.mp4".into()),
            ]
        );
        let history = library.history(10).unwrap();
        assert_eq!(history.len(), 2);
        assert!(history.iter().all(|entry| entry.media_id == expected_id));
        assert_eq!(
            library.playlist_items(playlist).unwrap()[0].record.id,
            expected_id
        );
        assert_eq!(
            library.get(canonical_id).unwrap().unwrap().view_settings,
            Some(canonical_view)
        );
        if legacy_collision {
            assert!(library.get(expected_id).unwrap().unwrap().favorite);
            assert!(library.get_by_location(original).unwrap().is_some());
        } else {
            assert!(library.get_by_location(original).unwrap().is_none());
        }
        second.close(&library);
        assert!(wait_closed(Duration::from_secs(5)));
    }

    #[test]
    fn canonical_entries_survive_endpoint_changes_and_quality_reopens() {
        reopen_rooted_clip(false);
    }

    #[test]
    fn colliding_legacy_history_keeps_its_user_data_on_quality_reopen() {
        reopen_rooted_clip(true);
    }

    /// A server whose file this player can't read, which transcodes it to
    /// 1080p or 720p (a real clip).
    #[derive(Default)]
    struct Unplayable {
        files: Mutex<u32>,
        asked: Mutex<Vec<(String, f64)>>,
        plex: bool,
    }

    impl Source for Unplayable {
        fn id(&self) -> &str {
            "server"
        }
        fn name(&self) -> &str {
            "Server"
        }
        fn kind(&self) -> fp_sources::SourceKind {
            if self.plex {
                fp_sources::SourceKind::Plex
            } else {
                fp_sources::SourceKind::MediaBrowser
            }
        }
        fn describe(&self) -> String {
            String::new()
        }
        fn list(&self, _: Option<&str>) -> fp_sources::Result<Vec<Entry>> {
            Ok(Vec::new())
        }
        fn open(&self, _: &str) -> fp_sources::Result<Arc<dyn fp_core::ByteSource>> {
            *lock(&self.files) += 1;
            if self.plex {
                return Err(fp_sources::Error::Unsupported(
                    "Plex requires conversion".into(),
                ));
            }
            Ok(Arc::new(fp_core::source::MemorySource(vec![7; 64 << 10])))
        }
        fn watch_session(&self, _: &Entry) -> Option<fp_sources::WatchSession> {
            self.plex.then(|| fp_sources::WatchSession {
                item: "1".into(),
                title: "Film".into(),
                endpoint: None,
                id: "session".into(),
                duration_ms: Some(300000),
                media_id: Some("10".into()),
                audio_stream: None,
                subtitle_stream: None,
            })
        }
        fn transcode_targets(&self, _: &Entry) -> Vec<TranscodeTarget> {
            [1080, 720]
                .map(|h| TranscodeTarget {
                    id: h.to_string(),
                    label: format!("{h}p"),
                    height: Some(h),
                    seekable: true,
                })
                .to_vec()
        }
        fn open_transcode(
            &self,
            _: &str,
            id: &str,
            start: f64,
        ) -> fp_sources::Result<Arc<dyn fp_core::ByteSource>> {
            lock(&self.asked).push((id.to_string(), start));
            let clip = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../fp-media/tests/data/h264_aac_180_LR.mp4");
            Ok(Arc::new(fp_core::source::FileSource::open(&clip)?))
        }
    }

    fn open_from(
        server: Arc<Unplayable>,
        quality: Quality,
        transcode: Option<&str>,
    ) -> Result<Opened, String> {
        let opener = Opener::new();
        opener.insert_source(server);
        let mut entry = Entry::new("Film", "mb:http://s#item=1", EntryKind::Video);
        entry.duration = Some(300.0);
        let req = OpenRequest {
            location: entry.location.clone(),
            source_id: Some("server".into()),
            entry: Some(entry),
            start_at: Some(30.0),
            quality,
            transcode: transcode.map(str::to_string),
            ..Default::default()
        };
        let settings = Settings {
            resume: false,
            ..Settings::default()
        };
        let lib = Library::open_in_memory().unwrap();
        open(req, &opener, &lib, &settings, &Reports::start())
    }

    #[test]
    fn a_file_that_cannot_play_plays_the_servers_transcode() {
        let server = Arc::new(Unplayable::default());
        let pb = open_from(server.clone(), Quality::Automatic, None)
            .unwrap()
            .playback;
        assert_eq!(pb.transcoding.as_ref().map(|t| t.id.as_str()), Some("1080"));
        assert_eq!(pb.request.transcode.as_deref(), Some("1080"));
        assert!(pb.notice.as_deref().is_some_and(|n| n.contains("1080p")));
        // From where it was asked, as long as the video, and it seeks.
        assert_eq!(*lock(&server.asked), [("1080".to_string(), 30.0)]);
        assert_eq!(pb.player.duration(), 300.0);
        assert!(pb.player.can_seek());
        assert_eq!(pb.targets.len(), 2);
    }

    #[test]
    fn plex_conversion_required_falls_back_only_under_automatic() {
        let server = Arc::new(Unplayable {
            plex: true,
            ..Default::default()
        });
        let pb = open_from(server.clone(), Quality::Automatic, None)
            .unwrap()
            .playback;
        assert_eq!(pb.transcoding.as_ref().map(|t| t.id.as_str()), Some("1080"));
        assert_eq!(*lock(&server.asked), [("1080".into(), 30.0)]);
        drop(pb);
        lock(&server.asked).clear();
        assert!(open_from(server.clone(), Quality::Original, None).is_err());
        assert!(lock(&server.asked).is_empty());
    }

    #[test]
    fn under_original_a_file_that_cannot_play_fails() {
        let server = Arc::new(Unplayable::default());
        assert!(open_from(server.clone(), Quality::Original, None).is_err());
        assert!(lock(&server.asked).is_empty());
    }

    #[test]
    fn a_transcode_picked_plays_without_the_file() {
        let server = Arc::new(Unplayable::default());
        let q = Quality::Transcode("720".into());
        let pb = open_from(server.clone(), q, Some("720")).unwrap().playback;
        assert_eq!(pb.transcoding.map(|t| t.label), Some("720p".to_string()));
        assert!(pb.notice.is_none());
        assert_eq!(*lock(&server.files), 0);
    }

    #[test]
    fn an_opening_video_shows_its_title_not_its_address() {
        let from_library = OpenRequest {
            location: "https://server/heresphere/4127".into(),
            title: Some("Beach Day".into()),
            ..Default::default()
        };
        assert_eq!(from_library.shown_title(), "Beach Day");
        let from_a_feed = OpenRequest {
            location: "https://server/deovr/4127".into(),
            entry: Some(Entry::new(
                "Beach Day",
                "https://server/deovr/4127",
                EntryKind::Video,
            )),
            ..Default::default()
        };
        assert_eq!(from_a_feed.shown_title(), "Beach Day");
        let by_address = OpenRequest {
            location: "https://nas/videos/Beach_Day.mp4?token=abc".into(),
            ..Default::default()
        };
        assert_eq!(by_address.shown_title(), "Beach Day");
    }

    #[test]
    fn library_opens_keep_the_format_the_source_declared() {
        use fp_core::format::{Projection, StereoLayout};
        use fp_core::source::EntryKind;
        let lib = Library::open_in_memory().unwrap();
        let loc = "http://127.0.0.1:1/deovr/scene/7/Room_Scene_7_LR_ALPHA.json";
        let mut e = Entry::new("Room Scene", loc, EntryKind::Video);
        e.format = Some(VideoFormat::new(
            Projection::fisheye(190.0),
            StereoLayout::SideBySide,
        ));
        lib.upsert_entry("feed", &e).unwrap();
        let record = lib.get_by_location(loc).unwrap();
        let resolve = |declared: Option<VideoFormat>| {
            let hints = declared.map_or_else(ContainerHints::default, |f| ContainerHints {
                projection: Some(f.projection),
                stereo: Some(f.stereo),
                eyes_swapped: f.eyes_swapped,
            });
            format::resolve(None, hints, &last_segment(loc), 2160, 1080).format
        };

        // From the source's browser and from the library or home (no
        // entry): a masked fisheye, not a flat screen.
        for declared in [
            declared_format(Some(&e), None),
            declared_format(None, record.as_ref()),
        ] {
            let f = resolve(declared);
            assert_eq!(f.projection, Projection::fisheye(190.0));
            assert_eq!(f.stereo, StereoLayout::SideBySide);
            assert!(f.alpha_packed);
        }
        // What the name alone gives.
        assert_eq!(resolve(None).projection, Projection::Flat);

        // A format guessed from the name or shape is not a declaration.
        let local = "/v/Clip_LR.mp4";
        lib.upsert(&fp_library::MediaUpsert::new(local, "movies"))
            .unwrap();
        let r = lib.get_by_location(local).unwrap();
        assert_eq!(declared_format(None, r.as_ref()), None);
    }

    #[test]
    fn where_a_flat_screen_was_moved_is_not_saved() {
        let turned = ViewSettings {
            yaw: 30.0,
            pitch: -10.0,
            zoom: 1.2,
            ..ViewSettings::default()
        };
        let flat = kept_with_video(turned, VideoFormat::default());
        assert_eq!((flat.yaw, flat.pitch, flat.zoom), (0.0, 0.0, 1.2));
        let vr = VideoFormat {
            projection: format::Projection::EQUIRECT_180,
            ..VideoFormat::default()
        };
        assert_eq!(kept_with_video(turned, vr), turned);
    }

    #[test]
    fn opening_waits_for_closing_videos_but_not_forever() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let slow = std::thread::spawn(move || {
            let _ = rx.recv();
        });
        CLOSING.lock().unwrap().push(slow);
        assert!(!wait_closed(Duration::from_millis(50)), "still closing");
        drop(tx);
        assert!(wait_closed(Duration::from_secs(5)));
        assert!(CLOSING.lock().unwrap().is_empty());
    }

    #[test]
    fn slow_decoding_is_noticed_and_explained() {
        assert!(!falling_behind(0, 0, 600));
        assert!(!falling_behind(1, 10, 590), "one stall, few drops");
        assert!(falling_behind(2, 0, 400));
        assert!(falling_behind(0, 40, 560));
        assert!(!falling_behind(0, 0, 0));
        let stream = |codec: &str, bit_depth| StreamInfo {
            index: 0,
            kind: fp_media::StreamKind::Video,
            codec: codec.into(),
            language: None,
            title: None,
            default: true,
            forced: false,
            hearing_impaired: false,
            width: 8192,
            height: 4096,
            fps: 59.94,
            bit_depth,
            pixel_format: if bit_depth > 8 {
                "yuv420p10le".into()
            } else {
                "yuv420p".into()
            },
            interlaced: false,
            transfer: Default::default(),
            rotation: 0,
            pixel_aspect: 1.0,
            mvc: false,
            sample_rate: 0,
            channels: 0,
            ambisonic: false,
        };
        let n = slow_decode_notice(&stream("hevc", 10), true, None);
        assert!(
            n.contains("10-bit HEVC") && n.contains("8-bit 4:2:0 H.265"),
            "{n}"
        );
        let n = slow_decode_notice(&stream("av1", 8), true, None);
        assert!(n.contains("for AV1,"), "{n}");
        let busy = "hardware decoder busy: other video sessions leave room for 8192x4064, \
                    not 8192x4096 10-bit";
        let n = slow_decode_notice(&stream("hevc", 8), true, Some(busy));
        assert!(n.contains("busy"), "{n}");
        let wrapper = "V4L2 REQBUFS: Cannot allocate memory (os error 12)";
        let n = slow_decode_notice(&stream("hevc", 8), true, Some(wrapper));
        assert!(n.contains("busy"), "{n}");
        // 8-bit, but 4:2:2: the hardware can't, whatever Windows says.
        let mut s422 = stream("hevc", 8);
        s422.pixel_format = "yuv422p".into();
        let n = slow_decode_notice(&s422, true, None);
        assert!(n.contains("for 4:2:2 HEVC"), "{n}");
        let n = slow_decode_notice(&stream("hevc", 8), false, None);
        assert!(n.contains("turn on hardware decoding"), "{n}");
        let refused = "unsupported: V4L2: stream size unknown";
        let n = slow_decode_notice(&stream("hevc", 8), true, Some(refused));
        assert!(n.contains("can't play this HEVC (8192x4096)"), "{n}");

        // The stream info's reason: the error when tried, else why not.
        let r = software_reason(&stream("hevc", 10), true, None);
        assert_eq!(r, "no hardware decoding for 10-bit HEVC");
        let r = software_reason(&stream("hevc", 10), false, None);
        assert_eq!(r, "no hardware decoding for 10-bit HEVC");
        let r = software_reason(&stream("hevc", 8), false, None);
        assert_eq!(r, "hardware decoding is off in Settings");
        let r = software_reason(&stream("hevc", 10), true, Some(busy));
        assert_eq!(r, busy);
        let r = software_reason(&stream("hevc", 8), true, Some(refused));
        assert_eq!(r, "V4L2: stream size unknown");

        // 10-bit HEVC in hardware where the renderer shows it; never other
        // 10-bit codecs.
        assert!(hardware_decodes(&stream("hevc", 10), true));
        assert!(!hardware_decodes(&stream("hevc", 10), false));
        assert!(!hardware_decodes(&stream("h264", 10), true));
        assert!(!hardware_decodes(&stream("vp9", 10), true));
        let mut s12 = stream("hevc", 12);
        s12.pixel_format = "yuv420p12le".into();
        assert!(!hardware_decodes(&s12, true));
        let mut s422 = stream("hevc", 10);
        s422.pixel_format = "yuv422p10le".into();
        assert!(!hardware_decodes(&s422, true));
        assert!(hardware_decodes(&stream("h264", 8), false));
    }

    #[test]
    fn picks_become_the_servers_tracks_and_back() {
        use fp_library::{PickedSubtitles, PickedTracks};
        use fp_sources::PlaybackTrack;
        let track =
            |id: &str, index: Option<usize>, location: Option<&str>, subtitle| PlaybackTrack {
                id: id.into(),
                index,
                location: location.map(Into::into),
                subtitle,
                title: String::new(),
                codec: String::new(),
                selected: false,
            };
        let tracks = [
            track("201", Some(1), None, false),
            track("202", Some(2), None, false),
            track("301", Some(3), None, true),
            track("302", None, Some("plex://sub"), true),
        ];
        let saved =
            |audio, subtitles| saved_source_tracks(&PickedTracks { audio, subtitles }, &tracks);
        assert_eq!(saved(None, None), (None, None), "nothing picked");
        assert_eq!(
            saved(Some(2), Some(PickedSubtitles::Stream(3))),
            (Some("202".into()), Some("301".into()))
        );
        assert_eq!(
            saved(None, Some(PickedSubtitles::File("plex://sub".into()))).1,
            Some("302".into())
        );
        assert_eq!(saved(None, Some(PickedSubtitles::Off)).1, Some("0".into()));
        assert_eq!(
            saved(Some(9), Some(PickedSubtitles::Stream(9))),
            (None, None)
        );

        assert_eq!(
            picked_source_subtitles(None, &tracks),
            Some(PickedSubtitles::Off)
        );
        assert_eq!(
            picked_source_subtitles(Some("301"), &tracks),
            Some(PickedSubtitles::Stream(3))
        );
        assert_eq!(
            picked_source_subtitles(Some("302"), &tracks),
            Some(PickedSubtitles::File("plex://sub".into()))
        );
        assert_eq!(picked_source_subtitles(Some("999"), &tracks), None);
    }
}
