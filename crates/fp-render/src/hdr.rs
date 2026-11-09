//! HDR on the SDR screen: the BT.2390 tone curve, gamut compression from
//! BT.2020 into BT.709, and each scene's peak measured from its frames.
//!
//! The scene shader runs the same maths (shaders/hdr.wgsl and scene.wgsl);
//! these copies are what the tests check it against.

/// The screen's white in nits: SDR reference white (BT.2408). HDR is mapped
/// so its peak lands here.
pub const SDR_WHITE: f32 = 203.0;
/// The peak assumed until a video's first frame has been measured.
pub const DEFAULT_PEAK: f32 = 1000.0;

/// Bins of the brightness histogram, evenly spaced in PQ.
pub const BINS: usize = 128;
/// Share of a frame's samples at or below its peak: a few stray bright
/// samples don't set it.
const PERCENTILE: f64 = 0.9999;
/// Frames the peak takes to follow a change within a scene.
const SMOOTHING: f32 = 20.0;
/// A change of the frame's average brightness (PQ) above which the peak
/// starts following faster, and above which it jumps: a scene cut.
const CUT_LOW: f32 = 0.01;
const CUT_HIGH: f32 = 0.03;

const M1: f32 = 0.159_301_76;
const M2: f32 = 78.84375;
const C1: f32 = 0.835_937_5;
const C2: f32 = 18.851_563;
const C3: f32 = 18.6875;

/// PQ code (0..1) of a brightness in nits.
pub fn pq_from_nits(nits: f32) -> f32 {
    let y = (nits.max(0.0) / 10000.0).powf(M1);
    ((C1 + C2 * y) / (1.0 + C3 * y)).powf(M2)
}

/// Brightness in nits of a PQ code.
pub fn nits_from_pq(e: f32) -> f32 {
    let p = e.clamp(0.0, 1.0).powf(1.0 / M2);
    ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / M1) * 10000.0
}

/// BT.2390's EETF: PQ code `e` of a source peaking at PQ `src`, for a
/// display peaking at PQ `dst`. Below the knee nothing changes; above it a
/// Hermite spline bends the rest of the source's range into the display's.
pub fn eetf(e: f32, src: f32, dst: f32) -> f32 {
    if src <= dst {
        return e;
    }
    let e1 = e / src;
    let max_lum = dst / src;
    let ks = (1.5 * max_lum - 0.5).max(0.0);
    if e1 < ks {
        return e;
    }
    let t = (e1.min(1.0) - ks) / (1.0 - ks);
    let (t2, t3) = (t * t, t * t * t);
    let p = (2.0 * t3 - 3.0 * t2 + 1.0) * ks
        + (t3 - 2.0 * t2 + t) * (1.0 - ks)
        + (-2.0 * t3 + 3.0 * t2) * max_lum;
    p * src
}

/// Linear colour in nits, tone mapped through its brightest channel so hue
/// is kept, for a source peaking at `peak` nits; 1.0 is the screen's white.
pub fn tone_map(rgb: [f32; 3], peak: f32) -> [f32; 3] {
    let m = rgb[0].max(rgb[1]).max(rgb[2]);
    if m <= 0.0 {
        return [0.0; 3];
    }
    let mapped = nits_from_pq(eetf(
        pq_from_nits(m),
        pq_from_nits(peak),
        pq_from_nits(SDR_WHITE),
    ));
    rgb.map(|c| c * mapped / m / SDR_WHITE)
}

/// Distance from the brightest channel at which compression starts.
const GAMUT_THRESHOLD: f32 = 0.8;
/// Per channel, the farthest a BT.2020 colour lands outside BT.709 (the
/// cyans, magentas and yellows of its gamut's edge), which is brought to
/// BT.709's edge.
const GAMUT_LIMIT: [f32; 3] = [1.60, 1.09, 1.12];
const GAMUT_POWER: f32 = 1.2;

fn compress_distance(d: f32, limit: f32) -> f32 {
    let (t, p) = (GAMUT_THRESHOLD, GAMUT_POWER);
    if d < t {
        return d;
    }
    let s = (limit - t) / (((1.0 - t) / (limit - t)).powf(-p) - 1.0).powf(1.0 / p);
    let x = (d - t) / s;
    t + s * x / (1.0 + x.powf(p)).powf(1.0 / p)
}

/// Brings linear BT.709 colour converted from BT.2020 inside BT.709: each
/// channel's distance below the brightest is compressed once past 0.8, so
/// most colours stay as they are, the most
/// saturated ones inside BT.709 lose a little to make room, and those
/// outside are desaturated smoothly to its edge, keeping their hue roughly
/// and their brightest channel exactly (the ACES reference gamut
/// compression).
pub fn gamut_compress(rgb: [f32; 3]) -> [f32; 3] {
    let ach = rgb[0].max(rgb[1]).max(rgb[2]);
    if ach <= 0.0 {
        return [0.0; 3];
    }
    let mut out = rgb;
    for (i, c) in out.iter_mut().enumerate() {
        let d = (ach - *c) / ach;
        *c = ach - compress_distance(d, GAMUT_LIMIT[i]) * ach;
    }
    out
}

/// Linear BT.2020 to linear BT.709 (BT.2087).
pub fn bt2020_to_709(c: [f32; 3]) -> [f32; 3] {
    [
        1.6605 * c[0] - 0.5876 * c[1] - 0.0728 * c[2],
        -0.1246 * c[0] + 1.1329 * c[1] - 0.0083 * c[2],
        -0.0182 * c[0] - 0.1006 * c[1] + 1.1187 * c[2],
    ]
}

/// A video's scene peak, from the brightness histograms of its frames
/// (PQ-coded brightest channel, [`BINS`] bins): each frame's peak is its
/// 99.99th percentile, followed smoothly within a scene and at once across
/// a cut, which shows as a jump in the frame's average.
#[derive(Debug, Default)]
pub struct PeakDetector {
    /// Smoothed peak and average, PQ; `None` until a frame was measured.
    state: Option<(f32, f32)>,
}

impl PeakDetector {
    /// Forgets the scene (another video, or HDR stopped).
    pub fn reset(&mut self) {
        self.state = None;
    }

    /// Takes one frame's histogram.
    pub fn add(&mut self, hist: &[u32]) {
        let total: u64 = hist.iter().map(|&n| n as u64).sum();
        if total == 0 {
            return;
        }
        let want = ((total as f64) * PERCENTILE).ceil() as u64;
        let mut seen = 0;
        let mut top = hist.len() - 1;
        for (i, &n) in hist.iter().enumerate() {
            seen += n as u64;
            if seen >= want {
                top = i;
                break;
            }
        }
        let bins = hist.len() as f32;
        let peak = (top + 1) as f32 / bins;
        let sum: f64 = hist
            .iter()
            .enumerate()
            .map(|(i, &n)| (i as f64 + 0.5) * n as f64)
            .sum();
        let average = (sum / total as f64) as f32 / bins;
        let Some((old_peak, old_average)) = self.state else {
            self.state = Some((peak, average));
            return;
        };
        let jump = (average - old_average).abs();
        let k = if jump >= CUT_HIGH {
            1.0
        } else if jump > CUT_LOW {
            let f = (jump - CUT_LOW) / (CUT_HIGH - CUT_LOW);
            1.0 / SMOOTHING + f * (1.0 - 1.0 / SMOOTHING)
        } else {
            1.0 / SMOOTHING
        };
        self.state = Some((
            old_peak + (peak - old_peak) * k,
            old_average + (average - old_average) * k,
        ));
    }

    /// The scene's peak in nits ([`DEFAULT_PEAK`] before any measurement).
    pub fn peak_nits(&self) -> f32 {
        match self.state {
            Some((peak, _)) => nits_from_pq(peak),
            None => DEFAULT_PEAK,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn pq_round_trips() {
        for nits in [0.0, 0.1, 1.0, 50.0, 100.0, 203.0, 1000.0, 4000.0, 10000.0] {
            let back = nits_from_pq(pq_from_nits(nits));
            assert!(close(back, nits, nits * 1e-3 + 1e-3), "{nits} -> {back}");
        }
        assert!(close(pq_from_nits(10000.0), 1.0, 1e-5));
        assert!(close(pq_from_nits(100.0), 0.5081, 1e-3));
    }

    #[test]
    fn the_curve_keeps_what_is_below_the_knee_and_brings_the_peak_to_white() {
        let (src, dst) = (pq_from_nits(1000.0), pq_from_nits(SDR_WHITE));
        // The knee for a 1000-nit source is at about 88 nits.
        for nits in [1.0, 10.0, 50.0, 80.0] {
            let e = pq_from_nits(nits);
            assert_eq!(eetf(e, src, dst), e, "{nits} nits changed");
        }
        assert!(close(eetf(src, src, dst), dst, 1e-5));
        // Brighter than the peak (the percentile left it out): white.
        assert!(close(eetf(1.0, src, dst), dst, 1e-5));
        // Continuous and rising through the knee and up to the peak.
        let mut last = 0.0;
        for i in 0..=1000 {
            let e = src * i as f32 / 1000.0;
            let out = eetf(e, src, dst);
            assert!(out >= last - 1e-6, "falls at {e}: {out} < {last}");
            assert!(out <= dst + 1e-6);
            last = out;
        }
    }

    #[test]
    fn a_source_no_brighter_than_the_screen_is_left_alone() {
        let (src, dst) = (pq_from_nits(150.0), pq_from_nits(SDR_WHITE));
        for nits in [10.0, 100.0, 150.0] {
            let e = pq_from_nits(nits);
            assert_eq!(eetf(e, src, dst), e);
        }
    }

    #[test]
    fn tone_mapping_keeps_hue_and_maps_the_peak_to_white() {
        let [r, g, b] = tone_map([1000.0, 500.0, 250.0], 1000.0);
        assert!(close(r, 1.0, 1e-4), "{r}");
        assert!(close(g / r, 0.5, 1e-4) && close(b / r, 0.25, 1e-4));
        // Below the knee: as graded.
        let [r, ..] = tone_map([50.0, 50.0, 50.0], 1000.0);
        assert!(close(r, 50.0 / SDR_WHITE, 1e-4), "{r}");
        // A dimmer scene peak leaves more of the range alone.
        let dim = tone_map([SDR_WHITE; 3], 400.0)[0];
        let bright = tone_map([SDR_WHITE; 3], 1000.0)[0];
        assert!(dim > bright, "{dim} <= {bright}");
        assert_eq!(tone_map([-1.0, -2.0, 0.0], 1000.0), [0.0; 3]);
    }

    #[test]
    fn most_colours_are_left_alone_and_saturated_ones_change_little() {
        for c in [
            [0.5, 0.4, 0.3],
            [0.2, 0.2, 0.2],
            [0.9, 0.3, 0.25],
            [0.3, 0.5, 0.9],
        ] {
            assert_eq!(gamut_compress(c), c);
        }
        // Inside BT.709 but past the threshold: a little less saturated.
        let [r, g, b] = gamut_compress([0.1, 0.5, 0.9]);
        assert!(r > 0.1 && r < 0.13, "{r}");
        assert_eq!((g, b), (0.5, 0.9));
    }

    #[test]
    fn bt2020_colours_land_inside_bt709_keeping_their_brightest_channel() {
        // The gamut's corners and the colours farthest outside BT.709.
        let edge = [
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.0, 1.0, 1.0],
            [1.0, 0.0, 1.0],
            [1.0, 1.0, 0.0],
            [0.0, 0.64, 0.69],
            [0.69, 0.0, 0.96],
            [0.67, 0.69, 0.0],
        ];
        for c in edge {
            let v = bt2020_to_709(c);
            let out = gamut_compress(v);
            let max = |x: [f32; 3]| x[0].max(x[1]).max(x[2]);
            assert!(close(max(out), max(v), 1e-5), "{c:?}: {out:?}");
            for ch in out {
                assert!(ch >= -1e-3 * max(v), "{c:?} still outside: {out:?}");
            }
            // The brightest channel stays the brightest.
            let argmax = |x: [f32; 3]| (0..3).max_by(|&a, &b| x[a].total_cmp(&x[b])).unwrap();
            assert_eq!(argmax(out), argmax(v), "{c:?}: {v:?} -> {out:?}");
        }
    }

    #[test]
    fn compression_is_smooth_where_it_starts() {
        let below = compress_distance(GAMUT_THRESHOLD - 1e-4, 1.6);
        let above = compress_distance(GAMUT_THRESHOLD + 1e-4, 1.6);
        assert!(close(below, above, 1e-3));
        assert!(close(compress_distance(1.6, 1.6), 1.0, 1e-4));
    }

    fn hist(samples: &[(usize, u32)]) -> Vec<u32> {
        let mut h = vec![0; BINS];
        for &(bin, n) in samples {
            h[bin] = n;
        }
        h
    }

    #[test]
    fn the_first_frame_sets_the_peak_and_stray_samples_do_not() {
        let mut d = PeakDetector::default();
        assert_eq!(d.peak_nits(), DEFAULT_PEAK);
        // 100,000 samples up to bin 70, and 5 brighter: under 0.01%.
        d.add(&hist(&[(40, 60_000), (70, 40_000), (120, 5)]));
        let want = nits_from_pq(71.0 / BINS as f32);
        assert!(
            close(d.peak_nits(), want, 0.01),
            "{} != {want}",
            d.peak_nits()
        );
        d.reset();
        assert_eq!(d.peak_nits(), DEFAULT_PEAK);
        d.add(&[0; BINS]);
        assert_eq!(d.peak_nits(), DEFAULT_PEAK, "an empty histogram counts");
    }

    #[test]
    fn the_peak_follows_slowly_within_a_scene_and_jumps_at_a_cut() {
        let mut d = PeakDetector::default();
        d.add(&hist(&[(60, 1000), (90, 100)]));
        let first = d.peak_nits();
        // Same average, a brighter highlight: a twentieth of the way.
        d.add(&hist(&[(60, 1000), (100, 100)]));
        let pq = |n| pq_from_nits(n);
        let step = (pq(d.peak_nits()) - pq(first)) / (101.0 / 128.0 - pq(first));
        assert!(close(step, 1.0 / SMOOTHING, 1e-3), "{step}");
        // A cut to a dark scene: the average drops by far more than CUT_HIGH.
        d.add(&hist(&[(20, 1000), (40, 100)]));
        let want = nits_from_pq(41.0 / BINS as f32);
        assert!(
            close(d.peak_nits(), want, 0.01),
            "{} != {want}",
            d.peak_nits()
        );
    }
}
