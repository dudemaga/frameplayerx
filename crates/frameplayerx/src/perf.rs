//! The performance overlay's numbers. The system's come from /proc and /sys
//! once a second, on a thread of their own, while the overlay shows. The rest
//! are noted twice a second from the render loop: rates come from those notes,
//! so repainting the overlay can't change a reading.

use fp_core::sync::lock;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_secs(1);
const NOTE_EVERY: Duration = Duration::from_millis(500);
/// Long enough to average over a slow scene's uneven frames and to hold a
/// rate steady, short enough to show a stall as it happens.
const RATE_WINDOW: Duration = Duration::from_secs(2);
/// A low-bitrate film gets a block every few seconds.
const NETWORK_WINDOW: Duration = Duration::from_secs(3);
/// How long a drop, a stall or a retry stays amber.
const TROUBLE_WINDOW: Duration = Duration::from_secs(10);

/// One reading, over the second before it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    /// The whole device's CPU, percent of all cores.
    pub cpu: Option<f32>,
    /// Its hottest sensor, °C.
    pub cpu_temp: Option<f32>,
    /// The device is slowing it down for heat.
    pub cpu_held: bool,
    /// Memory in use (total less available) and the total, bytes.
    pub mem: Option<(u64, u64)>,
    /// Share of the time the GPU spent on FramePlayerX's work, percent;
    /// None where the driver doesn't report it.
    pub gpu: Option<f32>,
    /// The GPU's clock, MHz.
    pub gpu_mhz: Option<u32>,
    /// Its hottest sensor, °C.
    pub gpu_temp: Option<f32>,
    /// The device is slowing it down for heat.
    pub gpu_held: bool,
}

/// Samples while alive; the thread stops at its next reading once this is
/// dropped.
pub struct Monitor {
    latest: Arc<Mutex<Option<Sample>>>,
    stop: Arc<AtomicBool>,
}

impl Monitor {
    pub fn start() -> Monitor {
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (out, quit) = (latest.clone(), stop.clone());
        let spawned = std::thread::Builder::new()
            .name("fp-perf".into())
            .spawn(move || {
                let heat = HeatFiles::find();
                let mut prev = Counters::read(&heat);
                while !quit.load(Ordering::Relaxed) {
                    std::thread::sleep(INTERVAL);
                    let now = Counters::read(&heat);
                    *lock(&out) = Some(now.since(&prev));
                    prev = now;
                }
            });
        if let Err(e) = spawned {
            log::warn!("performance sampler: {e}");
        }
        Monitor { latest, stop }
    }

    /// The newest reading (none until the first second has passed).
    pub fn latest(&self) -> Option<Sample> {
        *lock(&self.latest)
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A running total noted now and then, read back over a window.
struct Totals {
    span: Duration,
    notes: VecDeque<(Instant, u64)>,
}

impl Totals {
    fn new(span: Duration) -> Totals {
        Totals {
            span,
            notes: VecDeque::new(),
        }
    }

    fn note(&mut self, at: Instant, total: u64) {
        self.notes.push_back((at, total));
        while self
            .notes
            .get(1)
            .is_some_and(|n| at.duration_since(n.0) >= self.span)
        {
            self.notes.pop_front();
        }
    }

    fn clear(&mut self) {
        self.notes.clear();
    }

    /// Per second over the window, once it covers at least half of it.
    fn rate(&self) -> Option<f32> {
        let (first, last) = (self.notes.front()?, self.notes.back()?);
        let took = last.0.duration_since(first.0);
        (took >= self.span / 2).then(|| last.1.saturating_sub(first.1) as f32 / took.as_secs_f32())
    }

    fn rose(&self) -> bool {
        match (self.notes.front(), self.notes.back()) {
            (Some(first), Some(last)) => last.1 > first.1,
            _ => false,
        }
    }
}

/// What a note takes from the open video.
#[derive(Clone, Copy, Debug)]
pub struct VideoNote {
    /// Changes with every skip.
    pub serial: u64,
    pub playing: bool,
    pub shown: u64,
    /// Frames dropped and stalls.
    pub stutter: u64,
    /// Bytes received and retries, for a video read over the network.
    pub net: Option<(u64, u64)>,
}

/// The overlay's rates, from its notes.
pub struct Readings {
    last_note: Option<Instant>,
    notes: u64,
    frames: u64,
    frame_ms: VecDeque<f32>,
    drawn: Totals,
    shown: Totals,
    stutter: Totals,
    bytes: Totals,
    retries: Totals,
    serial: Option<u64>,
}

impl Default for Readings {
    fn default() -> Readings {
        Readings {
            last_note: None,
            notes: 0,
            frames: 0,
            frame_ms: VecDeque::new(),
            drawn: Totals::new(RATE_WINDOW),
            shown: Totals::new(RATE_WINDOW),
            stutter: Totals::new(TROUBLE_WINDOW),
            bytes: Totals::new(NETWORK_WINDOW),
            retries: Totals::new(TROUBLE_WINDOW),
            serial: None,
        }
    }
}

impl Readings {
    /// Counts a frame drawn, keeping about a second of frame times.
    pub fn frame(&mut self, ms: Option<f32>, display_hz: Option<f32>) {
        self.frames += 1;
        if let Some(ms) = ms {
            self.frame_ms.push_back(ms);
            let second = display_hz.map_or(90, |hz| hz.round().max(1.0) as usize);
            while self.frame_ms.len() > second {
                self.frame_ms.pop_front();
            }
        }
    }

    pub fn note_due(&self, at: Instant) -> bool {
        self.last_note
            .is_none_or(|t| at.duration_since(t) >= NOTE_EVERY)
    }

    /// Notes the counters; returns whether it's time to show new readings.
    pub fn note(&mut self, at: Instant, video: Option<VideoNote>) -> bool {
        self.last_note = Some(at);
        self.drawn.note(at, self.frames);
        match video {
            Some(v) => {
                if self.serial != Some(v.serial) {
                    self.serial = Some(v.serial);
                    self.shown.clear();
                    self.bytes.clear();
                }
                if v.playing {
                    self.shown.note(at, v.shown);
                } else {
                    self.shown.clear();
                }
                self.stutter.note(at, v.stutter);
                if let Some((bytes, retries)) = v.net {
                    self.bytes.note(at, bytes);
                    self.retries.note(at, retries);
                }
            }
            None => self.new_video(),
        }
        self.notes += 1;
        self.notes % 2 == 1
    }

    /// Another video's counters start again from nothing.
    pub fn new_video(&mut self) {
        self.serial = None;
        self.shown.clear();
        self.stutter.clear();
        self.bytes.clear();
        self.retries.clear();
    }

    /// Frames drawn per second.
    pub fn drawn(&self) -> Option<f32> {
        self.drawn.rate()
    }

    /// The typical frame time, ms.
    pub fn frame_ms(&self) -> Option<f32> {
        let mut t: Vec<f32> = self.frame_ms.iter().copied().collect();
        t.sort_by(f32::total_cmp);
        t.get(t.len() / 2).copied()
    }

    /// Video frames shown per second.
    pub fn video_fps(&self) -> Option<f32> {
        self.shown.rate()
    }

    /// Frames dropped or a stall in the last few seconds.
    pub fn stuttered(&self) -> bool {
        self.stutter.rose()
    }

    /// Megabytes received per second.
    pub fn mb_per_s(&self) -> Option<f32> {
        self.bytes.rate().map(|b| b / 1e6)
    }

    /// A retry in the last few seconds.
    pub fn retried(&self) -> bool {
        self.retries.rose()
    }

    /// Retries since the video opened.
    pub fn retries(&self) -> u64 {
        self.retries.notes.back().map_or(0, |n| n.1)
    }
}

/// Which chip a sensor or cooling device belongs to.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Chip {
    Cpu,
    Gpu,
}

/// By a thermal zone's name, as on a Steam Frame: `gpuss-0-thermal`,
/// `cpu7-top-thermal`, `cpuss0-thermal`.
fn zone_chip(name: &str) -> Option<Chip> {
    if name.starts_with("gpu") {
        Some(Chip::Gpu)
    } else if name.starts_with("cpu") {
        Some(Chip::Cpu)
    } else {
        None
    }
}

/// By a cooling device's name, as on a Steam Frame: `devfreq-3d00000.gpu`,
/// `cpufreq-cpu0`.
fn cooling_chip(name: &str) -> Option<Chip> {
    if name.starts_with("cpufreq") {
        Some(Chip::Cpu)
    } else if name.starts_with("devfreq") && name.contains("gpu") {
        Some(Chip::Gpu)
    } else {
        None
    }
}

/// The files that say how hot each chip is and whether it's held back, found
/// once: there are dozens, and they don't change.
#[derive(Default)]
struct HeatFiles {
    zones: Vec<(Chip, PathBuf)>,
    cooling: Vec<(Chip, PathBuf)>,
}

impl HeatFiles {
    fn find() -> HeatFiles {
        let mut found = HeatFiles::default();
        let Ok(dir) = std::fs::read_dir("/sys/class/thermal") else {
            return found;
        };
        for entry in dir.flatten() {
            let path = entry.path();
            let Ok(kind) = std::fs::read_to_string(path.join("type")) else {
                continue;
            };
            let kind = kind.trim();
            if let Some(chip) = zone_chip(kind) {
                found.zones.push((chip, path.join("temp")));
            }
            if let Some(chip) = cooling_chip(kind) {
                found.cooling.push((chip, path.join("cur_state")));
            }
        }
        found
    }

    /// The chip's hottest sensor, °C, and whether it's being slowed for heat.
    fn read(&self, chip: Chip) -> (Option<f32>, bool) {
        let read = |p: &PathBuf| std::fs::read_to_string(p).ok();
        let hottest = self
            .zones
            .iter()
            .filter(|z| z.0 == chip)
            .filter_map(|z| read(&z.1).as_deref().and_then(millidegrees))
            .fold(None, |max: Option<f32>, t| {
                Some(max.map_or(t, |m| m.max(t)))
            });
        let held = self
            .cooling
            .iter()
            .filter(|c| c.0 == chip)
            .filter_map(|c| read(&c.1))
            .any(|state| state.trim().parse::<u32>().is_ok_and(|s| s > 0));
        (hottest, held)
    }
}

/// A thermal zone's `temp`, °C.
fn millidegrees(text: &str) -> Option<f32> {
    Some(text.trim().parse::<f32>().ok()? / 1000.0)
}

/// Cumulative counters at one moment; two give a [`Sample`].
struct Counters {
    at: Instant,
    /// Busy and total CPU time of all cores, in ticks.
    cpu: Option<(u64, u64)>,
    gpu_ns: Option<u64>,
    mem: Option<(u64, u64)>,
    gpu_mhz: Option<u32>,
    cpu_heat: (Option<f32>, bool),
    gpu_heat: (Option<f32>, bool),
}

impl Counters {
    fn read(heat: &HeatFiles) -> Counters {
        let file = |p: &str| std::fs::read_to_string(p).ok();
        Counters {
            at: Instant::now(),
            cpu: file("/proc/stat").as_deref().and_then(cpu_times),
            gpu_ns: gpu_time(),
            mem: file("/proc/meminfo").as_deref().and_then(meminfo),
            gpu_mhz: gpu_clock(),
            cpu_heat: heat.read(Chip::Cpu),
            gpu_heat: heat.read(Chip::Gpu),
        }
    }

    fn since(&self, prev: &Counters) -> Sample {
        let secs = self.at.duration_since(prev.at).as_secs_f64().max(1e-3);
        let pct = |part: f64, whole: f64| (whole > 0.0).then(|| (100.0 * part / whole) as f32);
        let cpu = match (self.cpu, prev.cpu) {
            (Some((b, t)), Some((pb, pt))) => {
                pct(b.saturating_sub(pb) as f64, t.saturating_sub(pt) as f64)
            }
            _ => None,
        };
        let gpu = match (self.gpu_ns, prev.gpu_ns) {
            (Some(g), Some(pg)) => {
                pct(g.saturating_sub(pg) as f64, secs * 1e9).map(|p| p.min(100.0))
            }
            _ => None,
        };
        Sample {
            cpu,
            cpu_temp: self.cpu_heat.0,
            cpu_held: self.cpu_heat.1,
            mem: self
                .mem
                .map(|(total, available)| (total.saturating_sub(available), total)),
            gpu,
            gpu_mhz: self.gpu_mhz,
            gpu_temp: self.gpu_heat.0,
            gpu_held: self.gpu_heat.1,
        }
    }
}

/// From /proc/stat: busy and total time of all cores (ticks). Waiting for I/O
/// counts as idle; guest time is already in user time.
fn cpu_times(stat: &str) -> Option<(u64, u64)> {
    let first = stat.lines().next()?.strip_prefix("cpu ")?;
    let v: Vec<u64> = first
        .split_whitespace()
        .take(8)
        .map(|f| f.parse().ok())
        .collect::<Option<_>>()?;
    let (idle, iowait) = (*v.get(3)?, *v.get(4)?);
    let total: u64 = v.iter().sum();
    Some((total - idle - iowait, total))
}

/// A `Key:   123 kB` value from /proc/meminfo, bytes.
fn kb_field(text: &str, key: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with(key))?;
    let kb: u64 = line[key.len()..]
        .trim_start_matches(':')
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kb * 1024)
}

/// Total and available memory, bytes.
fn meminfo(text: &str) -> Option<(u64, u64)> {
    Some((kb_field(text, "MemTotal")?, kb_field(text, "MemAvailable")?))
}

/// A DRM client's id and its GPU time over all engines (ns), from the
/// fdinfo of a file open on a /dev/dri node.
fn drm_engine_time(fdinfo: &str) -> Option<(u64, u64)> {
    let mut id = None;
    let mut ns = None;
    for line in fdinfo.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if key == "drm-client-id" {
            id = value.parse().ok();
        } else if key.starts_with("drm-engine-")
            && let Some(n) = value
                .strip_suffix(" ns")
                .and_then(|n| n.parse::<u64>().ok())
        {
            *ns.get_or_insert(0) += n;
        }
    }
    Some((id?, ns?))
}

/// This process's GPU time over all its DRM clients (ns): each open
/// /dev/dri file reports its client's, and several files can share one.
fn gpu_time() -> Option<u64> {
    let mut seen = Vec::new();
    let mut total = None;
    for entry in std::fs::read_dir("/proc/self/fd").ok()?.flatten() {
        let on_dri =
            std::fs::read_link(entry.path()).is_ok_and(|target| target.starts_with("/dev/dri/"));
        if !on_dri {
            continue;
        }
        let info = std::fs::read_to_string(format!(
            "/proc/self/fdinfo/{}",
            entry.file_name().to_string_lossy()
        ));
        if let Some((id, ns)) = info.ok().as_deref().and_then(drm_engine_time)
            && !seen.contains(&id)
        {
            seen.push(id);
            *total.get_or_insert(0) += ns;
        }
    }
    total
}

/// The GPU's clock (MHz) from its devfreq node, the one named for a GPU.
fn gpu_clock() -> Option<u32> {
    std::fs::read_dir("/sys/class/devfreq")
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().contains("gpu"))
        .and_then(|e| std::fs::read_to_string(e.path().join("cur_freq")).ok())
        .and_then(|hz| hz.trim().parse::<u64>().ok())
        .map(|hz| (hz / 1_000_000) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    // As read on a Steam Frame (SteamOS 0.4.5).
    const STAT: &str = "cpu  400447 132664 196222 7662770 1308 77230 21967 0 0 0\n\
                        cpu0 41641 17281 24359 952174 79 21152 4003 0 0 0\n\
                        cpu1 41118 16882 24181 964064 68 10200 1308 0 0 0\n\
                        intr 1234 5 6\n";
    const MEMINFO: &str = "MemTotal:       16038044 kB\nMemFree:         9109772 kB\n\
                           MemAvailable:   11404720 kB\n";
    const FDINFO: &str = "pos:\t0\nflags:\t02400002\nmnt_id:\t30\nino:\t250\n\
                          drm-driver:\tmsm\ndrm-client-id:\t281\n\
                          drm-engine-gpu:\t1500000 ns\ndrm-cycles-gpu:\t0\n\
                          drm-maxfreq-gpu:\t903000000 Hz\ndrm-total-memory:\t0\n";

    #[test]
    fn parses_what_the_kernel_writes() {
        let total = 400447 + 132664 + 196222 + 7662770 + 1308 + 77230 + 21967;
        assert_eq!(cpu_times(STAT), Some((total - 7662770 - 1308, total)));
        assert_eq!(
            meminfo(MEMINFO),
            Some((16_038_044 * 1024, 11_404_720 * 1024))
        );
        assert_eq!(drm_engine_time(FDINFO), Some((281, 1_500_000)));
        assert_eq!(
            drm_engine_time("pos:\t0\nflags:\t0\n"),
            None,
            "not a DRM file"
        );
        assert_eq!(cpu_times("garbage"), None);
        assert_eq!(millidegrees("38100\n"), Some(38.1));
        assert_eq!(millidegrees("n/a"), None);
    }

    #[test]
    fn sensors_and_cooling_devices_go_with_their_chips() {
        for name in ["gpuss-0-thermal", "gpuss-4-thermal"] {
            assert_eq!(zone_chip(name), Some(Chip::Gpu), "{name}");
        }
        for name in ["cpu7-top-thermal", "cpuss0-thermal", "cpu0-thermal"] {
            assert_eq!(zone_chip(name), Some(Chip::Cpu), "{name}");
        }
        for name in ["video-thermal", "ddr-thermal", "max1720x_bat_7-36"] {
            assert_eq!(zone_chip(name), None, "{name}");
        }
        assert_eq!(cooling_chip("devfreq-3d00000.gpu"), Some(Chip::Gpu));
        assert_eq!(cooling_chip("cpufreq-cpu7"), Some(Chip::Cpu));
        assert_eq!(cooling_chip("devfreq-1d84000.ufshc"), None);
    }

    #[test]
    fn readings_become_percentages() {
        let at = Instant::now();
        let prev = Counters {
            at,
            cpu: Some((100, 1000)),
            gpu_ns: Some(0),
            mem: Some((16 << 30, 12 << 30)),
            gpu_mhz: Some(500),
            cpu_heat: (Some(40.0), false),
            gpu_heat: (Some(50.0), false),
        };
        let now = Counters {
            at: at + Duration::from_millis(500),
            cpu: Some((400, 2000)),
            gpu_ns: Some(250_000_000),
            mem: Some((16 << 30, 10 << 30)),
            gpu_mhz: Some(680),
            cpu_heat: (Some(61.0), false),
            gpu_heat: (Some(96.0), true),
        };
        let s = now.since(&prev);
        assert_eq!(s.cpu, Some(30.0));
        assert_eq!(s.gpu, Some(50.0));
        assert_eq!(s.mem, Some((6 << 30, 16 << 30)));
        assert_eq!(s.gpu_mhz, Some(680));
        assert_eq!((s.cpu_temp, s.cpu_held), (Some(61.0), false));
        assert_eq!((s.gpu_temp, s.gpu_held), (Some(96.0), true));
        // A counter that can't be read gives no number, not a zero.
        let blind = Counters {
            gpu_ns: None,
            ..now
        };
        assert_eq!(blind.since(&prev).gpu, None);
    }

    #[test]
    fn reads_this_machine() {
        let heat = HeatFiles::find();
        let a = Counters::read(&heat);
        std::thread::sleep(Duration::from_millis(50));
        let s = Counters::read(&heat).since(&a);
        if cfg!(target_os = "linux") {
            assert!(s.cpu.is_some());
            assert!(
                s.mem
                    .is_some_and(|(used, total)| used <= total && total > 0)
            );
        }
    }

    fn video(serial: u64, shown: u64) -> VideoNote {
        VideoNote {
            serial,
            playing: true,
            shown,
            stutter: 0,
            net: None,
        }
    }

    #[test]
    fn a_rate_holds_steady_while_frames_arrive_unevenly() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut r = Readings::default();
        r.note(at(0), Some(video(1, 0)));
        assert_eq!(r.video_fps(), None, "a dash until the window has data");
        // 24 fps in bursts: nothing arrives between two of these notes.
        r.note(at(500), Some(video(1, 12)));
        for (ms, shown) in [(1000, 24), (1500, 24), (2000, 48), (2500, 60)] {
            r.note(at(ms), Some(video(1, shown)));
            let fps = r.video_fps().unwrap();
            assert!(fps > 15.0, "{fps} fps at {ms} ms");
        }
        assert_eq!(r.video_fps(), Some(24.0));
        assert_eq!(r.video_fps(), Some(24.0), "reading it changes nothing");
    }

    #[test]
    fn notes_come_on_their_own_timer_and_every_other_one_shows() {
        let t0 = Instant::now();
        let mut r = Readings::default();
        assert!(r.note_due(t0));
        assert!(r.note(t0, None), "the first shows straight away");
        assert!(!r.note_due(t0 + Duration::from_millis(12)));
        let next = t0 + NOTE_EVERY;
        assert!(r.note_due(next));
        assert!(!r.note(next, None));
        assert!(r.note(next + NOTE_EVERY, None));
    }

    #[test]
    fn a_skip_or_a_pause_restarts_the_rate_and_a_new_video_everything() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let net = |bytes| VideoNote {
            net: Some((bytes, 0)),
            ..video(1, 0)
        };
        let mut r = Readings::default();
        r.note(at(0), Some(net(0)));
        r.note(at(2000), Some(net(6_000_000)));
        assert_eq!(r.mb_per_s(), Some(3.0));
        r.note(
            at(2500),
            Some(VideoNote {
                serial: 2,
                ..net(6_500_000)
            }),
        );
        assert_eq!(r.mb_per_s(), None, "what came before the skip is gone");

        let mut r = Readings::default();
        r.note(at(0), Some(video(1, 0)));
        r.note(at(1000), Some(video(1, 24)));
        assert!(r.video_fps().is_some());
        r.note(
            at(1500),
            Some(VideoNote {
                playing: false,
                ..video(1, 24)
            }),
        );
        assert_eq!(r.video_fps(), None, "paused isn't 0 fps");

        r.note(at(2000), Some(video(1, 24)));
        r.new_video();
        assert_eq!(r.video_fps(), None);
    }

    #[test]
    fn trouble_stays_amber_for_a_while_then_clears() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let stutter = |n| VideoNote {
            stutter: n,
            net: Some((0, n)),
            ..video(1, 0)
        };
        let mut r = Readings::default();
        r.note(at(0), Some(stutter(0)));
        assert!(!r.stuttered() && !r.retried());
        r.note(at(1), Some(stutter(1)));
        assert!(r.stuttered() && r.retried());
        r.note(at(10), Some(stutter(1)));
        assert!(r.stuttered(), "still within the window");
        r.note(at(12), Some(stutter(1)));
        assert!(!r.stuttered() && !r.retried());
    }

    #[test]
    fn frames_drawn_and_frame_time_come_from_the_render_loop() {
        let t0 = Instant::now();
        let mut r = Readings::default();
        assert_eq!(r.frame_ms(), None);
        r.note(t0, None);
        for ms in [6.0, 7.0, 30.0] {
            r.frame(Some(ms), Some(90.0));
        }
        assert_eq!(r.frame_ms(), Some(7.0), "the typical one, not the longest");
        r.note(t0 + RATE_WINDOW, None);
        assert_eq!(r.drawn(), Some(1.5));
    }
}
