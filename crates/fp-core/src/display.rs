//! How the headset's display is driven: refresh rate, render resolution and
//! foveated rendering (Settings › General › Display).

use serde::{Deserialize, Serialize};

/// The refresh rates offered, in Hz.
pub const REFRESH_RATES: [u32; 3] = [72, 90, 120];

/// The render resolutions offered: pixels across each eye's picture.
pub const EYE_RESOLUTIONS: [u32; 3] = [2160, 3072, 3744];

/// How much detail the eye pictures give up away from where the eyes look.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Foveation {
    /// Every pixel shaded.
    Off,
    /// Full detail within 30° of the gaze.
    #[default]
    Low,
    /// Full detail within 18° of the gaze.
    High,
}

impl Foveation {
    pub const ALL: [Foveation; 3] = [Foveation::Off, Foveation::Low, Foveation::High];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Display {
    /// One of [`REFRESH_RATES`].
    pub refresh_rate: u32,
    /// One of [`EYE_RESOLUTIONS`].
    pub eye_resolution: u32,
    pub foveation: Foveation,
}

/// 120 Hz gives 24, 30 and 60 fps video even frame timing; 3744 shimmers
/// least; low foveation keeps 8K 10-bit within a 120 Hz frame at 3744.
impl Default for Display {
    fn default() -> Self {
        Display {
            refresh_rate: 120,
            eye_resolution: 3744,
            foveation: Foveation::Low,
        }
    }
}

impl Display {
    /// With a value that isn't offered (a hand-edited file), its default.
    pub fn offered(self) -> Display {
        let d = Display::default();
        Display {
            refresh_rate: if REFRESH_RATES.contains(&self.refresh_rate) {
                self.refresh_rate
            } else {
                d.refresh_rate
            },
            eye_resolution: if EYE_RESOLUTIONS.contains(&self.eye_resolution) {
                self.eye_resolution
            } else {
                d.eye_resolution
            },
            foveation: self.foveation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_not_offered_become_the_defaults() {
        let odd = Display {
            refresh_rate: 144,
            eye_resolution: 4096,
            foveation: Foveation::High,
        };
        assert_eq!(
            odd.offered(),
            Display {
                foveation: Foveation::High,
                ..Display::default()
            }
        );
        let fine = Display {
            refresh_rate: 72,
            eye_resolution: 2160,
            foveation: Foveation::Off,
        };
        assert_eq!(fine.offered(), fine);
    }

    #[test]
    fn a_file_without_display_settings_gets_the_defaults() {
        let d: Display = serde_json::from_str("{}").unwrap();
        assert_eq!(d, Display::default());
        let d: Display = serde_json::from_str(r#"{"foveation":"high"}"#).unwrap();
        assert_eq!(d.foveation, Foveation::High);
    }
}
