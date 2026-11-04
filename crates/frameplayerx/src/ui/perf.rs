//! The performance overlay (Settings › General › Diagnostics): why a video
//! might be stuttering, on a panel you place with a grip.

use super::theme::{self, Weight};
use crate::perf::Sample;
use egui::{Color32, Rect};

const WIDTH_PTS: f32 = 640.0;
const ROW_PTS: f32 = 34.0;
const MARGIN_PTS: egui::Vec2 = egui::vec2(16.0, 12.0);
const COLUMN_GAP_PTS: f32 = 16.0;
const MOST_ROWS: u32 = 9;

/// The panel's size in points: room for its most rows.
pub const PANEL_PTS: [u32; 2] = [
    WIDTH_PTS as u32,
    (MOST_ROWS as f32 * ROW_PTS + 2.0 * MARGIN_PTS.y) as u32,
];

/// What the overlay shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overlay {
    /// Frames drawn per second, and the display's refresh rate.
    pub drawn: Option<f32>,
    pub display_hz: Option<f32>,
    /// The typical frame time, ms.
    pub frame_ms: Option<f32>,
    pub video: Option<Video>,
    pub sample: Option<Sample>,
}

/// The open video.
#[derive(Clone, Debug, PartialEq)]
pub struct Video {
    pub state: VideoState,
    /// The film's own frame rate.
    pub film_fps: f32,
    /// Since it opened.
    pub dropped: u64,
    pub stalls: u64,
    /// Either in the last few seconds.
    pub stuttered: bool,
    /// "HW · HEVC 10-bit".
    pub decoder: String,
    /// Seconds read ahead of the picture, and how far it's aiming.
    pub buffer: (f32, f32),
    /// The file has been read to its end, so the buffer can only fall.
    pub read_to_end: bool,
    pub network: Option<Network>,
    pub last_jump: Option<LastJump>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VideoState {
    /// Frames shown per second, once known.
    Playing(Option<f32>),
    Paused,
    Buffering,
    Ended,
}

/// A video read over the network.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Network {
    /// Megabytes received per second, once known.
    pub mb_s: Option<f32>,
    /// Nothing arriving because nothing more is wanted for now.
    pub idle: bool,
    pub retries: u64,
    /// One in the last few seconds.
    pub retried: bool,
}

/// What the last open or skip took.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LastJump {
    pub skip: bool,
    pub picture_s: f32,
    pub first_read_s: Option<f32>,
}

const NONE: &str = "–";

fn known(v: Option<f32>, digits: usize) -> String {
    v.map_or(NONE.into(), |v| format!("{v:.digits$}"))
}

/// Label, value, and whether the value is a warning sign.
pub fn rows(o: &Overlay) -> Vec<(&'static str, String, bool)> {
    let mut rows = Vec::new();
    let mut display = match (o.drawn, o.display_hz) {
        (Some(drawn), Some(hz)) => {
            let (drawn, hz) = (format!("{drawn:.0}"), format!("{hz:.0}"));
            tr!("perf-display-value", drawn = drawn, hz = hz)
        }
        _ => NONE.to_string(),
    };
    if let Some(ms) = o.frame_ms {
        display += &format!(" · {ms:.1} ms");
    }
    let missing_frames = match (o.drawn, o.display_hz) {
        (Some(drawn), Some(hz)) => drawn < hz * 0.95,
        _ => false,
    };
    rows.push((tr!("perf-display"), display, missing_frames));
    if let Some(v) = &o.video {
        video_rows(v, &mut rows);
    }
    let s = o.sample.unwrap_or_default();
    let temp = |t: Option<f32>| t.map(|t| format!("{t:.0}°C"));
    let join = |parts: Vec<Option<String>>| {
        let parts: Vec<String> = parts.into_iter().flatten().collect();
        if parts.is_empty() {
            NONE.to_string()
        } else {
            parts.join(" · ")
        }
    };
    rows.push((
        "CPU",
        join(vec![s.cpu.map(|c| format!("{c:.0}%")), temp(s.cpu_temp)]),
        s.cpu_held,
    ));
    rows.push((
        "GPU",
        join(vec![
            s.gpu.map(|g| format!("{g:.0}%")),
            s.gpu_mhz.map(|m| format!("{m} MHz")),
            temp(s.gpu_temp),
        ]),
        s.gpu_held,
    ));
    let gb = |bytes: u64| format!("{:.1}", bytes as f64 / (1u64 << 30) as f64);
    rows.push((
        tr!("perf-memory"),
        s.mem.map_or(NONE.to_string(), |(used, total)| {
            let (used, total) = (gb(used), gb(total));
            tr!("perf-memory-value", used = used, total = total)
        }),
        false,
    ));
    rows
}

fn video_rows(v: &Video, rows: &mut Vec<(&'static str, String, bool)>) {
    let state = match v.state {
        VideoState::Playing(fps) => {
            let (fps, film) = (known(fps, 1), format!("{:.1}", v.film_fps));
            tr!("perf-video-value", fps = fps, film = film)
        }
        VideoState::Paused => tr!("perf-video-paused").to_string(),
        VideoState::Buffering => tr!("perf-video-buffering").to_string(),
        VideoState::Ended => tr!("perf-video-ended").to_string(),
    };
    let tally = tr!("perf-stutter", dropped = v.dropped, stalls = v.stalls);
    rows.push((tr!("perf-video"), format!("{state} · {tally}"), v.stuttered));
    rows.push((tr!("adjust-decoder"), v.decoder.clone(), false));

    let (ahead, target) = v.buffer;
    let (ahead_text, target_text) = (format!("{ahead:.1}"), format!("{target:.0}"));
    rows.push((
        tr!("perf-buffer"),
        tr!(
            "perf-buffer-value",
            ahead = ahead_text,
            target = target_text
        ),
        ahead < 1.0 && !v.read_to_end,
    ));

    if let Some(n) = &v.network {
        let mut value = if n.idle {
            tr!("perf-network-idle").to_string()
        } else {
            n.mb_s
                .map_or(NONE.to_string(), |mb_s| format!("{mb_s:.1} MB/s"))
        };
        if n.retries > 0 {
            value += " · ";
            value += &tr!("perf-retries", count = n.retries);
        }
        rows.push((tr!("perf-network"), value, n.retried));
    }

    match &v.last_jump {
        Some(j) => {
            let mut value = format!("{:.1} s", j.picture_s);
            if let Some(read) = j.first_read_s.filter(|_| j.skip) {
                let secs = format!("{read:.1}");
                value += " · ";
                value += &tr!("perf-first-read", secs = secs);
            }
            let label = if j.skip {
                tr!("perf-last-skip")
            } else {
                tr!("perf-opened")
            };
            rows.push((label, value, false));
        }
        None => rows.push((tr!("perf-opened"), NONE.to_string(), false)),
    }
}

/// Paints the rows and returns the card they sit on: fixed columns, so numbers
/// don't shift sideways as they change.
pub fn overlay(ctx: &egui::Context, o: &Overlay) -> Rect {
    let rows = rows(o);
    let screen = ctx.screen_rect();
    let card = Rect::from_min_size(
        screen.min,
        egui::vec2(
            screen.width(),
            rows.len() as f32 * ROW_PTS + 2.0 * MARGIN_PTS.y,
        ),
    );
    let painter = ctx.layer_painter(egui::LayerId::background());
    painter.rect_filled(card, 12.0, Color32::from_black_alpha(200));
    let label_font = theme::font(Weight::SemiBold, 24.0);
    let value_font = theme::font(Weight::Regular, 24.0);
    let label_width = rows
        .iter()
        .map(|(label, _, _)| {
            ctx.fonts(|f| {
                f.layout_no_wrap(label.to_string(), label_font.clone(), theme::TEXT_2)
                    .size()
                    .x
            })
        })
        .fold(0.0, f32::max);
    let left = card.min.x + MARGIN_PTS.x;
    for (i, (label, value, warn)) in rows.iter().enumerate() {
        let y = card.min.y + MARGIN_PTS.y + (i as f32 + 0.5) * ROW_PTS;
        painter.text(
            egui::pos2(left, y),
            egui::Align2::LEFT_CENTER,
            label,
            label_font.clone(),
            theme::TEXT_2,
        );
        painter.text(
            egui::pos2(left + label_width + COLUMN_GAP_PTS, y),
            egui::Align2::LEFT_CENTER,
            value,
            value_font.clone(),
            if *warn { theme::WARN } else { theme::TEXT },
        );
    }
    card
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video() -> Video {
        Video {
            state: VideoState::Playing(Some(23.96)),
            film_fps: 23.976,
            dropped: 3,
            stalls: 1,
            stuttered: false,
            decoder: "HW · HEVC 10-bit".into(),
            buffer: (9.54, 15.0),
            read_to_end: false,
            network: Some(Network {
                mb_s: Some(6.21),
                idle: false,
                retries: 0,
                retried: false,
            }),
            last_jump: Some(LastJump {
                skip: true,
                picture_s: 4.23,
                first_read_s: Some(1.72),
            }),
        }
    }

    fn overlay_with(video: Option<Video>) -> Overlay {
        Overlay {
            drawn: Some(90.0),
            display_hz: Some(90.0),
            frame_ms: Some(6.12),
            video,
            sample: Some(Sample {
                cpu: Some(34.4),
                cpu_temp: Some(61.2),
                cpu_held: false,
                mem: Some((5 << 30, 15 << 30)),
                gpu: Some(41.0),
                gpu_mhz: Some(680),
                gpu_temp: Some(58.4),
                gpu_held: false,
            }),
        }
    }

    fn value<'a>(
        rows: &'a [(&'static str, String, bool)],
        label: &str,
    ) -> &'a (&'static str, String, bool) {
        rows.iter().find(|r| r.0 == label).expect(label)
    }

    #[test]
    fn rows_say_what_is_known() {
        let r = rows(&overlay_with(Some(video())));
        let texts: Vec<(&str, &str)> = r.iter().map(|(l, v, _)| (*l, v.as_str())).collect();
        assert_eq!(
            texts,
            [
                ("Display", "90 of 90 Hz · 6.1 ms"),
                ("Video", "24.0 of 24.0 fps · 3 dropped · 1 stall"),
                ("Decoder", "HW · HEVC 10-bit"),
                ("Buffer", "9.5 of 15 s"),
                ("Network", "6.2 MB/s"),
                ("Last skip", "4.2 s · first read 1.7 s"),
                ("CPU", "34% · 61°C"),
                ("GPU", "41% · 680 MHz · 58°C"),
                ("Memory", "5.0 of 15.0 GB"),
            ]
        );
        assert!(r.iter().all(|(_, _, warn)| !warn), "{r:?}");
        assert!(r.len() as u32 <= MOST_ROWS);
    }

    #[test]
    fn nothing_known_is_a_dash_rather_than_a_made_up_zero() {
        let r = rows(&Overlay::default());
        assert_eq!(r.len(), 4, "no video, so no video rows");
        assert!(r.iter().all(|(_, v, warn)| v == "–" && !warn), "{r:?}");

        let mut v = video();
        v.state = VideoState::Playing(None);
        v.network = Some(Network {
            mb_s: None,
            idle: false,
            retries: 0,
            retried: false,
        });
        v.last_jump = None;
        let r = rows(&overlay_with(Some(v)));
        assert!(value(&r, "Video").1.starts_with("– of 24.0 fps"));
        assert_eq!(value(&r, "Network").1, "–");
        assert_eq!(value(&r, "Opened").1, "–");
    }

    #[test]
    fn a_stopped_video_says_so_instead_of_0_fps() {
        for (state, says) in [
            (VideoState::Paused, "Paused"),
            (VideoState::Buffering, "Buffering"),
            (VideoState::Ended, "Ended"),
        ] {
            let mut v = video();
            v.state = state;
            let r = rows(&overlay_with(Some(v)));
            assert!(value(&r, "Video").1.starts_with(says), "{r:?}");
        }
    }

    #[test]
    fn opening_and_a_video_not_on_the_network_leave_out_what_they_lack() {
        let mut v = video();
        v.network = None;
        v.last_jump = Some(LastJump {
            skip: false,
            picture_s: 3.44,
            first_read_s: None,
        });
        let r = rows(&overlay_with(Some(v)));
        assert!(r.iter().all(|(l, _, _)| *l != "Network"));
        assert_eq!(value(&r, "Opened").1, "3.4 s");
    }

    #[test]
    fn amber_marks_trouble() {
        let mut o = overlay_with(Some(video()));
        o.drawn = Some(80.0);
        assert!(value(&rows(&o), "Display").2, "under 95% of 90 Hz");
        o.drawn = Some(86.0);
        assert!(!value(&rows(&o), "Display").2);

        let mut v = video();
        v.stuttered = true;
        v.buffer = (0.9, 15.0);
        v.network = Some(Network {
            mb_s: Some(0.0),
            idle: false,
            retries: 2,
            retried: true,
        });
        let r = rows(&overlay_with(Some(v.clone())));
        assert!(value(&r, "Video").2);
        assert!(value(&r, "Buffer").2);
        let network = value(&r, "Network");
        assert_eq!(network.1, "0.0 MB/s · 2 retries");
        assert!(network.2);

        v.read_to_end = true;
        let r = rows(&overlay_with(Some(v)));
        assert!(!value(&r, "Buffer").2, "the film is ending");

        let mut o = overlay_with(None);
        let s = o.sample.as_mut().unwrap();
        s.cpu_held = true;
        s.gpu_held = true;
        let r = rows(&o);
        assert!(
            value(&r, "CPU").2 && value(&r, "GPU").2,
            "held back for heat"
        );
    }

    #[test]
    fn a_full_buffer_makes_the_network_idle() {
        let mut v = video();
        v.network = Some(Network {
            mb_s: Some(0.0),
            idle: true,
            retries: 0,
            retried: false,
        });
        let r = rows(&overlay_with(Some(v)));
        let network = value(&r, "Network");
        assert_eq!(network.1, "Idle");
        assert!(!network.2);
    }
}
