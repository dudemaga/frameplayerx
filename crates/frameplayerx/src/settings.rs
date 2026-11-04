//! Persistent user settings (`~/.config/frameplayerx/settings.json`).
//! Source credentials live separately in `sources.json` (mode 0600).

use fp_core::view::ViewSettings;
use fp_haptics::HapticsSettings;
use fp_remote::config::RemoteConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A haptic device the user added.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HapticDeviceConfig {
    /// TCode over serial (`/dev/ttyACM0`), `tcp://host:port` or `udp://host:port`.
    Tcode { endpoint: String },
    /// Intiface Central's server.
    Buttplug { url: String },
    /// The Handy, by connection key.
    Handy { key: String },
}

impl HapticDeviceConfig {
    pub fn label(&self) -> String {
        match self {
            HapticDeviceConfig::Tcode { endpoint } => format!("TCode {endpoint}"),
            HapticDeviceConfig::Buttplug { url } => format!("Intiface {url}"),
            HapticDeviceConfig::Handy { key } => {
                let shown: String = key.chars().take(3).collect();
                format!("The Handy ({shown}…)")
            }
        }
    }
}

/// Which subtitles a video starts with (see [`crate::subtitles::choose`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleMode {
    Off,
    /// Forced subtitles, subtitles in your language when the audio is in
    /// another one, and a subtitle file named like the video.
    #[default]
    Automatic,
    /// Subtitles in your language whenever the video has them.
    Always,
}

/// Settings › Interface size: the sizes offered.
pub const UI_SCALES: std::ops::RangeInclusive<f32> = 0.8..=1.5;

/// Settings › Image cache: the sizes offered, in MB.
pub const IMAGE_CACHE_SIZES: [u32; 5] = [100, 250, 500, 1000, 2000];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Use the V4L2 hardware decoder when available.
    pub hardware_decoding: bool,
    /// ALSA device ("default" routes to PipeWire on SteamOS).
    pub audio_device: String,
    pub volume: f32,
    pub subtitles: SubtitleMode,
    /// The language subtitles are wanted in (ISO 639-1).
    pub subtitle_language: String,
    /// The interface's language, one of [`crate::i18n::LANGUAGES`]; none
    /// follows Steam's.
    pub language: Option<String>,
    /// Defaults for videos without saved adjustments.
    pub default_view: ViewSettings,
    /// Show the real world around flat screens and the UI.
    pub passthrough: bool,
    /// Resume videos where they were left.
    pub resume: bool,
    /// The format a video starts in when nothing identifies it (no
    /// metadata, file name tag or telling shape).
    pub default_format: fp_core::format::VideoFormat,
    /// Seconds of inactivity before the playback UI hides.
    pub auto_hide_secs: f32,
    /// Thumbstick seek step in seconds.
    pub seek_step: f64,
    pub remote: RemoteConfig,
    /// Haptic devices and scripts. Off, the app shows nothing about them,
    /// loads no scripts and runs no haptics engine.
    pub haptics_enabled: bool,
    pub haptics: HapticsSettings,
    pub haptic_devices: Vec<HapticDeviceConfig>,
    pub update_channel: fp_updater::Channel,
    pub check_updates: bool,
    /// UI text size multiplier, in [`UI_SCALES`].
    pub ui_scale: f32,
    /// Room for server pictures kept on the headset, in MB
    /// ([`crate::image_cache`]).
    pub image_cache_mb: u32,
    /// Make thumbnails for videos whose server offers no picture.
    pub make_thumbnails: bool,
    /// A partly watched video shows the frame where it was left off.
    pub resume_frames: bool,
    /// Flat videos and the panels follow the head ([`crate::world::follow`]).
    pub follow: bool,
    /// The video playing starts again when it ends.
    pub loop_video: bool,
    /// The list a video was opened from plays through, the next starting
    /// when one ends (Play all, a playlist and what was queued always do).
    pub autoplay: bool,
    /// Up next starts again from the first video after the last, and X and
    /// Y step round it.
    pub loop_playlist: bool,
    /// The performance overlay (Settings › General › Diagnostics).
    pub perf_overlay: bool,
    /// What the controller buttons and thumbsticks do.
    pub controls: crate::bindings::Bindings,
    /// Where the library and adjust panels were moved to: yaw and pitch in
    /// degrees from their usual places (left and up positive).
    pub panel_offsets: PanelOffsets,
    /// How folders are ordered when browsing a source.
    pub browse_sort: fp_sources::EntrySort,
    /// Sources locked with a PIN: source id to the PIN's hash (see
    /// [`crate::pin`]). Left out of the file when empty.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub source_pins: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelOffsets {
    pub main: [f32; 2],
    /// The adjust panel, from beside the control bar.
    pub adjust: [f32; 2],
    /// The control bar, from below a flat screen (or in front of you
    /// during a 180°/360° video).
    pub bar: [f32; 2],
    /// The performance overlay, from where it starts.
    pub perf: [f32; 2],
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            hardware_decoding: true,
            audio_device: "default".into(),
            volume: 1.0,
            subtitles: SubtitleMode::default(),
            subtitle_language: crate::i18n::system_language()
                .and_then(|l| fp_core::language::from_locale(&l))
                .unwrap_or("en")
                .to_string(),
            language: None,
            default_view: ViewSettings::default(),
            passthrough: false,
            resume: true,
            default_format: fp_core::format::VideoFormat::FALLBACK,
            auto_hide_secs: 4.0,
            seek_step: 10.0,
            remote: RemoteConfig::default(),
            haptics_enabled: false,
            haptics: HapticsSettings::default(),
            haptic_devices: Vec::new(),
            // A beta build keeps getting betas until the user picks Stable.
            update_channel: if env!("CARGO_PKG_VERSION").contains('-') {
                fp_updater::Channel::Beta
            } else {
                fp_updater::Channel::Stable
            },
            check_updates: true,
            ui_scale: 1.0,
            image_cache_mb: 500,
            make_thumbnails: true,
            resume_frames: true,
            follow: false,
            loop_video: false,
            autoplay: false,
            loop_playlist: false,
            perf_overlay: false,
            controls: crate::bindings::Bindings::default(),
            panel_offsets: PanelOffsets::default(),
            browse_sort: fp_sources::EntrySort::default(),
            source_pins: Default::default(),
        }
    }
}

impl Settings {
    /// Adjustments for a video that has none saved: the defaults, following
    /// the global chroma key.
    pub fn new_video_view(&self) -> ViewSettings {
        ViewSettings {
            key_own: Some(false),
            ..self.default_view
        }
    }

    pub fn path() -> PathBuf {
        fp_core::dirs::config_dir().join("settings.json")
    }

    pub fn sources_path() -> PathBuf {
        fp_core::dirs::config_dir().join("sources.json")
    }

    /// The language the interface is shown in: the one picked, else
    /// [`crate::i18n::automatic`].
    pub fn interface_language(&self) -> String {
        self.language
            .clone()
            .unwrap_or_else(|| crate::i18n::automatic().to_string())
    }

    /// Loads settings, falling back to defaults (and logging why) when the
    /// file is missing or unreadable.
    pub fn load(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Settings>(&text) {
                Ok(mut s) => {
                    // Saved before the global chroma key existed: its key
                    // was never chosen, so take the current default.
                    if s.default_view.key_own.is_none() {
                        s.default_view.set_key(&ViewSettings::default());
                        s.default_view.key_own = Some(false);
                    }
                    // Saved before height existed: the right grip's up / down
                    // was free then, so it gets the new default.
                    let before_height = serde_json::from_str::<serde_json::Value>(&text)
                        .ok()
                        .is_some_and(|v| v["default_view"].get("height").is_none());
                    if before_height
                        && s.controls.right.grip_y == crate::bindings::AxisAction::Nothing
                    {
                        s.controls.right.grip_y = crate::bindings::AxisAction::Height;
                    }
                    s
                }
                Err(e) => {
                    log::warn!("settings {}: {e}; using defaults", path.display());
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        }
    }

    /// Writes atomically (temp file + rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_partial_files() {
        let dir = std::env::temp_dir().join(format!("fp-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        let mut s = Settings {
            volume: 0.5,
            ..Default::default()
        };
        s.haptic_devices.push(HapticDeviceConfig::Tcode {
            endpoint: "tcp://10.0.0.2:8000".into(),
        });
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        // Source PINs are only written once one is set.
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("source_pins")
        );
        s.source_pins.insert("nas".into(), crate::pin::hash("2468"));
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        std::fs::write(&path, r#"{"volume": 0.25}"#).unwrap();
        let p = Settings::load(&path);
        assert_eq!(p.volume, 0.25);
        assert_eq!(p.seek_step, 10.0);
        assert!(!p.haptics_enabled, "haptics stay off until turned on");
        assert_eq!(p.default_format, fp_core::format::VideoFormat::FALLBACK);
        // Saved before the global chroma key: the old green default goes.
        std::fs::write(
            &path,
            r#"{"default_view": {"zoom": 1.5, "key_color": [0.0, 1.0, 0.0]}}"#,
        )
        .unwrap();
        let p = Settings::load(&path);
        assert_eq!(p.default_view.zoom, 1.5);
        assert_eq!(p.default_view.key_color, ViewSettings::default().key_color);
        assert_eq!(p.default_view.key_own, Some(false));
        // Saved before height: the right grip's free up / down gets it; one
        // set to nothing since stays so.
        let old =
            r#"{"default_view": {"zoom": 1.0}, "controls": {"right": {"grip_y": "nothing"}}}"#;
        std::fs::write(&path, old).unwrap();
        let height = crate::bindings::AxisAction::Height;
        assert_eq!(Settings::load(&path).controls.right.grip_y, height);
        let mut s = Settings::default();
        s.controls.right.grip_y = crate::bindings::AxisAction::Nothing;
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(Settings::load(&path).volume, 1.0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
