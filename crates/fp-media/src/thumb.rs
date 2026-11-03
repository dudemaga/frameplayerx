//! Probing and single-frame grabs for the media library.

use crate::decode::{Decoder, Frame, HwDecode, Packet};
use crate::info::{MediaInfo, read_info};
use crate::input::Input;
use crate::{Error, Result};
use fp_core::ByteSource;
use fp_core::format::StereoLayout;
use fp_ffmpeg_sys as ff;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Which part of the frame to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Crop {
    Full,
    /// Left half (side-by-side stereo).
    LeftHalf,
    /// Left half stretched horizontally by 2x (half side-by-side stereo).
    LeftHalfStretched,
    /// Top half (top/bottom stereo).
    TopHalf,
    /// Top half stretched vertically by 2x (half top/bottom stereo).
    TopHalfStretched,
}

impl Crop {
    /// The stereo layout whose eye this keeps.
    fn layout(self) -> StereoLayout {
        match self {
            Crop::Full => StereoLayout::Mono,
            Crop::LeftHalf => StereoLayout::SideBySide,
            Crop::LeftHalfStretched => StereoLayout::HalfSideBySide,
            Crop::TopHalf => StereoLayout::TopBottom,
            Crop::TopHalfStretched => StereoLayout::HalfTopBottom,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Decodes the first video frame (or a still image: PNG, JPEG) at full size.
/// Used for photo viewing and by renderer tests.
pub fn decode_first_frame(src: Arc<dyn ByteSource>, name: &str) -> Result<crate::VideoFrame> {
    let mut input = Input::open(src, name, Arc::new(AtomicBool::new(false)))?;
    let vi = input
        .best_stream(ff::AVMEDIA_TYPE_VIDEO)
        .ok_or(Error::NoStream("video"))?;
    let mut dec = Decoder::open(&input, vi, HwDecode::Off)?;
    let mut pkt = Packet::new();
    let mut frame = Frame::new();
    let mut conv = crate::frame::FrameConverter::default();
    let mut eof = false;
    for _ in 0..10_000 {
        if !eof {
            if input.read(pkt.as_ptr())? {
                if pkt.stream_index() == vi {
                    dec.send(pkt.as_ptr())?;
                }
                pkt.unref();
            } else {
                eof = true;
                dec.send(std::ptr::null())?;
            }
        }
        match dec.receive(&mut frame)? {
            Some(true) => {
                let t = dec.frame_time(&frame).unwrap_or(0.0);
                return conv.convert(frame.take_raw(), t, 0.0, 0);
            }
            Some(false) => break,
            None => {}
        }
    }
    Err(Error::Unsupported("no frame decoded".into()))
}

/// Reads stream information without decoding. Setting `interrupt` makes
/// reads in progress fail, so a probe of a slow network file can be cut
/// short.
pub fn probe(
    src: Arc<dyn ByteSource>,
    name: &str,
    interrupt: Arc<AtomicBool>,
) -> Result<MediaInfo> {
    let input = Input::open(src, name, interrupt)?;
    Ok(read_info(&input))
}

/// Decodes the frame a thumbnail at `at` seconds shows (see
/// [`Grabber::grab`]) and scales it to fit within `max_w` x `max_h`, keeping
/// the aspect ratio of the cropped area. Setting `interrupt` stops it at the
/// next read.
pub fn grab(
    src: Arc<dyn ByteSource>,
    name: &str,
    at: f64,
    max_w: u32,
    max_h: u32,
    crop: Crop,
    interrupt: Arc<AtomicBool>,
) -> Result<Rgba> {
    Grabber::open(src, name, interrupt)?.grab(at, max_w, max_h, crop)
}

/// Thumbnails of one video: opened (and probed) once, then any number of
/// frames.
pub struct Grabber {
    input: Input,
    info: MediaInfo,
    /// The video stream, its clockwise turn and pixel aspect, and its
    /// decoder, once a frame was asked for.
    video: Option<(usize, (u32, f64), Decoder)>,
    /// Whether it has read on from the start.
    used: bool,
    /// When the last picture grabbed shows.
    last: Option<f64>,
}

impl Grabber {
    /// Opens a video for thumbnails, reading what [`probe`] does. Setting
    /// `interrupt` makes reads in progress fail.
    pub fn open(
        src: Arc<dyn ByteSource>,
        name: &str,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Grabber> {
        let input = Input::open(src, name, interrupt)?;
        let info = read_info(&input);
        Ok(Grabber {
            input,
            info,
            video: None,
            used: false,
            last: None,
        })
    }

    /// The video stream, its shape and its decoder, opened on first use:
    /// in software (the hardware decoder is playback's), keyframes only.
    fn video(&mut self) -> Result<&mut (usize, (u32, f64), Decoder)> {
        if self.video.is_none() {
            let index = self
                .input
                .best_stream(ff::AVMEDIA_TYPE_VIDEO)
                .ok_or(Error::NoStream("video"))?;
            let shape = crate::info::picture_shape(&self.input, index);
            let dec = Decoder::open(&self.input, index, HwDecode::Stills)?;
            self.video = Some((index, shape, dec));
        }
        Ok(self.video.as_mut().expect("just opened"))
    }

    /// What the video holds, as [`probe`] reads it.
    pub fn info(&self) -> &MediaInfo {
        &self.info
    }

    /// The keyframe at or before `at` seconds (the first picture when `at`
    /// is 0), scaled to fit within `max_w` x `max_h` keeping the aspect
    /// ratio of the cropped area. Only that one picture is decoded: the
    /// frames between it and `at` would cost far more than they show, a
    /// whole group of pictures of 4K or 8K for each thumbnail. When that
    /// keyframe is the one grabbed last (thumbnails closer together than
    /// keyframes), the next keyframe instead.
    pub fn grab(&mut self, at: f64, max_w: u32, max_h: u32, crop: Crop) -> Result<Rgba> {
        self.video()?;
        if at > 0.0 || self.used {
            self.input.seek(at.max(0.0))?;
        }
        self.used = true;
        let (mut frame, mut shows) = self.keyframe()?;
        if at > 0.0
            && self.last.is_some()
            && shows == self.last
            && self.input.seek_after(at).is_ok()
            && let Ok((next, next_shows)) = self.keyframe()
            && next_shows > self.last
        {
            (frame, shows) = (next, next_shows);
        }
        self.last = shows;
        let shape = self.video()?.1;
        scale(&frame, max_w, max_h, crop, shape)
    }

    /// The first picture decoded from where the input is, and when it
    /// shows.
    fn keyframe(&mut self) -> Result<(Frame, Option<f64>)> {
        let Grabber { input, video, .. } = self;
        let (index, _, dec) = video.as_mut().expect("opened by grab");
        dec.flush();
        let mut pkt = Packet::new();
        let mut frame = Frame::new();
        for _ in 0..10_000 {
            let more = input.read(pkt.as_ptr())?;
            if more && pkt.stream_index() != *index {
                pkt.unref();
                continue;
            }
            if more {
                dec.send(pkt.as_ptr())?;
                pkt.unref();
                if let Some(true) = dec.receive(&mut frame)? {
                    let t = dec.frame_time(&frame);
                    return Ok((frame, t));
                }
            }
            // A decoder holds pictures back to put them in order, until
            // later ones are decoded, which here are skipped: drained, it
            // hands the keyframe out now instead of after the next one.
            dec.send(std::ptr::null())?;
            for _ in 0..4 {
                match dec.receive(&mut frame)? {
                    Some(true) => {
                        let t = dec.frame_time(&frame);
                        return Ok((frame, t));
                    }
                    Some(false) => break,
                    None => {}
                }
            }
            if !more {
                break;
            }
            // Nothing yet (a picture it skipped): on to the next packet.
            dec.flush();
        }
        Err(Error::Unsupported("no frame decoded".into()))
    }
}

/// Crops and scales `frame` as shown: unsqueezed and turned upright by
/// `(rotation, pixel_aspect)`, the eye's pixel aspect as the player has it
/// ([`StereoLayout::eye_pixel_aspect`]).
fn scale(
    frame: &Frame,
    max_w: u32,
    max_h: u32,
    crop: Crop,
    (rotation, pixel_aspect): (u32, f64),
) -> Result<Rgba> {
    let (max_w, max_h) = if rotation % 180 == 90 {
        (max_h, max_w)
    } else {
        (max_w, max_h)
    };
    // SAFETY: frame is decoded; swscale reads only the cropped region by
    // limiting the source width/height (pointers start at the top-left).
    unsafe {
        let f = &*frame.as_ptr();
        let (sw, sh, aspect_w, aspect_h) = match crop {
            Crop::Full => (f.width, f.height, f.width as f64, f.height as f64),
            Crop::LeftHalf => (f.width / 2, f.height, (f.width / 2) as f64, f.height as f64),
            Crop::LeftHalfStretched => (f.width / 2, f.height, f.width as f64, f.height as f64),
            Crop::TopHalf => (f.width, f.height / 2, f.width as f64, (f.height / 2) as f64),
            Crop::TopHalfStretched => (f.width, f.height / 2, f.width as f64, f.height as f64),
        };
        if sw <= 0 || sh <= 0 {
            return Err(Error::Unsupported("empty frame".into()));
        }
        let aspect_w = aspect_w * crop.layout().eye_pixel_aspect(pixel_aspect);
        let s = (max_w as f64 / aspect_w)
            .min(max_h as f64 / aspect_h)
            .min(1.0);
        let dw = ((aspect_w * s).round() as i32).max(2) & !1;
        let dh = ((aspect_h * s).round() as i32).max(2) & !1;
        let ctx = ff::sws_getContext(
            sw,
            sh,
            f.format,
            dw,
            dh,
            ff::AV_PIX_FMT_RGBA,
            (ff::SWS_AREA | ff::SWS_ACCURATE_RND | ff::SWS_FULL_CHR_H_INT) as i32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if ctx.is_null() {
            return Err(Error::Unsupported(format!(
                "cannot scale pixel format {}",
                f.format
            )));
        }
        let mut pixels = vec![0u8; (dw * dh * 4) as usize];
        let dst = [
            pixels.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ];
        let dst_stride = [dw * 4, 0, 0, 0];
        ff::sws_scale(
            ctx,
            f.data.as_ptr() as *const *const u8,
            f.linesize.as_ptr(),
            0,
            sh,
            dst.as_ptr(),
            dst_stride.as_ptr(),
        );
        ff::sws_freeContext(ctx);
        let img = Rgba {
            width: dw as u32,
            height: dh as u32,
            pixels,
        };
        Ok(turned(img, rotation))
    }
}

/// `img` turned clockwise by `degrees`, a multiple of 90.
fn turned(img: Rgba, degrees: u32) -> Rgba {
    let (w, h) = (img.width as usize, img.height as usize);
    let (new_w, new_h) = match degrees % 360 {
        0 => return img,
        180 => (w, h),
        _ => (h, w),
    };
    let mut pixels = vec![0u8; img.pixels.len()];
    for y in 0..h {
        for x in 0..w {
            let (nx, ny) = match degrees % 360 {
                90 => (h - 1 - y, x),
                180 => (w - 1 - x, h - 1 - y),
                _ => (y, w - 1 - x),
            };
            let (s, d) = ((y * w + x) * 4, (ny * new_w + nx) * 4);
            pixels[d..d + 4].copy_from_slice(&img.pixels[s..s + 4]);
        }
    }
    Rgba {
        width: new_w as u32,
        height: new_h as u32,
        pixels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::source::FileSource;

    fn src(name: &str) -> Arc<dyn ByteSource> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name);
        Arc::new(FileSource::open(&path).unwrap())
    }

    fn no_interrupt() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn grabs_left_eye_thumbnail() {
        let img = grab(
            src("h264_aac_180_LR.mp4"),
            "x.mp4",
            1.0,
            160,
            160,
            Crop::LeftHalf,
            no_interrupt(),
        )
        .unwrap();
        assert_eq!((img.width, img.height), (160, 160));
        assert_eq!(img.pixels.len(), 160 * 160 * 4);
        let distinct: std::collections::HashSet<_> = img
            .pixels
            .chunks(4)
            .map(|p| (p[0] / 32, p[1] / 32, p[2] / 32))
            .collect();
        assert!(distinct.len() > 10, "image looks blank");
    }

    /// A grab from a session of its own.
    fn grab_alone(at: f64) -> Rgba {
        grab(
            src("h264_aac_180_LR.mp4"),
            "x.mp4",
            at,
            160,
            160,
            Crop::Full,
            no_interrupt(),
        )
        .unwrap()
    }

    #[test]
    fn a_thumbnail_is_the_keyframe_before_its_time() {
        // Keyframes at 0 and 1 s, 30 frames apart.
        let (first, second) = (grab_alone(0.0), grab_alone(1.0));
        assert_ne!(first, second);
        assert_eq!(grab_alone(0.9), first);
        assert_eq!(grab_alone(1.9), second);
    }

    #[test]
    fn one_session_grabs_each_keyframe_once_where_it_can() {
        let mut g = Grabber::open(src("h264_aac_180_LR.mp4"), "x.mp4", no_interrupt()).unwrap();
        let mut at = |t| g.grab(t, 160, 160, Crop::Full).unwrap();
        let (first, second) = (grab_alone(0.0), grab_alone(1.0));
        assert_eq!(at(0.2), first);
        // The keyframe before 0.6 s was the last one: the next instead.
        assert_eq!(at(0.6), second);
        // No keyframe after 1.9 s: the one before it again.
        assert_eq!(at(1.9), second);
        assert_eq!(at(0.5), first, "back again");
        assert_eq!(g.info().video_stream().unwrap().width, 640);
    }

    #[test]
    fn grabs_av1_and_vp9() {
        for name in ["av1_opus.webm", "vp9.webm"] {
            let mut g = Grabber::open(src(name), name, no_interrupt()).unwrap();
            for at in [0.0, 0.5] {
                let img = g.grab(at, 160, 160, Crop::Full).unwrap();
                let distinct: std::collections::HashSet<_> = img
                    .pixels
                    .chunks(4)
                    .map(|p| (p[0] / 32, p[1] / 32, p[2] / 32))
                    .collect();
                assert!(distinct.len() > 10, "{name} at {at}: looks blank");
            }
        }
    }

    #[test]
    fn decodes_first_frame_full_size() {
        let f = decode_first_frame(src("h264_aac_180_LR.mp4"), "x.mp4").unwrap();
        assert_eq!((f.width, f.height), (640, 320));
    }

    #[test]
    fn grabs_10bit_top_half_and_probes() {
        let img = grab(
            src("hevc10_tb.mkv"),
            "x.mkv",
            0.5,
            256,
            256,
            Crop::TopHalf,
            no_interrupt(),
        )
        .unwrap();
        assert_eq!((img.width, img.height), (256, 128));
        let info = probe(src("av1_opus.webm"), "x.webm", no_interrupt()).unwrap();
        assert_eq!(info.video_stream().unwrap().codec, "av1");
    }

    #[test]
    fn portrait_and_anamorphic_thumbnails_look_as_shown() {
        let grab_full = |src, max| grab(src, "x.mp4", 1.0, max, max, Crop::Full, no_interrupt());
        let portrait = grab_full(crate::info::tests::portrait_clip(), 160).unwrap();
        assert_eq!((portrait.width, portrait.height), (80, 160));
        let wide = grab_full(crate::info::tests::anamorphic_clip(), 640).unwrap();
        assert_eq!((wide.width, wide.height), (640, 240));
    }

    #[test]
    fn a_full_side_by_side_eye_marked_1_2_is_square() {
        let marked = crate::info::tests::side_by_side_marked_1_2_clip;
        let left = grab(
            marked(),
            "x.mp4",
            1.0,
            160,
            160,
            Crop::LeftHalf,
            no_interrupt(),
        )
        .unwrap();
        assert_eq!((left.width, left.height), (160, 160));
        // The whole frame shows at one eye's shape, as in a 2D player.
        let full = grab(marked(), "x.mp4", 1.0, 160, 160, Crop::Full, no_interrupt()).unwrap();
        assert_eq!((full.width, full.height), (160, 160));
    }

    #[test]
    fn turns_pictures_clockwise() {
        // A red pixel left of a blue one.
        let (r, b) = ([255, 0, 0, 255], [0, 0, 255, 255]);
        let img = Rgba {
            width: 2,
            height: 1,
            pixels: [r, b].concat(),
        };
        let px = |img: &Rgba| img.pixels.chunks(4).map(<[u8]>::to_vec).collect::<Vec<_>>();
        let quarter = turned(img.clone(), 90);
        assert_eq!((quarter.width, quarter.height), (1, 2));
        assert_eq!(px(&quarter), [r.to_vec(), b.to_vec()], "red on top");
        assert_eq!(px(&turned(img.clone(), 180)), [b.to_vec(), r.to_vec()]);
        assert_eq!(
            px(&turned(img.clone(), 270)),
            [b.to_vec(), r.to_vec()],
            "blue on top"
        );
        assert_eq!(turned(img.clone(), 0), img);
    }

    #[test]
    fn interrupted_grab_fails() {
        let stop = Arc::new(AtomicBool::new(true));
        let r = grab(
            src("h264_aac_180_LR.mp4"),
            "x.mp4",
            1.0,
            160,
            160,
            Crop::Full,
            stop,
        );
        assert!(r.is_err());
    }

    /// What making one video's pictures costs, as the library worker does
    /// it: a probe, a 640 px thumbnail at 20 % and a 1280 px frame where it
    /// was left off (here at half way). Prints wall and CPU time.
    #[test]
    #[ignore = "a benchmark: THUMB_BENCH=video"]
    fn thumbnail_cost() {
        let path = std::env::var("THUMB_BENCH").expect("THUMB_BENCH");
        let src = || -> Arc<dyn ByteSource> {
            Arc::new(FileSource::open(std::path::Path::new(&path)).unwrap())
        };
        let cpu = || {
            // SAFETY: getrusage fills the struct.
            unsafe {
                let mut u: libc::rusage = std::mem::zeroed();
                libc::getrusage(libc::RUSAGE_SELF, &mut u);
                let t = |v: libc::timeval| v.tv_sec as f64 + v.tv_usec as f64 / 1e6;
                (t(u.ru_utime) + t(u.ru_stime), u.ru_maxrss)
            }
        };
        let (cpu0, _) = cpu();
        let start = std::time::Instant::now();
        let info = probe(src(), &path, no_interrupt()).unwrap();
        let d = info.duration;
        let mut g = Grabber::open(src(), &path, no_interrupt()).unwrap();
        let out = std::env::var("THUMB_OUT").ok();
        let save = |name: String, img: &Rgba| {
            if let Some(dir) = &out {
                let f = format!("{dir}/{name}_{}x{}.rgba", img.width, img.height);
                std::fs::write(f, &img.pixels).unwrap();
            }
        };
        save(
            "main".into(),
            &g.grab(d * 0.2, 640, 640, Crop::Full).unwrap(),
        );
        let mut g = Grabber::open(src(), &path, no_interrupt()).unwrap();
        save(
            "resume".into(),
            &g.grab(d * 0.5, 1280, 1280, Crop::Full).unwrap(),
        );
        let (cpu1, rss) = cpu();
        eprintln!(
            "{path}: {:.2} s wall, {:.2} s CPU, peak RSS {} MiB",
            start.elapsed().as_secs_f64(),
            cpu1 - cpu0,
            rss / 1024
        );
    }
}
