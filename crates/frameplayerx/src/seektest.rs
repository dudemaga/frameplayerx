//! `frameplayerx --seek-test FILE|URL`: plays a video the way a viewer
//! skipping through it does, without a headset, GPU or sound, and checks
//! what testers ran into: skips that buffer for long or forever (an SMB
//! share stalled after a few), pausing, and hardware decoder sessions not
//! given back when a video closes (every later video "Hardware decoder
//! busy" until a restart).
//!
//! Each round opens the video, plays a little (`--play` seconds, reported
//! every 10 s, when given), skips `--step` seconds `--seeks` times (back to
//! the start near the end), pauses for a few seconds, then closes it and
//! checks that FramePlayerX holds no hardware decoder session any more.
//! Network locations go through the sources set up in FramePlayerX (SMB
//! credentials included).
//!
//! It also measures stream reads: what the demuxer read by each picture, how
//! fast, and how long it waited, with medians at the end. `--trace-reads`
//! prints every read, so a file's read pattern can be checked.

use crate::services::Opener;
use crate::settings::Settings;
use fp_core::ByteSource;
use fp_core::sync::lock;
use fp_media::{Player, PlayerConfig, PlayerState, audio::output::Backend};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct SeekTestOptions {
    pub seeks: usize,
    pub step: f64,
    pub rounds: usize,
    pub hw: fp_media::HwDecode,
    /// Seconds to play after opening, reporting every [`REPORT_EVERY`].
    pub play: Option<f64>,
    pub trace_reads: bool,
    /// Instead of playing, reads straight through for this many seconds.
    pub read_test: Option<f64>,
    /// Seconds of video to read ahead, in place of the player's defaults.
    pub read_ahead: Option<f64>,
}

impl Default for SeekTestOptions {
    fn default() -> Self {
        SeekTestOptions {
            seeks: 30,
            step: 60.0,
            rounds: 3,
            hw: fp_media::HwDecode::Auto,
            play: None,
            trace_reads: false,
            read_test: None,
            read_ahead: None,
        }
    }
}

/// A skip taking longer than this to show a picture is a failure.
const STUCK: Duration = Duration::from_secs(20);
/// Slower than this is reported as buffering.
const SLOW: Duration = Duration::from_secs(2);
const REPORT_EVERY: f64 = 10.0;
/// Reads this far apart aren't one burst: the demuxer reads straight on until
/// its queue is full, then waits about 10 ms at a time.
const HELD_BACK: Duration = Duration::from_millis(5);
/// A shorter burst may have come from the block cache alone, which says
/// nothing about the link.
const MIN_BURST: u64 = 16 << 20;

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 20) as f64
}

fn millis(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn mb_per_s(bytes: u64, d: Duration) -> f64 {
    bytes as f64 / 1e6 / d.as_secs_f64().max(1e-9)
}

/// Reads with no wait for the queue between them.
#[derive(Clone, Copy)]
struct Burst {
    start: Instant,
    end: Instant,
    bytes: u64,
}

impl Burst {
    fn counted(&self) -> (u64, Duration) {
        if self.bytes < MIN_BURST {
            return (0, Duration::ZERO);
        }
        (self.bytes, self.end.duration_since(self.start))
    }
}

/// What the demuxer has read in one round.
#[derive(Default)]
struct ReadTally {
    bytes: u64,
    waited: Duration,
    next: u64,
    in_order: u64,
    burst: Option<Burst>,
    closed_bytes: u64,
    closed_time: Duration,
    awaiting_jump: bool,
    first_jump: Option<Duration>,
}

impl ReadTally {
    /// Returns the bytes read in order before this read when it jumped.
    fn record(&mut self, offset: u64, got: u64, began: Instant, took: Duration) -> Option<u64> {
        let end = began + took;
        self.bytes += got;
        self.waited += took;
        let jump = (offset != self.next).then_some(self.in_order);
        if jump.is_some() {
            self.in_order = 0;
            if self.awaiting_jump {
                self.awaiting_jump = false;
                self.first_jump = Some(took);
            }
        }
        self.in_order += got;
        self.next = offset + got;
        match &mut self.burst {
            Some(b) if began.saturating_duration_since(b.end) < HELD_BACK => {
                b.end = b.end.max(end);
                b.bytes += got;
            }
            _ => {
                let (bytes, time) = self.burst.map_or((0, Duration::ZERO), |b| b.counted());
                self.closed_bytes += bytes;
                self.closed_time += time;
                self.burst = Some(Burst {
                    start: began,
                    end,
                    bytes: got,
                });
            }
        }
        jump
    }

    /// Bytes and time spent reading while the demuxer's queue had room.
    fn filling(&self) -> (u64, Duration) {
        let (bytes, time) = self.burst.map_or((0, Duration::ZERO), |b| b.counted());
        (self.closed_bytes + bytes, self.closed_time + time)
    }

    /// A skip is starting: the next jump is its first read.
    fn start_skip(&mut self) {
        self.awaiting_jump = true;
        self.first_jump = None;
    }
}

/// Passes reads through, counting them. The tally lives apart so the source
/// still closes with the player.
struct ReadProbe {
    src: Arc<dyn ByteSource>,
    tally: Arc<Mutex<ReadTally>>,
    started: Instant,
    trace: bool,
}

impl ByteSource for ReadProbe {
    fn size(&self) -> Option<u64> {
        self.src.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let began = Instant::now();
        let r = self.src.read_at(offset, buf);
        let took = began.elapsed();
        let got = *r.as_ref().unwrap_or(&0) as u64;
        let jump = lock(&self.tally).record(offset, got, began, took);
        if self.trace {
            let jump = jump
                .map(|run| format!(", jump after {:.2} MiB in order", mib(run)))
                .unwrap_or_default();
            println!(
                "  read +{:.3} s at {offset} len {} got {got} in {:.1} ms{jump}",
                began.duration_since(self.started).as_secs_f64(),
                buf.len(),
                millis(took),
            );
        }
        r
    }

    fn describe(&self) -> String {
        self.src.describe()
    }

    fn streamed(&self) -> bool {
        self.src.streamed()
    }

    fn remote(&self) -> bool {
        self.src.remote()
    }
}

/// The middle value, or the mean of the middle two.
fn median(values: &[f64]) -> Option<f64> {
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let mid = v.len() / 2;
    match v.len() {
        0 => None,
        n if n % 2 == 1 => Some(v[mid]),
        _ => Some((v[mid - 1] + v[mid]) / 2.0),
    }
}

/// Medians and totals across every round.
#[derive(Default)]
struct Summary {
    first_picture_ms: Vec<f64>,
    first_picture_mib: Vec<f64>,
    skip_ms: Vec<f64>,
    skip_mib: Vec<f64>,
    first_read_ms: Vec<f64>,
    thrown_away_mib: Vec<f64>,
    most_queued: u64,
    played: Duration,
    shown: u64,
    dropped: u64,
    stalls: u64,
    played_bytes: u64,
    played_waited: Duration,
    filling_bytes: u64,
    filling_time: Duration,
}

impl Summary {
    fn print(&self) {
        println!("summary:");
        if let (Some(ms), Some(read)) = (
            median(&self.first_picture_ms),
            median(&self.first_picture_mib),
        ) {
            println!(
                "  first picture: median {ms:.0} ms, read {read:.1} MiB, of {}",
                self.first_picture_ms.len()
            );
        }
        if let (Some(ms), Some(read)) = (median(&self.skip_ms), median(&self.skip_mib)) {
            let slowest = self.skip_ms.iter().copied().fold(0.0, f64::max);
            println!(
                "  skip to picture: median {ms:.0} ms, slowest {slowest:.0} ms, read {read:.1} MiB, of {}",
                self.skip_ms.len()
            );
        }
        if let Some(ms) = median(&self.first_read_ms) {
            let slowest = self.first_read_ms.iter().copied().fold(0.0, f64::max);
            println!(
                "  first read after a skip: median {ms:.0} ms, slowest {slowest:.0} ms, of {}",
                self.first_read_ms.len()
            );
        }
        if let Some(thrown) = median(&self.thrown_away_mib) {
            println!("  buffer thrown away by a skip: median {thrown:.1} MiB");
        }
        if !self.played.is_zero() {
            println!("  most queued: {:.1} MiB", mib(self.most_queued));
            println!(
                "  played {:.0} s: {} frames, {} dropped, {} stalls, read {:.1} MB/s, waiting on reads {:.0}% of the time",
                self.played.as_secs_f64(),
                self.shown,
                self.dropped,
                self.stalls,
                mb_per_s(self.played_bytes, self.played),
                self.played_waited.as_secs_f64() / self.played.as_secs_f64() * 100.0,
            );
        }
        if self.filling_bytes == 0 {
            println!(
                "  read speed while the buffer fills: no burst of {:.0} MiB",
                mib(MIN_BURST)
            );
        } else {
            println!(
                "  read speed while the buffer fills: {:.1} MB/s over {:.0} MiB",
                mb_per_s(self.filling_bytes, self.filling_time),
                mib(self.filling_bytes),
            );
        }
    }
}

/// Runs the display loop at ~90 Hz for `d`, or until `until` holds.
/// Returns whether `until` held.
fn run_for(p: &Player, d: Duration, mut until: impl FnMut(&Player) -> bool) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        let _ = p.current_frame();
        if until(p) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(11));
    }
    false
}

/// Shows the first picture at or after `target` (playing, not buffering).
fn landed(p: &Player, target: f64) -> bool {
    p.state() != PlayerState::Buffering && p.current_frame().is_some_and(|f| f.pts >= target - 1.0)
}

/// Shows the first picture of a skip from `from` to `target`. Skipping back,
/// the frame from before the skip is already past `target`.
fn landed_skip(p: &Player, from: f64, target: f64) -> bool {
    landed(p, target) && (target >= from || p.current_frame().is_some_and(|f| f.pts < from - 1.0))
}

/// Plays `d` and returns (stalls, frames dropped, frames shown) meanwhile.
fn play(p: &Player, d: Duration) -> (u64, u64, u64) {
    let a = p.stats();
    run_for(p, d, |_| false);
    let b = p.stats();
    (
        b.stalls - a.stalls,
        b.frames_dropped - a.frames_dropped,
        b.frames_shown - a.frames_shown,
    )
}

/// Plays `secs`, printing what happened every [`REPORT_EVERY`].
fn play_reporting(p: &Player, tally: &Mutex<ReadTally>, secs: f64, summary: &mut Summary) {
    let mut done = 0.0;
    while done < secs {
        let slice = (secs - done).min(REPORT_EVERY);
        let (bytes, waited) = {
            let t = lock(tally);
            (t.bytes, t.waited)
        };
        let began = Instant::now();
        let (stalls, dropped, shown) = play(p, Duration::from_secs_f64(slice));
        let took = began.elapsed();
        let (bytes, waited) = {
            let t = lock(tally);
            (t.bytes - bytes, t.waited - waited)
        };
        done += slice;
        println!(
            "  played to {done:>4.0} s: {shown} frames, {dropped} dropped, {stalls} stalls, read {:.1} MB/s, waiting on reads {:.0}% of the time",
            mb_per_s(bytes, took),
            waited.as_secs_f64() / took.as_secs_f64() * 100.0,
        );
        summary.played += took;
        summary.shown += shown;
        summary.dropped += dropped;
        summary.stalls += stalls;
        summary.played_bytes += bytes;
        summary.played_waited += waited;
        summary.most_queued = summary.most_queued.max(p.stats().bytes_queued as u64);
    }
}

/// Closes on a thread of its own, like the app, and says how long it took.
fn close(p: Player) -> Result<Duration, String> {
    let t = Instant::now();
    let h = std::thread::spawn(move || drop(p));
    while !h.is_finished() {
        if t.elapsed() > STUCK {
            return Err(format!(
                "closing the video did not finish in {} s",
                STUCK.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = h.join();
    Ok(t.elapsed())
}

/// The file's name. A URL's query goes, since it can carry a token.
fn file_name(location: &str) -> String {
    let path = if location.contains("://") {
        location.split(['?', '#']).next().unwrap_or(location)
    } else {
        location
    };
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Reads from the start as fast as `src` delivers, with no player to wait for.
fn read_test(src: &dyn ByteSource, secs: f64) -> Result<(), String> {
    let mut buf = vec![0u8; 256 * 1024];
    let t = Instant::now();
    let mut at = 0;
    while t.elapsed().as_secs_f64() < secs {
        let n = src.read_at(at, &mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        at += n as u64;
    }
    println!(
        "read {:.0} MiB in {:.1} s: {:.1} MB/s",
        mib(at),
        t.elapsed().as_secs_f64(),
        mb_per_s(at, t.elapsed())
    );
    Ok(())
}

pub fn run(location: &str, opts: &SeekTestOptions) -> Result<(), String> {
    let opener = Opener::new();
    let configs = fp_sources::load_configs(&Settings::sources_path()).unwrap_or_default();
    for e in opener.set_sources(&configs) {
        log::warn!("source unavailable: {e}");
    }
    if let Some(secs) = opts.read_test {
        return read_test(&*opener.open_in(None, location)?, secs);
    }
    let name = file_name(location);
    let mut failures = Vec::new();
    let mut summary = Summary::default();
    println!(
        "seek test: {name}, {} rounds of {} skips of {} s",
        opts.rounds, opts.seeks, opts.step
    );
    for round in 1..=opts.rounds {
        let held = fp_media::hardware_sessions();
        if held != 0 {
            failures.push(format!(
                "round {round}: {held} hardware session(s) held before opening"
            ));
        }
        let t = Instant::now();
        let tally = Arc::new(Mutex::new(ReadTally::default()));
        let src: Arc<dyn ByteSource> = Arc::new(ReadProbe {
            src: opener.open_in(None, location)?,
            tally: tally.clone(),
            started: t,
            trace: opts.trace_reads,
        });
        let defaults = PlayerConfig::default();
        let cfg = PlayerConfig {
            hw_decode: opts.hw,
            audio: Backend::Null,
            read_ahead_secs: opts.read_ahead.unwrap_or(defaults.read_ahead_secs),
            remote_read_ahead_secs: opts.read_ahead.unwrap_or(defaults.remote_read_ahead_secs),
            ..defaults
        };
        let p = Player::open(src, &name, cfg, |_| fp_media::Tracks::default())
            .map_err(|e| format!("{name}: {e}"))?;
        let opened = t.elapsed();
        let opened_read = lock(&tally).bytes;
        if !run_for(&p, STUCK, |p| landed(p, 0.0)) {
            failures.push(format!("round {round}: no picture after opening"));
            close(p)?;
            break;
        }
        let first_picture = t.elapsed();
        let read = lock(&tally).bytes;
        summary.first_picture_ms.push(millis(first_picture));
        summary.first_picture_mib.push(mib(read));
        let st = p.stats();
        println!(
            "round {round}: opened in {} ms, read {:.1} MiB; first picture in {} ms, read {:.1} MiB; decoder {}{}, {} hardware session(s)",
            opened.as_millis(),
            mib(opened_read),
            first_picture.as_millis(),
            mib(read),
            st.video_decoder,
            if st.hardware { " (hardware)" } else { "" },
            fp_media::hardware_sessions()
        );
        if !st.hardware && opts.hw == fp_media::HwDecode::Auto {
            failures.push(format!("round {round}: decoding in software"));
        }
        match opts.play {
            Some(secs) => play_reporting(&p, &tally, secs, &mut summary),
            None => {
                let (stalls, dropped, shown) = play(&p, Duration::from_secs(2));
                println!("  play 2 s: {shown} frames, {dropped} dropped, {stalls} stalls");
            }
        }

        let duration = p.duration();
        let mut slowest = Duration::ZERO;
        for seek in 1..=opts.seeks {
            let from = p.position();
            let mut target = from + opts.step;
            if duration > 0.0 && target > duration - 5.0 {
                target = 5.0_f64.min(duration / 2.0);
            }
            summary
                .thrown_away_mib
                .push(mib(p.stats().bytes_queued as u64));
            let before = {
                let mut t = lock(&tally);
                t.start_skip();
                t.bytes
            };
            let t = Instant::now();
            p.seek(target, true);
            if !run_for(&p, STUCK, |p| landed_skip(p, from, target)) {
                let msg = format!(
                    "round {round} skip {seek} to {target:.0} s: no picture after {} s (state {:?}{})",
                    STUCK.as_secs(),
                    p.state(),
                    p.error().map(|e| format!(", {e}")).unwrap_or_default()
                );
                println!("  {msg}");
                failures.push(msg);
                break;
            }
            let took = t.elapsed();
            slowest = slowest.max(took);
            let (read, first_read) = {
                let t = lock(&tally);
                (t.bytes - before, t.first_jump)
            };
            summary.skip_ms.push(millis(took));
            summary.skip_mib.push(mib(read));
            summary.first_read_ms.extend(first_read.map(millis));
            let (stalls, dropped, shown) = play(&p, Duration::from_millis(1500));
            let st = p.stats();
            let flag = if took > SLOW { "  <- buffered" } else { "" };
            println!(
                "  skip {seek:>3} to {:>6.0} s: picture in {:>5} ms, read {:.1} MiB, first read {} ms, then {shown} frames, {dropped} dropped, {stalls} stalls, {}{flag}",
                target,
                took.as_millis(),
                mib(read),
                first_read.map_or("-".to_string(), |d| format!("{:.0}", millis(d))),
                st.video_decoder
            );
            if took > SLOW {
                failures.push(format!(
                    "round {round} skip {seek}: buffered {} ms",
                    took.as_millis()
                ));
            }
        }
        println!("  slowest skip: {} ms", slowest.as_millis());

        // Paused, the picture stands still: a decoder short of buffers
        // used to wait forever here and never close.
        p.pause();
        run_for(&p, Duration::from_secs(3), |_| false);
        p.play();
        let (_, _, shown) = play(&p, Duration::from_secs(2));
        println!("  after a 3 s pause: {shown} frames in 2 s");
        if shown == 0 {
            failures.push(format!("round {round}: no frames after pausing"));
        }

        let took = close(p)?;
        // The native decoder's session goes with the decode thread; give
        // the driver a moment.
        let deadline = Instant::now() + Duration::from_secs(2);
        while fp_media::hardware_sessions() != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let held = fp_media::hardware_sessions();
        println!(
            "  closed in {} ms, {held} hardware session(s) still held",
            took.as_millis()
        );
        if held != 0 {
            failures.push(format!(
                "round {round}: {held} hardware session(s) not given back"
            ));
        }
        let (bytes, time) = lock(&tally).filling();
        summary.filling_bytes += bytes;
        summary.filling_time += time;
    }
    summary.print();
    if failures.is_empty() {
        println!("PASS");
        Ok(())
    } else {
        println!("FAIL");
        for f in &failures {
            println!("  {f}");
        }
        Err(format!("{} problem(s)", failures.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_MIB: u64 = 1 << 20;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_jump_reports_the_bytes_read_in_order_before_it() {
        let mut t = ReadTally::default();
        let at = Instant::now();
        assert_eq!(t.record(0, ONE_MIB, at, ms(1)), None);
        assert_eq!(t.record(ONE_MIB, ONE_MIB, at, ms(1)), None);
        assert_eq!(t.record(100 * ONE_MIB, 10, at, ms(1)), Some(2 * ONE_MIB));
        assert_eq!(t.record(100 * ONE_MIB + 10, 10, at, ms(1)), None);
        assert_eq!(t.record(0, 10, at, ms(1)), Some(20));
        assert_eq!(t.bytes, 2 * ONE_MIB + 30);
        assert_eq!(t.waited, ms(5));
    }

    #[test]
    fn the_first_read_of_a_skip_is_its_first_jump() {
        let mut t = ReadTally::default();
        let at = Instant::now();
        t.record(0, 100, at, ms(1));
        t.start_skip();
        t.record(100, 100, at, ms(2));
        assert_eq!(t.first_jump, None, "still reading on from before the skip");
        t.record(5000, 100, at, ms(30));
        t.record(9000, 100, at, ms(40));
        assert_eq!(t.first_jump, Some(ms(30)));
        t.start_skip();
        assert_eq!(t.first_jump, None);
    }

    #[test]
    fn filling_speed_counts_long_bursts_and_ignores_the_wait_between_them() {
        let mut t = ReadTally::default();
        let start = Instant::now();
        let read_burst = |t: &mut ReadTally, from: Duration, offset: u64, count: u64| {
            for i in 0..count {
                let began = start + from + ms(2 * i);
                t.record(offset + i * ONE_MIB, ONE_MIB, began, ms(1));
            }
        };
        read_burst(&mut t, ms(0), 0, 20);
        assert_eq!(t.filling(), (20 * ONE_MIB, ms(39)), "a burst still running");
        // A second later: 16 MiB more, far apart from the first.
        read_burst(&mut t, ms(1000), 20 * ONE_MIB, 16);
        assert_eq!(t.filling(), (36 * ONE_MIB, ms(39 + 31)));
        // Reads the block cache could have served say nothing about the link.
        read_burst(&mut t, ms(2000), 36 * ONE_MIB, 8);
        read_burst(&mut t, ms(3000), 44 * ONE_MIB, 8);
        assert_eq!(t.filling(), (36 * ONE_MIB, ms(39 + 31)));
    }

    #[test]
    fn overlapping_readers_stay_one_burst() {
        let mut t = ReadTally::default();
        let start = Instant::now();
        t.record(0, 16 * ONE_MIB, start, ms(10));
        t.record(5000 * ONE_MIB, ONE_MIB, start + ms(1), ms(3));
        assert_eq!(t.filling(), (17 * ONE_MIB, ms(10)));
    }

    struct Bytes(Vec<u8>);

    impl ByteSource for Bytes {
        fn size(&self) -> Option<u64> {
            Some(self.0.len() as u64)
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            let rest = self.0.get(offset as usize..).unwrap_or_default();
            let n = rest.len().min(buf.len());
            buf[..n].copy_from_slice(&rest[..n]);
            Ok(n)
        }
        fn describe(&self) -> String {
            "bytes".into()
        }
        fn streamed(&self) -> bool {
            true
        }
        fn remote(&self) -> bool {
            true
        }
    }

    #[test]
    fn the_probe_passes_everything_through_and_counts_what_came_back() {
        let tally = Arc::new(Mutex::new(ReadTally::default()));
        let probe = ReadProbe {
            src: Arc::new(Bytes(vec![7; 100])),
            tally: tally.clone(),
            started: Instant::now(),
            trace: false,
        };
        assert_eq!(probe.size(), Some(100));
        assert_eq!(probe.describe(), "bytes");
        assert!(probe.streamed(), "demuxers must not seek in a live stream");
        assert!(probe.remote(), "the player keeps more in hand");
        let mut buf = [0u8; 60];
        assert_eq!(probe.read_at(90, &mut buf).unwrap(), 10);
        assert_eq!(buf[..10], [7; 10]);
        assert_eq!(lock(&tally).bytes, 10);
    }

    #[test]
    fn median_of_any_count() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0]), Some(3.0));
        assert_eq!(median(&[9.0, 1.0, 5.0]), Some(5.0));
        assert_eq!(median(&[9.0, 1.0, 5.0, 3.0]), Some(4.0));
    }

    #[test]
    fn a_urls_query_stays_out_of_its_name() {
        assert_eq!(
            file_name("https://cdn.example/dl/film.mkv?token=secret#t=1"),
            "film.mkv"
        );
        assert_eq!(file_name("/media/what?.mkv"), "what?.mkv");
        assert_eq!(file_name("smb://nas/share/a.mp4"), "a.mp4");
    }
}
