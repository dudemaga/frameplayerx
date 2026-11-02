//! What is in a media file: streams, duration, chapters, and the format hints
//! (spherical projection, stereo packing, HDR) that drive the renderer.

use crate::input::Input;
use crate::q2d;
use fp_core::format::{ContainerHints, Projection, StereoLayout};
use fp_ffmpeg_sys as ff;
use std::ffi::{CStr, CString};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Video,
    Audio,
    Subtitle,
    Other,
}

/// HDR transfer function of a video stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Transfer {
    #[default]
    Sdr,
    Pq,
    Hlg,
}

#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub index: usize,
    pub kind: StreamKind,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    /// Subtitles for only the parts in another language.
    pub forced: bool,
    /// Subtitles that describe sounds too (SDH).
    pub hearing_impaired: bool,
    // video
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub bit_depth: u32,
    /// FFmpeg's name for the pixel format (`yuv420p`, `yuv420p10le`), empty
    /// when unknown.
    pub pixel_format: String,
    /// Coded as fields (1080i Blu-ray and TV), as far as FFmpeg can tell.
    pub interlaced: bool,
    pub transfer: Transfer,
    /// Clockwise turn that shows the picture upright, from its display
    /// matrix: 0, 90, 180 or 270 degrees. Phones store portrait clips
    /// sideways with a 90° turn.
    pub rotation: u32,
    /// Width of a stored pixel relative to its height. Anamorphic video
    /// (DVDs, some cameras) is stored squeezed and shown wider.
    pub pixel_aspect: f64,
    /// 3D Blu-ray video (H.264 MVC): both eyes decode side by side, so
    /// `width` is twice an eye's.
    pub mvc: bool,
    // audio
    pub sample_rate: u32,
    pub channels: u32,
    /// First-order (or higher) ambisonic audio (AmbiX / ACN-SN3D).
    pub ambisonic: bool,
}

/// The stream is coded as fields: its field order is known and isn't
/// progressive.
pub(crate) fn interlaced(par: &ff::AVCodecParameters) -> bool {
    !matches!(
        par.field_order,
        ff::AV_FIELD_UNKNOWN | ff::AV_FIELD_PROGRESSIVE
    )
}

impl StreamInfo {
    /// The picture's size as shown: unsqueezed, then turned upright.
    pub fn display_size(&self) -> (u32, u32) {
        let w = (self.width as f64 * self.pixel_aspect).round() as u32;
        if self.rotation % 180 == 90 {
            (self.height, w)
        } else {
            (w, self.height)
        }
    }

    /// Chroma subsampling of a video stream, from its pixel format:
    /// `"4:2:0"` (also when unknown), `"4:2:2"` or `"4:4:4"`.
    pub fn chroma(&self) -> &'static str {
        if self.pixel_format.contains("444") || self.pixel_format.starts_with("gbr") {
            "4:4:4"
        } else if self.pixel_format.contains("422") {
            "4:2:2"
        } else {
            "4:2:0"
        }
    }
}

#[derive(Clone, Debug)]
pub struct Chapter {
    pub start: f64,
    pub end: f64,
    pub title: String,
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub name: String,
    pub container: String,
    pub duration: f64,
    pub streams: Vec<StreamInfo>,
    pub chapters: Vec<Chapter>,
    pub hints: ContainerHints,
    pub video: Option<usize>,
    pub audio: Option<usize>,
    pub title: Option<String>,
}

impl MediaInfo {
    pub fn video_stream(&self) -> Option<&StreamInfo> {
        self.video
            .and_then(|i| self.streams.iter().find(|s| s.index == i))
    }
    pub fn audio_streams(&self) -> impl Iterator<Item = &StreamInfo> {
        self.streams.iter().filter(|s| s.kind == StreamKind::Audio)
    }
    pub fn subtitle_streams(&self) -> impl Iterator<Item = &StreamInfo> {
        self.streams
            .iter()
            .filter(|s| s.kind == StreamKind::Subtitle)
    }
}

pub(crate) fn dict_get(dict: *mut ff::AVDictionary, key: &str) -> Option<String> {
    let k = CString::new(key).ok()?;
    // SAFETY: av_dict_get tolerates a null dict; returned entry is owned by it.
    unsafe {
        let e = ff::av_dict_get(dict, k.as_ptr(), std::ptr::null(), 0);
        if e.is_null() || (*e).value.is_null() {
            None
        } else {
            Some(CStr::from_ptr((*e).value).to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
        }
    }
}

fn side_data(par: &ff::AVCodecParameters, kind: ff::AVPacketSideDataType) -> Option<&[u8]> {
    if par.coded_side_data.is_null() || par.nb_coded_side_data <= 0 {
        return None;
    }
    // SAFETY: coded_side_data holds nb_coded_side_data entries.
    let all =
        unsafe { std::slice::from_raw_parts(par.coded_side_data, par.nb_coded_side_data as usize) };
    all.iter()
        .find(|sd| sd.type_ == kind && !sd.data.is_null() && sd.size > 0)
        // SAFETY: data is valid for size bytes while the stream lives.
        .map(|sd| unsafe { std::slice::from_raw_parts(sd.data, sd.size) })
}

/// Maps FFmpeg spherical side data to a projection.
fn spherical(par: &ff::AVCodecParameters) -> Option<Projection> {
    let bytes = side_data(par, ff::AV_PKT_DATA_SPHERICAL)?;
    if bytes.len() < size_of::<ff::AVSphericalMapping>() {
        return None;
    }
    // SAFETY: size checked; FFmpeg stores an AVSphericalMapping here.
    let m = unsafe { &*(bytes.as_ptr() as *const ff::AVSphericalMapping) };
    match m.projection {
        ff::AV_SPHERICAL_EQUIRECTANGULAR => Some(Projection::EQUIRECT_360),
        ff::AV_SPHERICAL_HALF_EQUIRECTANGULAR => Some(Projection::EQUIRECT_180),
        ff::AV_SPHERICAL_EQUIRECTANGULAR_TILE => {
            // Bounds are 0.32 fixed-point fractions cropped from each side.
            let crop = (m.bound_left as f64 + m.bound_right as f64) / 4_294_967_296.0;
            let h_fov = (360.0 * (1.0 - crop)).round() as f32;
            Some(Projection::Equirect {
                h_fov: h_fov.clamp(90.0, 360.0),
                v_fov: 180.0,
            })
        }
        ff::AV_SPHERICAL_CUBEMAP => Some(Projection::Eac { h_fov: 360.0 }),
        ff::AV_SPHERICAL_FISHEYE => Some(Projection::fisheye(180.0)),
        ff::AV_SPHERICAL_RECTILINEAR => Some(Projection::Flat),
        _ => None,
    }
}

/// The stereo layout, and whether the right eye comes first.
fn stereo3d(
    par: &ff::AVCodecParameters,
    metadata: *mut ff::AVDictionary,
) -> Option<(StereoLayout, bool)> {
    if let Some(bytes) = side_data(par, ff::AV_PKT_DATA_STEREO3D)
        && bytes.len() >= size_of::<ff::AVStereo3D>()
    {
        // SAFETY: size checked; FFmpeg stores an AVStereo3D here.
        let s = unsafe { &*(bytes.as_ptr() as *const ff::AVStereo3D) };
        let swapped = s.flags & ff::AV_STEREO3D_FLAG_INVERT as i32 != 0;
        match s.type_ {
            ff::AV_STEREO3D_SIDEBYSIDE | ff::AV_STEREO3D_SIDEBYSIDE_QUINCUNX => {
                return Some((StereoLayout::SideBySide, swapped));
            }
            ff::AV_STEREO3D_TOPBOTTOM => return Some((StereoLayout::TopBottom, swapped)),
            ff::AV_STEREO3D_2D => return Some((StereoLayout::Mono, false)),
            _ => {}
        }
    }
    // Matroska StereoMode as exported in stream metadata.
    match dict_get(metadata, "stereo_mode")?.as_str() {
        "left_right" => Some((StereoLayout::SideBySide, false)),
        "right_left" => Some((StereoLayout::SideBySide, true)),
        "top_bottom" => Some((StereoLayout::TopBottom, false)),
        "bottom_top" => Some((StereoLayout::TopBottom, true)),
        "mono" => Some((StereoLayout::Mono, false)),
        _ => None,
    }
}

fn rotation(par: &ff::AVCodecParameters) -> u32 {
    match side_data(par, ff::AV_PKT_DATA_DISPLAYMATRIX) {
        Some(b) if b.len() >= 36 => {
            // SAFETY: a display matrix is 9 int32 values.
            let r = unsafe { ff::av_display_rotation_get(b.as_ptr() as *const i32) };
            if r.is_nan() {
                0
            } else {
                // FFmpeg gives the turn anticlockwise.
                (-r / 90.0).round().rem_euclid(4.0) as u32 * 90
            }
        }
        _ => 0,
    }
}

/// The stream's pixel aspect, from the container (MP4 `pasp`, Matroska's
/// display size) or else the codec; 1 when unknown or implausible.
fn pixel_aspect(fmt: *mut ff::AVFormatContext, st: *mut ff::AVStream) -> f64 {
    // SAFETY: both are valid while the input is open; a null frame is allowed.
    let sar = unsafe { ff::av_guess_sample_aspect_ratio(fmt, st, std::ptr::null_mut()) };
    let a = q2d(sar);
    if (0.25..=4.0).contains(&a) { a } else { 1.0 }
}

/// How the video stream at `index` is shown: its clockwise turn in degrees
/// and its pixel aspect.
pub(crate) fn picture_shape(input: &Input, index: usize) -> (u32, f64) {
    let st = input.streams()[index];
    // SAFETY: the stream and its parameters are valid while the input is open.
    let par = unsafe { &*(*st).codecpar };
    (rotation(par), pixel_aspect(input.fmt, st))
}

pub fn read_info(input: &Input) -> MediaInfo {
    // SAFETY: input.fmt is open for the duration of this call.
    let fmt = unsafe { &*input.fmt };
    let container = if fmt.iformat.is_null() {
        String::new()
    } else {
        // SAFETY: iformat->name is a static string.
        unsafe {
            CStr::from_ptr((*fmt.iformat).name)
                .to_string_lossy()
                .into_owned()
        }
    };
    let duration = if fmt.duration > 0 {
        fmt.duration as f64 / ff::AV_TIME_BASE as f64
    } else {
        0.0
    };
    let mut hints = ContainerHints::default();
    let mut streams = Vec::new();
    for (index, &st) in input.streams().iter().enumerate() {
        if input.hidden(index) {
            continue;
        }
        // SAFETY: stream pointers and their codec parameters are valid while
        // the input is open.
        let (st, par) = unsafe { (&*st, &*(*st).codecpar) };
        // SAFETY: avcodec_get_name returns a static string.
        let codec = unsafe {
            CStr::from_ptr(ff::avcodec_get_name(par.codec_id))
                .to_string_lossy()
                .into_owned()
        };
        let kind = match par.codec_type {
            ff::AVMEDIA_TYPE_VIDEO
                if st.disposition & ff::AV_DISPOSITION_ATTACHED_PIC as i32 == 0 =>
            {
                StreamKind::Video
            }
            ff::AVMEDIA_TYPE_AUDIO => StreamKind::Audio,
            ff::AVMEDIA_TYPE_SUBTITLE => StreamKind::Subtitle,
            _ => StreamKind::Other,
        };
        let mut s = StreamInfo {
            index,
            kind,
            codec,
            language: dict_get(st.metadata, "language"),
            title: dict_get(st.metadata, "title"),
            default: st.disposition & ff::AV_DISPOSITION_DEFAULT as i32 != 0,
            forced: st.disposition & ff::AV_DISPOSITION_FORCED as i32 != 0,
            hearing_impaired: st.disposition & ff::AV_DISPOSITION_HEARING_IMPAIRED as i32 != 0,
            width: 0,
            height: 0,
            fps: 0.0,
            bit_depth: 8,
            pixel_format: String::new(),
            interlaced: false,
            transfer: Transfer::Sdr,
            rotation: 0,
            pixel_aspect: 1.0,
            mvc: false,
            sample_rate: 0,
            channels: 0,
            ambisonic: false,
        };
        match kind {
            StreamKind::Video => {
                s.width = par.width.max(0) as u32;
                s.height = par.height.max(0) as u32;
                s.fps = q2d(st.avg_frame_rate).max(q2d(st.r_frame_rate).min(240.0));
                // SAFETY: pixdesc lookup on a format value; null when unknown.
                let desc = unsafe { ff::av_pix_fmt_desc_get(par.format) };
                if !desc.is_null() {
                    // SAFETY: non-null descriptors are static.
                    s.bit_depth = unsafe { (*desc).comp[0].depth } as u32;
                    // SAFETY: the descriptor's name is a static C string.
                    s.pixel_format = unsafe { CStr::from_ptr((*desc).name) }
                        .to_string_lossy()
                        .into_owned();
                }
                s.interlaced = interlaced(par);
                s.transfer = match par.color_trc {
                    ff::AVCOL_TRC_SMPTE2084 => Transfer::Pq,
                    ff::AVCOL_TRC_ARIB_STD_B67 => Transfer::Hlg,
                    _ => Transfer::Sdr,
                };
                (s.rotation, s.pixel_aspect) = picture_shape(input, index);
                if hints.projection.is_none() {
                    hints.projection = spherical(par);
                }
                s.mvc = crate::mvc::stream_config(input, index).is_some();
                if s.mvc {
                    s.width *= 2;
                    hints.stereo = Some(StereoLayout::SideBySide);
                    hints.eyes_swapped =
                        dict_get(st.metadata, "stereo_mode").as_deref() == Some("block_rl");
                } else if hints.stereo.is_none()
                    && let Some((stereo, swapped)) = stereo3d(par, st.metadata)
                {
                    hints.stereo = Some(stereo);
                    hints.eyes_swapped = swapped;
                }
            }
            StreamKind::Audio => {
                s.sample_rate = par.sample_rate.max(0) as u32;
                s.channels = par.ch_layout.nb_channels.max(0) as u32;
                s.ambisonic = par.ch_layout.order == ff::AV_CHANNEL_ORDER_AMBISONIC
                    || s.title
                        .as_deref()
                        .is_some_and(|t| t.to_lowercase().contains("ambisonic"));
            }
            _ => {}
        }
        streams.push(s);
    }
    let chapters = if fmt.chapters.is_null() {
        Vec::new()
    } else {
        // SAFETY: chapters has nb_chapters valid pointers.
        unsafe { std::slice::from_raw_parts(fmt.chapters, fmt.nb_chapters as usize) }
            .iter()
            .map(|&c| {
                // SAFETY: chapter pointers are valid while open.
                let c = unsafe { &*c };
                let tb = q2d(c.time_base);
                let at = |t: i64| (t as f64 * tb - input.origin()).max(0.0);
                Chapter {
                    start: at(c.start),
                    end: at(c.end),
                    title: dict_get(c.metadata, "title").unwrap_or_default(),
                }
            })
            .collect()
    };
    // A Blu-ray title: the playlist's length and chapters, not its first
    // clip's.
    let (container, duration, chapters) = match input.title() {
        Some(t) => ("bluray".to_string(), t.duration, t.chapters.clone()),
        None => (container, duration, chapters),
    };
    MediaInfo {
        name: input.name.clone(),
        container,
        duration,
        video: input.best_stream(ff::AVMEDIA_TYPE_VIDEO).filter(|i| {
            streams
                .iter()
                .any(|s: &StreamInfo| s.index == *i && s.kind == StreamKind::Video)
        }),
        audio: input.best_stream(ff::AVMEDIA_TYPE_AUDIO),
        title: dict_get(fmt.metadata, "title"),
        streams,
        chapters,
        hints,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use fp_core::source::{FileSource, MemorySource};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name)
    }

    pub(crate) fn open(name: &str) -> Input {
        let src = Arc::new(FileSource::open(&fixture(name)).expect("fixture"));
        Input::open(src, name, Arc::new(AtomicBool::new(false))).expect("open")
    }

    /// Fixture `name` with `bytes` written `skip` bytes after the first
    /// `marker` (a box type or element id).
    pub(crate) fn patched(
        name: &str,
        marker: &[u8],
        skip: usize,
        bytes: &[u8],
    ) -> Arc<MemorySource> {
        let mut b = std::fs::read(fixture(name)).expect("fixture");
        let at = b
            .windows(marker.len())
            .position(|w| w == marker)
            .expect("marker")
            + skip;
        b[at..at + bytes.len()].copy_from_slice(bytes);
        Arc::new(MemorySource(b))
    }

    /// h264_aac_180_LR.mp4 (640x320) as a phone stores a portrait clip: a
    /// track matrix turning it 90° clockwise.
    pub(crate) fn portrait_clip() -> Arc<MemorySource> {
        // A version 0 tkhd has 40 bytes between its type and the matrix.
        let matrix: Vec<u8> = [0, 0x10000, 0, -0x10000, 0, 0, 320 << 16, 0, 1 << 30]
            .iter()
            .flat_map(|v: &i32| v.to_be_bytes())
            .collect();
        patched("h264_aac_180_LR.mp4", b"tkhd", 4 + 40, &matrix)
    }

    /// h264_aac_180_LR.mp4 with 4:3 pixels (its `pasp` box).
    pub(crate) fn anamorphic_clip() -> Arc<MemorySource> {
        let pasp: Vec<u8> = [4u32, 3].iter().flat_map(|v| v.to_be_bytes()).collect();
        patched("h264_aac_180_LR.mp4", b"pasp", 4, &pasp)
    }

    /// h264_aac_180_LR.mp4, two 320x320 eyes side by side, marked 1:2 as
    /// some full side-by-side re-encodes are.
    pub(crate) fn side_by_side_marked_1_2_clip() -> Arc<MemorySource> {
        let pasp: Vec<u8> = [1u32, 2].iter().flat_map(|v| v.to_be_bytes()).collect();
        patched("h264_aac_180_LR.mp4", b"pasp", 4, &pasp)
    }

    fn info_of(src: Arc<MemorySource>, name: &str) -> MediaInfo {
        read_info(&Input::open(src, name, Arc::new(AtomicBool::new(false))).expect("open"))
    }

    #[test]
    fn portrait_clip_turns_upright() {
        let info = info_of(portrait_clip(), "portrait.mp4");
        let v = info.video_stream().unwrap();
        assert_eq!((v.width, v.height, v.rotation), (640, 320, 90));
        assert_eq!(v.display_size(), (320, 640));
        let plain = read_info(&open("h264_aac_180_LR.mp4"));
        let v = plain.video_stream().unwrap();
        assert_eq!(
            (v.rotation, v.pixel_aspect, v.display_size()),
            (0, 1.0, (640, 320))
        );
    }

    #[test]
    fn anamorphic_clip_unsqueezes() {
        let info = info_of(anamorphic_clip(), "wide.mp4");
        let v = info.video_stream().unwrap();
        assert!(
            (v.pixel_aspect - 4.0 / 3.0).abs() < 1e-6,
            "{}",
            v.pixel_aspect
        );
        assert_eq!(v.display_size(), (853, 320));
    }

    #[test]
    fn a_late_start_is_the_timeline_origin() {
        let mut ts = Input::open(
            crate::ts_fixture::h264_aac_ts(10.0),
            "x.ts",
            Arc::new(AtomicBool::new(false)),
        )
        .expect("open");
        assert!((ts.origin() - 10.0).abs() < 0.1, "origin {}", ts.origin());
        let info = read_info(&ts);
        assert_eq!(info.container, "mpegts");
        assert!((info.duration - 2.0).abs() < 0.2, "{}", info.duration);
        // A seek counts from the start too: the next video is from just
        // before 1.25 s, not from the file's beginning.
        let vi = info.video.unwrap();
        // SAFETY: the stream lives while the input is open.
        let tb = q2d(unsafe { (*ts.streams()[vi]).time_base });
        ts.seek(1.25).unwrap();
        let mut pkt = crate::decode::Packet::new();
        let t = loop {
            assert!(ts.read(pkt.as_ptr()).unwrap(), "no video after the seek");
            if pkt.stream_index() == vi {
                // SAFETY: a packet just read.
                break unsafe { (*pkt.as_ptr()).dts } as f64 * tb - ts.origin();
            }
            pkt.unref();
        };
        assert!((0.75..=1.25).contains(&t), "seeked to {t}");
        assert_eq!(open("h264_aac_180_LR.mp4").origin(), 0.0);
        // A subtitle file starts at its first cue, which stays where it is.
        assert_eq!(open("subs.srt").origin(), 0.0);
    }

    #[test]
    fn right_eye_first_in_matroska() {
        assert!(!read_info(&open("hevc10_tb.mkv")).hints.eyes_swapped);
        // StereoMode 2: top/bottom with the right eye on top.
        let info = info_of(
            patched("hevc10_tb.mkv", &[0x53, 0xb8, 0x81], 3, &[2]),
            "x.mkv",
        );
        assert_eq!(info.hints.stereo, Some(StereoLayout::TopBottom));
        assert!(info.hints.eyes_swapped);
    }

    #[test]
    fn h264_mp4() {
        let info = read_info(&open("h264_aac_180_LR.mp4"));
        assert_eq!(info.container, "mov,mp4,m4a,3gp,3g2,mj2");
        assert!((info.duration - 2.0).abs() < 0.1, "{}", info.duration);
        let v = info.video_stream().unwrap();
        assert_eq!(
            (v.codec.as_str(), v.width, v.height, v.bit_depth),
            ("h264", 640, 320, 8)
        );
        assert!((v.fps - 30.0).abs() < 0.01);
        let a = info.audio_streams().next().unwrap();
        assert_eq!(
            (a.codec.as_str(), a.sample_rate, a.channels, a.ambisonic),
            ("aac", 48000, 2, false)
        );
    }

    /// A VR180 camera/YouTube-style file: no hint in the name, only
    /// Spherical Video V2 metadata (made by tools/make-vr180-fixture.py).
    #[test]
    fn vr180_spherical_metadata() {
        let info = read_info(&open("camera_clip.mp4"));
        assert_eq!(info.hints.stereo, Some(StereoLayout::SideBySide));
        assert_eq!(info.hints.projection, Some(Projection::EQUIRECT_180));
        let d = fp_core::format::resolve(None, info.hints, "camera_clip.mp4", 256, 128);
        assert_eq!(d.evidence, fp_core::format::Evidence::Metadata);
    }

    #[test]
    fn hevc10_mkv_stereo_metadata_and_subtitles() {
        let info = read_info(&open("hevc10_tb.mkv"));
        let v = info.video_stream().unwrap();
        assert_eq!((v.codec.as_str(), v.bit_depth), ("hevc", 10));
        assert_eq!(info.hints.stereo, Some(StereoLayout::TopBottom));
        let s = info.subtitle_streams().next().unwrap();
        assert_eq!(s.codec, "subrip");
        assert_eq!(s.language.as_deref(), Some("eng"));
        assert!(info.audio.is_none());
    }

    #[test]
    fn webm_av1_and_vp9() {
        let a = read_info(&open("av1_opus.webm"));
        assert_eq!(a.video_stream().unwrap().codec, "av1");
        assert_eq!(a.audio_streams().next().unwrap().codec, "opus");
        let b = read_info(&open("vp9.webm"));
        assert_eq!(b.video_stream().unwrap().codec, "vp9");
    }

    #[test]
    fn garbage_fails_cleanly() {
        let src = Arc::new(MemorySource(vec![0u8; 4096]));
        assert!(Input::open(src, "junk.bin", Arc::new(AtomicBool::new(false))).is_err());
    }
}
