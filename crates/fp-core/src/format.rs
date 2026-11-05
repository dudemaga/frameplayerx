//! Projection and stereo layout of a video, and detection of both from file
//! names (DeoVR / HereSphere conventions) and container metadata.

use serde::{Deserialize, Serialize};

/// How the image maps onto the viewer's field of view.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Projection {
    /// A rectangular screen in front of the viewer (2D or 3D movies).
    Flat,
    /// Equirectangular: `h_fov` degrees wide (180 or 360), `v_fov` tall (180).
    Equirect { h_fov: f32, v_fov: f32 },
    /// Equidistant fisheye with the given full field of view in degrees.
    /// Used by most dual-fisheye VR cameras (MKX200, MKX220, Canon RF 5.2mm).
    Fisheye { fov: f32 },
    /// YouTube equi-angular cubemap, 360 (6 faces) or 180 (front half).
    Eac { h_fov: f32 },
}

impl Projection {
    pub const EQUIRECT_180: Projection = Projection::Equirect {
        h_fov: 180.0,
        v_fov: 180.0,
    };
    pub const EQUIRECT_360: Projection = Projection::Equirect {
        h_fov: 360.0,
        v_fov: 180.0,
    };

    pub fn fisheye(fov: f32) -> Projection {
        Projection::Fisheye { fov }
    }

    /// Short label for UI and file-name suffixes.
    pub fn label(&self) -> String {
        match *self {
            Projection::Flat => "Flat".into(),
            Projection::Equirect { h_fov, .. } => format!("{}°", h_fov.round()),
            Projection::Fisheye { fov } => format!("Fisheye {}°", fov.round()),
            Projection::Eac { h_fov } => format!("EAC {}°", h_fov.round()),
        }
    }

    /// Projections offered in the UI, in display order.
    pub fn presets() -> Vec<(&'static str, Projection)> {
        vec![
            ("Flat screen", Projection::Flat),
            ("180°", Projection::EQUIRECT_180),
            ("360°", Projection::EQUIRECT_360),
            ("Fisheye 180°", Projection::fisheye(180.0)),
            ("Fisheye 190° (Canon RF 5.2)", Projection::fisheye(190.0)),
            ("Fisheye 200° (MKX200)", Projection::fisheye(200.0)),
            ("Fisheye 220° (MKX220/VRCA220)", Projection::fisheye(220.0)),
            ("EAC 360° (YouTube)", Projection::Eac { h_fov: 360.0 }),
            ("EAC 180°", Projection::Eac { h_fov: 180.0 }),
        ]
    }
}

/// How the two eyes are packed in each frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StereoLayout {
    #[default]
    Mono,
    /// Left eye on the left half (full width per eye).
    SideBySide,
    /// Left eye on the left half, squeezed horizontally to half width (e.g. 16:9 container).
    HalfSideBySide,
    /// Left eye on the top half (full height per eye).
    TopBottom,
    /// Left eye on the top half, squeezed vertically to half height (e.g. 16:9 container).
    HalfTopBottom,
}

impl StereoLayout {
    pub fn label(&self) -> &'static str {
        match self {
            StereoLayout::Mono => "Mono",
            StereoLayout::SideBySide => "Side by side",
            StereoLayout::HalfSideBySide => "Half side by side",
            StereoLayout::TopBottom => "Top/bottom",
            StereoLayout::HalfTopBottom => "Half top/bottom",
        }
    }

    /// True if the layout packs two eyes (any 3D format).
    pub fn is_stereo(&self) -> bool {
        *self != StereoLayout::Mono
    }

    /// True if the eye views are side-by-side (full or half).
    pub fn is_side_by_side(&self) -> bool {
        matches!(
            self,
            StereoLayout::SideBySide | StereoLayout::HalfSideBySide
        )
    }

    /// True if the eye views are top-and-bottom (full or half).
    pub fn is_top_bottom(&self) -> bool {
        matches!(self, StereoLayout::TopBottom | StereoLayout::HalfTopBottom)
    }

    /// The pixel aspect each eye is shown with, of a video whose pixels are
    /// `pixel_aspect` times as wide as tall. Some full side-by-side
    /// re-encodes are marked 1:2, the packing itself, so a 2D player shows
    /// the whole frame at one eye's shape: their eyes' pixels are square.
    pub fn eye_pixel_aspect(&self, pixel_aspect: f64) -> f64 {
        if *self == StereoLayout::SideBySide && (pixel_aspect - 0.5).abs() < 0.01 {
            1.0
        } else {
            pixel_aspect
        }
    }
}

/// Complete format of a video.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoFormat {
    pub projection: Projection,
    pub stereo: StereoLayout,
    /// The file packs the right eye first (`_RL`, `_BT`).
    #[serde(default)]
    pub eyes_swapped: bool,
    /// A passthrough video carrying its own see-through mask (`_ALPHA`):
    /// each eye's mask, scaled to [`ALPHA_PACK_SCALE`], sits in the red
    /// channel outside the fisheye circles of a side-by-side frame.
    #[serde(default)]
    pub alpha_packed: bool,
}

/// Size of an alpha-packed video's masks relative to the eye they belong
/// to. The left half's mask is centred on the frame's top middle, the right
/// half's on its corner (wrapping round the edges).
pub const ALPHA_PACK_SCALE: f32 = 0.4;

impl Default for VideoFormat {
    fn default() -> Self {
        VideoFormat {
            projection: Projection::Flat,
            stereo: StereoLayout::Mono,
            eyes_swapped: false,
            alpha_packed: false,
        }
    }
}

impl VideoFormat {
    /// The format of a video nothing identifies (no metadata, file name tag
    /// or telling shape): stereo 180 side by side, the most common VR video
    /// format and what FramePlayerX is mostly used for. FramePlayerX's
    /// settings choose another for the player.
    pub const FALLBACK: VideoFormat = VideoFormat {
        projection: Projection::EQUIRECT_180,
        stereo: StereoLayout::SideBySide,
        eyes_swapped: false,
        alpha_packed: false,
    };

    pub const fn new(projection: Projection, stereo: StereoLayout) -> Self {
        VideoFormat {
            projection,
            stereo,
            eyes_swapped: false,
            alpha_packed: false,
        }
    }

    pub fn label(&self) -> String {
        match self.stereo {
            StereoLayout::Mono => format!("{} mono", self.projection.label()),
            s => format!("{} {}", self.projection.label(), s.label()),
        }
    }

    /// Scale of the packed passthrough mask, if the video has one that can
    /// be read (the packing is only defined for side-by-side frames).
    pub fn alpha_pack_scale(&self) -> Option<f32> {
        (self.alpha_packed && self.stereo.is_side_by_side()).then_some(ALPHA_PACK_SCALE)
    }

    /// Same projection, stereo layout and eye order (the alpha mask aside).
    pub fn same_layout(&self, other: &VideoFormat) -> bool {
        VideoFormat {
            alpha_packed: other.alpha_packed,
            ..*self
        } == *other
    }
}

/// Where a detected format came from, strongest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// The user chose it for this file.
    User,
    /// Spherical / stereo-3D metadata in the container.
    Metadata,
    /// Tokens in the file name.
    FileName,
    /// Guessed from the frame aspect ratio.
    AspectRatio,
    /// Nothing known: plain flat 2D.
    Default,
}

impl Evidence {
    /// Where the format came from, for the UI.
    pub fn label(&self) -> &'static str {
        match self {
            Evidence::User => "your choice",
            Evidence::Metadata => "from file metadata",
            Evidence::FileName => "from the file name",
            Evidence::AspectRatio => "guessed from the shape",
            Evidence::Default => "default",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DetectedFormat {
    pub format: VideoFormat,
    pub evidence: Evidence,
}

/// Splits a file name into upper-case tokens on any non-alphanumeric
/// character, keeping the extension out.
fn tokens(name: &str) -> Vec<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = match base.rfind('.') {
        Some(i) if i > 0 && base.len() - i <= 5 => &base[..i],
        _ => base,
    };
    stem.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_uppercase)
        .collect()
}

/// Detects projection and stereo layout from a file name.
///
/// Follows the conventions DeoVR and HereSphere document: `_LR`/`_SBS`/`_3DH`
/// for side by side, `_TB`/`_OU`/`_TAB`/`_3DV` for top/bottom, `_180`/`_360`,
/// `_FISHEYE`, `_FISHEYE190`, `_MKX200`, `_MKX220`, `_RF52`, `_VRCA220`,
/// `_EAC`, `_FLAT`/`_2D`/`_MONO`, and `_ALPHA` for a packed passthrough
/// mask. Returns `None` when the name says nothing about the layout.
pub fn detect_from_name(name: &str) -> Option<VideoFormat> {
    let toks = tokens(name);
    let has = |t: &str| toks.iter().any(|x| x == t);
    let mut projection = None;
    let mut stereo = None;
    let mut swapped = false;
    let alpha = has("ALPHA");

    for t in &toks {
        match t.as_str() {
            "LR" | "SBS" | "3DH" | "FSBS" => stereo = Some(StereoLayout::SideBySide),
            "HSBS" => stereo = Some(StereoLayout::HalfSideBySide),
            "RL" | "FRL" => {
                stereo = Some(StereoLayout::SideBySide);
                swapped = true;
            }
            "HRL" => {
                stereo = Some(StereoLayout::HalfSideBySide);
                swapped = true;
            }
            "TB" | "OU" | "3DV" | "FOU" | "FTB" | "FTAB" | "OVERUNDER" | "TOPBOTTOM" => {
                stereo = Some(StereoLayout::TopBottom);
            }
            "HOU" | "HTB" | "HTAB" => stereo = Some(StereoLayout::HalfTopBottom),
            // On its own it could be a word in a title ("Bar Tab").
            "TAB" if ["3D", "H", "F", "HALF", "FULL"].iter().any(|t| has(t)) => {
                stereo = Some(StereoLayout::TopBottom);
            }
            "BT" | "FBT" => {
                stereo = Some(StereoLayout::TopBottom);
                swapped = true;
            }
            "HBT" => {
                stereo = Some(StereoLayout::HalfTopBottom);
                swapped = true;
            }
            "MONO" | "2D" => stereo = Some(StereoLayout::Mono),
            "180" | "180X180" | "DOME" | "VR180" | "180VR" | "VR180SBS" | "VR180LR" => {
                projection = Some(Projection::EQUIRECT_180);
                if t.ends_with("SBS") || t.ends_with("LR") {
                    stereo = Some(StereoLayout::SideBySide);
                }
            }
            "360" | "360X180" | "SPHERE" | "VR360" | "360VR" => {
                projection = Some(Projection::EQUIRECT_360);
            }
            "FLAT" => projection = Some(Projection::Flat),
            "EAC" | "EAC360" => projection = Some(Projection::Eac { h_fov: 360.0 }),
            "EAC180" => projection = Some(Projection::Eac { h_fov: 180.0 }),
            "MKX200" => projection = Some(Projection::fisheye(200.0)),
            "MKX220" | "VRCA220" => projection = Some(Projection::fisheye(220.0)),
            "RF52" => projection = Some(Projection::fisheye(190.0)),
            "FISHEYE" => projection = Some(Projection::fisheye(180.0)),
            _ => {
                if let Some(deg) = t
                    .strip_prefix("FISHEYE")
                    .and_then(|d| d.parse::<f32>().ok())
                    && (120.0..=300.0).contains(&deg)
                {
                    projection = Some(Projection::fisheye(deg));
                }
            }
        }
    }

    if has("HALF") {
        if stereo == Some(StereoLayout::SideBySide) {
            stereo = Some(StereoLayout::HalfSideBySide);
        } else if stereo == Some(StereoLayout::TopBottom) {
            stereo = Some(StereoLayout::HalfTopBottom);
        }
    }
    if has("FULL") {
        if stereo == Some(StereoLayout::HalfSideBySide) {
            stereo = Some(StereoLayout::SideBySide);
        } else if stereo == Some(StereoLayout::HalfTopBottom) {
            stereo = Some(StereoLayout::TopBottom);
        }
    }

    // A 180/360 tag on its own implies stereo SBS for 180 (the norm) and mono
    // for 360, unless the name said otherwise.
    if projection.is_none() && stereo.is_none() {
        return None;
    }
    // "VR" plus a stereo tag and no projection means the common 180 format.
    let projection = projection.unwrap_or(
        if stereo.is_some_and(|s| s != StereoLayout::Mono) && has("VR") {
            Projection::EQUIRECT_180
        } else {
            Projection::Flat
        },
    );
    let stereo = stereo.unwrap_or(match projection {
        Projection::Equirect { h_fov, .. } if h_fov <= 180.0 => StereoLayout::SideBySide,
        Projection::Fisheye { .. } => StereoLayout::SideBySide,
        Projection::Eac { h_fov } if h_fov <= 180.0 => StereoLayout::SideBySide,
        _ => StereoLayout::Mono,
    });
    Some(VideoFormat {
        projection,
        stereo,
        eyes_swapped: swapped,
        alpha_packed: alpha,
    })
}

/// Guesses a format from the frame size, as shown, when nothing else is
/// known. Only confident cases: 2:1 is mono 360 or (from 4K up) stereo
/// 180, a big 1:1 frame is 360 top/bottom, and a movie's shape (4:3 to
/// 2.76:1, or two 16:9 pictures side by side or stacked) or a phone's
/// portrait one (4:5 to 9:20, but for 180° top/bottom's 1:2) is a flat
/// screen: no VR format has those shapes, but for YouTube's 3:2 cubemap
/// (EAC), which is left alone from 2560 wide.
pub fn guess_from_aspect(width: u32, height: u32) -> Option<VideoFormat> {
    if width == 0 || height == 0 {
        return None;
    }
    let ar = width as f32 / height as f32;
    let near = |target: f32, tolerance: f32| (ar - target).abs() < tolerance;
    let cubemap = near(1.5, 0.02) && width >= 2560;
    let movie = (1.25..1.9).contains(&ar) || (2.1..2.8).contains(&ar);
    let portrait = (0.44..0.49).contains(&ar) || (0.52..0.81).contains(&ar);
    if (movie && !cubemap) || portrait {
        return Some(VideoFormat::new(Projection::Flat, StereoLayout::Mono));
    }
    // Full side by side (3840x1080) and full over/under (1920x2160) 3D.
    if near(32.0 / 9.0, 0.05) {
        return Some(VideoFormat::new(Projection::Flat, StereoLayout::SideBySide));
    }
    if near(8.0 / 9.0, 0.02) {
        return Some(VideoFormat::new(Projection::Flat, StereoLayout::TopBottom));
    }
    // 3840x1920 and similar: SBS 180 (two square eyes) or mono 360 (2:1).
    // The two are indistinguishable by shape; VR players default to SBS 180
    // for high resolutions because it is far more common for VR video.
    if (ar - 2.0).abs() < 0.02 && width >= 3840 {
        return Some(VideoFormat::new(
            Projection::EQUIRECT_180,
            StereoLayout::SideBySide,
        ));
    }
    if (ar - 2.0).abs() < 0.02 {
        return Some(VideoFormat::new(
            Projection::EQUIRECT_360,
            StereoLayout::Mono,
        ));
    }
    if (ar - 1.0).abs() < 0.02 && width >= 2880 {
        return Some(VideoFormat::new(
            Projection::EQUIRECT_360,
            StereoLayout::TopBottom,
        ));
    }
    None
}

/// Spherical / stereo metadata read from the container (Google Spatial
/// Media, Matroska `StereoMode`, FFmpeg side data).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ContainerHints {
    pub projection: Option<Projection>,
    pub stereo: Option<StereoLayout>,
    /// With `stereo`: the right eye comes first (Matroska's `right_left`
    /// and `bottom_top`).
    pub eyes_swapped: bool,
}

/// For flat screens, refines generic SideBySide or TopBottom into HalfSideBySide
/// or HalfTopBottom if the frame aspect ratio indicates squeezed 3D (e.g. 16:9
/// container) rather than full-resolution 3D (32:9 or 8:9), unless the file
/// name explicitly specified full (`FSBS`, `FOU`, `FULL`).
fn refine_flat_stereo(format: &mut VideoFormat, name: &str, width: u32, height: u32) {
    if format.projection != Projection::Flat || width == 0 || height == 0 {
        return;
    }
    let toks = tokens(name);
    let is_explicit_full = toks
        .iter()
        .any(|t| t == "FSBS" || t == "FOU" || t == "FTB" || t == "FTAB" || t == "FULL");
    if is_explicit_full {
        return;
    }
    let ar = width as f32 / height as f32;
    if format.stereo == StereoLayout::SideBySide && ar < 2.8 {
        format.stereo = StereoLayout::HalfSideBySide;
    } else if format.stereo == StereoLayout::TopBottom && ar > 1.2 {
        format.stereo = StereoLayout::HalfTopBottom;
    }
}

/// Combines every source of evidence. `user` wins, then container metadata,
/// then the file name, then the aspect ratio.
pub fn resolve(
    user: Option<VideoFormat>,
    hints: ContainerHints,
    name: &str,
    width: u32,
    height: u32,
) -> DetectedFormat {
    if let Some(format) = user {
        return DetectedFormat {
            format,
            evidence: Evidence::User,
        };
    }
    let from_name = detect_from_name(name);
    if hints.projection.is_some() || hints.stereo.is_some() {
        let projection = hints
            .projection
            .or(from_name.map(|f| f.projection))
            .unwrap_or(Projection::Flat);
        let stereo = hints
            .stereo
            .or(from_name.map(|f| f.stereo))
            .unwrap_or_default();
        let eyes_swapped = match hints.stereo {
            Some(_) => hints.eyes_swapped,
            None => from_name.is_some_and(|f| f.eyes_swapped),
        };
        let alpha_packed = from_name.is_some_and(|f| f.alpha_packed);
        let mut format = VideoFormat {
            projection,
            stereo,
            eyes_swapped,
            alpha_packed,
        };
        refine_flat_stereo(&mut format, name, width, height);
        return DetectedFormat {
            format,
            evidence: Evidence::Metadata,
        };
    }
    if let Some(mut format) = from_name {
        refine_flat_stereo(&mut format, name, width, height);
        return DetectedFormat {
            format,
            evidence: Evidence::FileName,
        };
    }
    if let Some(format) = guess_from_aspect(width, height) {
        return DetectedFormat {
            format,
            evidence: Evidence::AspectRatio,
        };
    }
    DetectedFormat {
        format: VideoFormat::FALLBACK,
        evidence: Evidence::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Projection as P;
    use StereoLayout as S;

    fn d(name: &str) -> Option<(P, S, bool)> {
        detect_from_name(name).map(|f| (f.projection, f.stereo, f.eyes_swapped))
    }

    #[test]
    fn tab_means_top_and_bottom_unless_it_could_be_a_word() {
        let flat = |s| Some((P::Flat, s, false));
        assert_eq!(d("Clip.HTAB.mkv"), flat(S::HalfTopBottom));
        assert_eq!(d("HTAB-3D.mkv"), flat(S::HalfTopBottom));
        assert_eq!(d("Clip.FTAB.mkv"), flat(S::TopBottom));
        assert_eq!(d("Movie.3D.TAB.1080p.mkv"), flat(S::TopBottom));
        assert_eq!(d("Movie.H-TAB.mkv"), flat(S::TopBottom));
        assert_eq!(d("Movie.Half-TAB.mkv"), flat(S::HalfTopBottom));
        for name in ["Bar Tab.mkv", "Tab Hunter 2015.mkv", "Clip.TAB.mkv"] {
            assert_eq!(d(name), None, "{name}");
        }
        let halves = resolve(
            None,
            ContainerHints::default(),
            "Movie.H-TAB.mkv",
            1920,
            1080,
        );
        assert_eq!(halves.format.stereo, S::HalfTopBottom, "by its shape");
        let full = resolve(
            None,
            ContainerHints::default(),
            "Movie.FTAB.mkv",
            1920,
            1080,
        );
        assert_eq!(full.format.stereo, S::TopBottom, "named full");
    }

    #[test]
    fn vr180_names() {
        let sbs180 = Some((P::EQUIRECT_180, S::SideBySide, false));
        for n in [
            "MyTrip_VR180.mp4",
            "vr180_beach.mov",
            "Beach.VR180.SBS.mp4",
            "GoPro_VR180_8K.mp4",
            "trip-vr180-3d.mp4",
            "x_VR180SBS.mp4",
        ] {
            assert_eq!(d(n), sbs180, "{n}");
        }
        assert_eq!(
            d("talk_VR180_2D.mp4"),
            Some((P::EQUIRECT_180, S::Mono, false))
        );
        assert_eq!(
            d("talk_VR180_TB.mp4"),
            Some((P::EQUIRECT_180, S::TopBottom, false))
        );
        assert_eq!(d("tour_VR360.mp4"), Some((P::EQUIRECT_360, S::Mono, false)));
    }

    #[test]
    fn deovr_style_names() {
        assert_eq!(
            d("Scene_180_LR.mp4"),
            Some((P::EQUIRECT_180, S::SideBySide, false))
        );
        assert_eq!(
            d("scene-180x180_3dh.mp4"),
            Some((P::EQUIRECT_180, S::SideBySide, false))
        );
        assert_eq!(
            d("Trip_360_TB.mkv"),
            Some((P::EQUIRECT_360, S::TopBottom, false))
        );
        assert_eq!(
            d("Trip_360_mono.mp4"),
            Some((P::EQUIRECT_360, S::Mono, false))
        );
        assert_eq!(d("Trip_360.mp4"), Some((P::EQUIRECT_360, S::Mono, false)));
        assert_eq!(
            d("clip_180.mp4"),
            Some((P::EQUIRECT_180, S::SideBySide, false))
        );
        assert_eq!(
            d("Movie.3D.HSBS.1080p.mkv"),
            Some((P::Flat, S::HalfSideBySide, false))
        );
        assert_eq!(
            d("Movie.Half-SBS.1080p.mkv"),
            Some((P::Flat, S::HalfSideBySide, false))
        );
        assert_eq!(
            d("Movie.3D.FSBS.1080p.mkv"),
            Some((P::Flat, S::SideBySide, false))
        );
        assert_eq!(
            d("Movie.3D.HOU.1080p.mkv"),
            Some((P::Flat, S::HalfTopBottom, false))
        );
        assert_eq!(d("Movie_FLAT_OU.mp4"), Some((P::Flat, S::TopBottom, false)));
        assert_eq!(d("Movie.mkv"), None);
        assert_eq!(d("Holiday 2024.mp4"), None);
    }

    #[test]
    fn alpha_packed_passthrough() {
        let f = detect_from_name("Concert Pass-Through_4096p_FISHEYE190_alpha.mp4").unwrap();
        assert_eq!(f.projection, P::fisheye(190.0));
        assert_eq!(f.stereo, S::SideBySide);
        assert!(f.alpha_packed);
        assert!(!detect_from_name("Scene_180_LR.mp4").unwrap().alpha_packed);
        assert!(
            !detect_from_name("Alphabet_180_LR.mp4")
                .unwrap()
                .alpha_packed
        );
        // Container metadata decides the layout; the name still says alpha.
        let hints = ContainerHints {
            projection: Some(P::EQUIRECT_180),
            stereo: Some(S::SideBySide),
            ..Default::default()
        };
        assert!(
            resolve(None, hints, "x_180_LR_alpha.mp4", 8192, 4096)
                .format
                .alpha_packed
        );
        let plain = VideoFormat::new(P::fisheye(190.0), S::SideBySide);
        assert!(f.same_layout(&plain) && plain.same_layout(&f));
        assert!(!f.same_layout(&VideoFormat::new(P::EQUIRECT_180, S::SideBySide)));
    }

    #[test]
    fn swapped_eyes() {
        assert_eq!(
            d("x_180_RL.mp4"),
            Some((P::EQUIRECT_180, S::SideBySide, true))
        );
        assert_eq!(
            d("x_360_BT.mp4"),
            Some((P::EQUIRECT_360, S::TopBottom, true))
        );
    }

    #[test]
    fn eye_order_from_metadata() {
        let stereo = |stereo, eyes_swapped| ContainerHints {
            stereo: Some(stereo),
            eyes_swapped,
            ..Default::default()
        };
        let swapped = |hints, name| resolve(None, hints, name, 1920, 2160).format.eyes_swapped;
        assert!(swapped(stereo(S::TopBottom, true), "a.mkv"));
        // The metadata gives the order along with the layout.
        assert!(!swapped(stereo(S::TopBottom, false), "a_BT.mkv"));
        // Projection alone: the name still gives the order.
        let projection = ContainerHints {
            projection: Some(P::EQUIRECT_180),
            ..Default::default()
        };
        assert!(swapped(projection, "a_RL.mp4"));
    }

    #[test]
    fn fisheye_family() {
        assert_eq!(
            d("a_MKX200.mp4"),
            Some((P::fisheye(200.0), S::SideBySide, false))
        );
        assert_eq!(
            d("a_mkx220_LR.mp4"),
            Some((P::fisheye(220.0), S::SideBySide, false))
        );
        assert_eq!(
            d("a_VRCA220.mp4"),
            Some((P::fisheye(220.0), S::SideBySide, false))
        );
        assert_eq!(
            d("a_RF52.mp4"),
            Some((P::fisheye(190.0), S::SideBySide, false))
        );
        assert_eq!(
            d("a_FISHEYE190.mp4"),
            Some((P::fisheye(190.0), S::SideBySide, false))
        );
        assert_eq!(
            d("a_fisheye.mp4"),
            Some((P::fisheye(180.0), S::SideBySide, false))
        );
        assert_eq!(d("a_FISHEYE9999.mp4"), None);
    }

    #[test]
    fn eac() {
        assert_eq!(
            d("yt_EAC.webm"),
            Some((P::Eac { h_fov: 360.0 }, S::Mono, false))
        );
        assert_eq!(
            d("yt_EAC180_LR.webm"),
            Some((P::Eac { h_fov: 180.0 }, S::SideBySide, false))
        );
    }

    #[test]
    fn extension_and_paths_ignored() {
        assert_eq!(d("/media/vr/180/notes.txt"), None);
        assert_eq!(
            d("C:\\vr\\Scene_LR_180.MP4"),
            Some((P::EQUIRECT_180, S::SideBySide, false))
        );
    }

    #[test]
    fn aspect_guess() {
        assert_eq!(guess_from_aspect(5760, 2880).unwrap().stereo, S::SideBySide);
        assert_eq!(
            guess_from_aspect(2048, 1024).unwrap().projection,
            P::EQUIRECT_360
        );
        assert_eq!(guess_from_aspect(4096, 4096).unwrap().stereo, S::TopBottom);
        // Movies: 16:9, 4:3, 1.85, scope, and full-width 3D.
        let flat = Some(VideoFormat::new(P::Flat, S::Mono));
        for (w, h) in [
            (1920, 1080),
            (3840, 2160),
            (1440, 1080),
            (1998, 1080),
            (1920, 800),
        ] {
            assert_eq!(guess_from_aspect(w, h), flat, "{w}x{h}");
        }
        assert_eq!(
            guess_from_aspect(3840, 1080),
            Some(VideoFormat::new(P::Flat, S::SideBySide))
        );
        assert_eq!(
            guess_from_aspect(1920, 2160),
            Some(VideoFormat::new(P::Flat, S::TopBottom))
        );
        assert_eq!(guess_from_aspect(720, 480), flat, "DVD");
        // Phone clips held upright: 9:16, 3:4, 4:5 and a screen recording.
        for (w, h) in [
            (1080, 1920),
            (2160, 3840),
            (1440, 1920),
            (1080, 1350),
            (1080, 2340),
        ] {
            assert_eq!(guess_from_aspect(w, h), flat, "{w}x{h}");
        }
        assert_eq!(guess_from_aspect(4096, 8192), None, "180° top/bottom");
        // Nothing to go by: a cubemap's shape, square below 360 sizes, or
        // no size.
        assert_eq!(guess_from_aspect(3072, 2048), None);
        assert_eq!(guess_from_aspect(1080, 1080), None);
        assert_eq!(guess_from_aspect(0, 0), None);
    }

    #[test]
    fn resolve_precedence() {
        let user = VideoFormat::new(P::Flat, S::Mono);
        assert_eq!(
            resolve(Some(user), Default::default(), "a_180_LR.mp4", 0, 0).evidence,
            Evidence::User
        );
        let meta = ContainerHints {
            projection: Some(P::EQUIRECT_360),
            ..Default::default()
        };
        let r = resolve(None, meta, "a_TB.mp4", 4096, 4096);
        assert_eq!(
            (r.evidence, r.format.projection, r.format.stereo),
            (Evidence::Metadata, P::EQUIRECT_360, S::TopBottom)
        );
        assert_eq!(
            resolve(None, Default::default(), "a_180_LR.mp4", 0, 0).evidence,
            Evidence::FileName
        );
        assert_eq!(
            resolve(None, Default::default(), "a.mp4", 5760, 2880).evidence,
            Evidence::AspectRatio
        );
        assert_eq!(
            resolve(None, Default::default(), "a.mp4", 1920, 1080).format,
            VideoFormat::new(P::Flat, S::Mono)
        );
        let unknown = resolve(None, Default::default(), "a.mp4", 1080, 1080);
        assert_eq!(
            (unknown.evidence, unknown.format),
            (Evidence::Default, VideoFormat::FALLBACK)
        );
        // Generic SBS with 16:9 frame is refined to HalfSideBySide:
        assert_eq!(
            resolve(None, Default::default(), "Movie_SBS.mkv", 1920, 1080)
                .format
                .stereo,
            S::HalfSideBySide
        );
        // Generic SBS with 32:9 frame remains Full SideBySide:
        assert_eq!(
            resolve(None, Default::default(), "Movie_SBS.mkv", 3840, 1080)
                .format
                .stereo,
            S::SideBySide
        );
        // Explicit FSBS is not refined to Half:
        assert_eq!(
            resolve(None, Default::default(), "Movie_FSBS.mkv", 1920, 1080)
                .format
                .stereo,
            S::SideBySide
        );
        // VR180 SBS is spherical and not refined:
        assert_eq!(
            resolve(None, Default::default(), "VR180_SBS.mp4", 3840, 1920)
                .format
                .stereo,
            S::SideBySide
        );
        // Generic OU with 16:9 frame is refined to HalfTopBottom:
        assert_eq!(
            resolve(None, Default::default(), "Movie_OU.mkv", 1920, 1080)
                .format
                .stereo,
            S::HalfTopBottom
        );
        // Generic OU with 8:9 frame remains Full TopBottom:
        assert_eq!(
            resolve(None, Default::default(), "Movie_OU.mkv", 1920, 2160)
                .format
                .stereo,
            S::TopBottom
        );
    }

    #[test]
    fn serde_roundtrip() {
        let f = VideoFormat::new(P::fisheye(200.0), S::SideBySide);
        let j = serde_json::to_string(&f).unwrap();
        assert_eq!(serde_json::from_str::<VideoFormat>(&j).unwrap(), f);
        assert!(j.contains("\"kind\":\"fisheye\""), "{j}");
    }

    #[test]
    fn full_side_by_side_marked_1_2_has_square_eyes() {
        use StereoLayout::*;
        assert_eq!(SideBySide.eye_pixel_aspect(0.5), 1.0);
        // Anything else is the eyes' own: 1:2 in other layouts, 2:1 top/bottom
        // (also real 2x anamorphic), DVD pixels.
        for (layout, pa) in [
            (Mono, 0.5),
            (HalfSideBySide, 0.5),
            (HalfTopBottom, 0.5),
            (TopBottom, 2.0),
            (HalfSideBySide, 2.0),
            (SideBySide, 32.0 / 27.0),
        ] {
            assert_eq!(layout.eye_pixel_aspect(pa), pa, "{layout:?} {pa}");
        }
    }
}
