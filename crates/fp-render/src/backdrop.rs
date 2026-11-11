//! Finds a chroma-keyed video's backdrop, the green or blue screen behind
//! what it shows, in a picture of one of its frames: the commonest strongly
//! coloured shade, when it covers enough of the picture.

use crate::params::chroma;
use glam::{Vec2, Vec3};

/// `colour` (sRGB 0..1) at full strength, as the one-tap key colours are:
/// its lowest channel at 0 and its highest at 1. A backdrop on video is
/// paler, close enough to grey that a key at its own colour would take the
/// people in front of it too.
pub fn pure(colour: [f32; 3]) -> [f32; 3] {
    let lo = colour.iter().copied().fold(f32::INFINITY, f32::min);
    let hi = colour.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    colour.map(|c| (c - lo) / (hi - lo).max(1e-3))
}

/// Chroma (BT.709 Cb and Cr, as the key compares them) below which a sample
/// is too grey to be a backdrop: skin stays below it.
const MIN_CHROMA: f32 = 0.15;
/// Darker than this, a sample is outside the picture (letterboxing, around
/// a fisheye's circles) or too dark to tell.
const MIN_LUMA: f32 = 0.06;
/// How far in chroma a backdrop's samples stray from its colour, as light
/// falls unevenly across it.
const SPREAD: f32 = 0.1;
/// The least of the picture a backdrop covers.
const MIN_SHARE: f32 = 0.15;
/// The least of the frame the picture covers: a fade or a logo on black
/// is no backdrop.
const MIN_LIT: f32 = 0.25;
/// Histogram bins each way over Cb and Cr, -0.5 to 0.5.
const BINS: usize = 32;

/// The backdrop's colour in an RGBA picture of a frame, sRGB 0..1: the
/// commonest strongly coloured shade, when it covers enough of the picture.
pub fn find(rgba: &[u8]) -> Option<[f32; 3]> {
    let luma = Vec3::new(0.2126, 0.7152, 0.0722);
    let pixels = rgba.as_chunks::<4>().0;
    let all = pixels.len();
    let samples: Vec<(Vec3, Vec2)> = pixels
        .iter()
        .map(|p| Vec3::new(p[0].into(), p[1].into(), p[2].into()) / 255.0)
        .filter(|c| c.dot(luma) >= MIN_LUMA)
        .map(|c| (c, chroma(c.to_array())))
        .collect();
    if (samples.len() as f32) < MIN_LIT * all as f32 {
        return None;
    }
    let coloured = || samples.iter().filter(|(_, k)| k.length() >= MIN_CHROMA);
    let bin = |v: f32| (((v + 0.5) * BINS as f32) as usize).min(BINS - 1);
    let mut hist = [[0u32; BINS]; BINS];
    for (_, k) in coloured() {
        hist[bin(k.x)][bin(k.y)] += 1;
    }
    let around = |x: usize, y: usize| -> u32 {
        let near = |i: usize| i.saturating_sub(1)..=(i + 1).min(BINS - 1);
        near(x)
            .flat_map(|a| near(y).map(move |b| (a, b)))
            .map(|(a, b)| hist[a][b])
            .sum()
    };
    let (bx, by) = (0..BINS)
        .flat_map(|x| (0..BINS).map(move |y| (x, y)))
        .max_by_key(|&(x, y)| around(x, y))?;
    let mid = |i: usize| (i as f32 + 0.5) / BINS as f32 - 0.5;
    let mut centre = Vec2::new(mid(bx), mid(by));
    // From the densest bins to the middle of the samples near them, a few
    // times, as an unevenly lit backdrop spreads over several bins.
    let mut backdrop = (Vec3::ZERO, 0);
    for _ in 0..3 {
        let (mut k_sum, mut c_sum, mut n) = (Vec2::ZERO, Vec3::ZERO, 0);
        for (c, k) in coloured().filter(|(_, k)| k.distance(centre) < SPREAD) {
            k_sum += *k;
            c_sum += *c;
            n += 1;
        }
        if n == 0 {
            return None;
        }
        centre = k_sum / n as f32;
        backdrop = (c_sum / n as f32, n);
    }
    let (colour, n) = backdrop;
    (n as f32 >= MIN_SHARE * samples.len() as f32).then(|| colour.to_array())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: u32 = 128;

    /// A probe-sized picture whose pixel at (u, v), 0..1, is `f`'s colour.
    fn picture(f: impl Fn(f32, f32) -> [u8; 3]) -> Vec<u8> {
        let mut rgba = Vec::new();
        for y in 0..SIZE {
            for x in 0..SIZE {
                let [r, g, b] = f(
                    (x as f32 + 0.5) / SIZE as f32,
                    (y as f32 + 0.5) / SIZE as f32,
                );
                rgba.extend_from_slice(&[r, g, b, 255]);
            }
        }
        rgba
    }

    fn near(got: [f32; 3], want: [u8; 3], tol: f32) -> bool {
        got.iter()
            .zip(want)
            .all(|(g, w)| (g * 255.0 - f32::from(w)).abs() <= tol)
    }

    /// A little noise, the same each run.
    fn noise(u: f32, v: f32) -> i16 {
        ((u * 977.0 + v * 613.0).sin() * 9.0) as i16
    }

    fn shade(c: [u8; 3], by: i16) -> [u8; 3] {
        c.map(|x| (i16::from(x) + by).clamp(0, 255) as u8)
    }

    #[test]
    fn a_green_screen_behind_a_person() {
        let green = [40, 190, 70];
        let p = picture(|u, v| {
            if (u - 0.5).abs() < 0.15 && v > 0.25 {
                shade([170, 130, 110], noise(u, v))
            } else {
                shade(green, noise(u, v))
            }
        });
        let found = find(&p).expect("a backdrop");
        assert!(near(found, green, 4.0), "{found:?}");
    }

    #[test]
    fn an_unevenly_lit_blue_screen_in_fisheye_circles() {
        // Two image circles on black, brighter at the top, a subject in each.
        let p = picture(|u, v| {
            let x = (u * 2.0).fract();
            let r = ((x - 0.5).powi(2) + (v - 0.5).powi(2)).sqrt();
            if r > 0.5 {
                [0, 0, 0]
            } else if r < 0.15 {
                [200, 160, 140]
            } else {
                let lit = 1.0 - 0.4 * v;
                [(25.0 * lit) as u8, (60.0 * lit) as u8, (210.0 * lit) as u8]
            }
        });
        let [r, g, b] = find(&p).expect("a backdrop");
        assert!(b > 0.55 && r < 0.15 && g < 0.25, "{:?}", [r, g, b]);
    }

    #[test]
    fn no_backdrop_in_an_ordinary_picture() {
        // Hues all round, none covering much.
        let rainbow = picture(|u, v| {
            let h = u * 6.0;
            let c = |o: f32| ((((h + o) % 6.0) - 3.0).abs() - 1.0).clamp(0.0, 1.0);
            let l = 0.4 + 0.5 * v;
            [c(0.0), c(4.0), c(2.0)].map(|x| (x * l * 255.0) as u8)
        });
        assert_eq!(find(&rainbow), None);
        let grey = picture(|u, _| [(60.0 + 150.0 * u) as u8; 3]);
        assert_eq!(find(&grey), None);
        // Skin up close.
        let face = picture(|u, v| shade([205, 150, 125], noise(u, v)));
        assert_eq!(find(&face), None);
    }

    #[test]
    fn a_fade_or_a_logo_on_black_is_no_backdrop() {
        let logo = picture(|u, v| {
            if (u - 0.5).abs() < 0.2 && (v - 0.5).abs() < 0.1 {
                [30, 200, 60]
            } else {
                [2, 2, 2]
            }
        });
        assert_eq!(find(&logo), None);
        assert_eq!(find(&picture(|_, _| [0, 0, 0])), None);
    }

    #[test]
    fn a_backdrop_at_full_strength() {
        // The green found in a real video becomes about the Green screen
        // swatch.
        let [r, g, b] = pure([0.24, 0.69, 0.25]);
        assert!(r.abs() < 1e-6 && (g - 1.0).abs() < 1e-6 && (b - 0.022).abs() < 0.005);
        // A blue screen keeps its hue.
        let [r, g, b] = pure([0.12, 0.27, 0.78]);
        assert!(r.abs() < 1e-6 && (g - 0.227).abs() < 0.005 && (b - 1.0).abs() < 1e-6);
        // Grey has no hue to keep.
        assert_eq!(pure([0.5; 3]), [0.0; 3]);
    }
}
