//! Frame sequential 3D: the eyes take turns, frame after frame, at twice the
//! film's frame rate. Any decoder can decode it, as nothing but the timing is
//! unusual. Each left frame and the right frame after it are put side by side
//! into one picture twice as wide, which the renderer shows as full
//! side-by-side 3D at the film's own frame rate.
//!
//! A frame's eye comes from its timestamp, so that a seek or a damaged frame
//! can't leave every later frame on the wrong eye as counting frames would.

use crate::decode::{Decoder, Frame};
use crate::input::Input;
use crate::mvc::side_by_side;
use crate::nal::Packing;
use crate::{Result, q2d};
use fp_ffmpeg_sys as ff;

/// A frame sequential stream's timing.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stream {
    /// The first frame's time, in the stream's time base.
    first: i64,
    /// How long each frame lasts, in the same units.
    frame: f64,
}

/// Whether stream `index` is labelled frame sequential, and if so whether
/// the right eye comes first: by a label in the container, else in the video
/// stream, else `FS3D` in the file name.
pub(crate) fn labelled(input: &Input, index: usize) -> Option<bool> {
    let st = *input.streams().get(index)?;
    // SAFETY: the stream and its parameters live as long as the input.
    let par = unsafe { &*(*st).codecpar };
    match (
        crate::info::stereo3d_side_data(par),
        input.frame_packing(index),
    ) {
        (Some(s), _) if s.type_ == ff::AV_STEREO3D_FRAMESEQUENCE => {
            Some(s.flags & ff::AV_STEREO3D_FLAG_INVERT as i32 != 0)
        }
        (None, Some((Packing::FrameSequential, right_first))) => Some(right_first),
        (None, None) if fp_core::format::names_frame_sequential(&input.name) => Some(false),
        _ => None,
    }
}

/// How stream `index` decodes as frame sequential, if it does: as labelled,
/// unless the viewer said otherwise ([`Input::set_frame_sequential`]). Stays
/// None for a stream that doesn't say how fast its frames come, as its eyes
/// can't then be told apart.
pub(crate) fn stream(input: &Input, index: usize) -> Option<Stream> {
    match input.frame_sequential() {
        Some(false) => return None,
        Some(true) => {}
        None => {
            labelled(input, index)?;
        }
    }
    let st = *input.streams().get(index)?;
    // SAFETY: the stream lives as long as the input.
    let st = unsafe { &*st };
    let rate = [st.r_frame_rate, st.avg_frame_rate]
        .map(q2d)
        .into_iter()
        .find(|r| (1.0..=480.0).contains(r))?;
    let frame = 1.0 / rate / q2d(st.time_base);
    let first = if st.start_time == ff::AV_NOPTS_VALUE {
        0
    } else {
        st.start_time
    };
    Some(Stream { first, frame })
}

impl Stream {
    /// Whether the frame at `pts` is the first eye's.
    fn first_eye(self, pts: i64) -> bool {
        let index = (pts.saturating_sub(self.first) as f64 / self.frame).round() as i64;
        index & 1 == 0
    }
}

/// A frame waiting for the other eye's, with its time.
struct Waiting {
    frame: Frame,
    pts: i64,
    duration: i64,
}

/// Decodes a frame sequential stream with `inner`, and pairs the eyes.
pub(crate) struct FrameSeqDecoder {
    pub(crate) inner: Decoder,
    stream: Stream,
    /// Decoding keyframes alone, for pictures of the video: there's no other
    /// eye to wait for, and the next keyframe may be seconds of reading away.
    stills: bool,
    left: Option<Waiting>,
    scratch: Frame,
}

impl FrameSeqDecoder {
    pub(crate) fn new(inner: Decoder, stream: Stream, stills: bool) -> FrameSeqDecoder {
        FrameSeqDecoder {
            inner,
            stream,
            stills,
            left: None,
            scratch: Frame::new(),
        }
    }

    /// Receives a side-by-side frame, as [`Decoder::receive`].
    pub(crate) fn receive(&mut self, frame: &mut Frame) -> Result<Option<bool>> {
        loop {
            match self.inner.receive(&mut self.scratch)? {
                None => return Ok(None),
                Some(false) => {
                    // The end: a left frame still waiting goes out alone.
                    if let Some(left) = self.left.take() {
                        pair(frame, &left, &left.frame)?;
                        return Ok(Some(true));
                    }
                    return Ok(Some(false));
                }
                Some(true) => {}
            }
            // SAFETY: a decoded frame.
            let (pts, duration) = unsafe {
                let f = &*self.scratch.as_ptr();
                (f.best_effort_timestamp, f.duration)
            };
            let decoded = Waiting {
                frame: Frame(self.scratch.take_raw()),
                pts,
                duration,
            };
            if self.stills {
                pair(frame, &decoded, &decoded.frame)?;
                return Ok(Some(true));
            }
            if self.stream.first_eye(pts) {
                // A left frame whose right never came goes out alone.
                if let Some(lone) = self.left.replace(decoded) {
                    pair(frame, &lone, &lone.frame)?;
                    return Ok(Some(true));
                }
            } else if let Some(left) = self.left.take() {
                if (pts.saturating_sub(left.pts) as f64) < self.stream.frame * 1.5 {
                    pair(frame, &left, &decoded.frame)?;
                    return Ok(Some(true));
                }
                self.left = Some(left);
            }
        }
    }

    pub(crate) fn flush(&mut self) {
        self.left = None;
        self.inner.flush();
    }
}

/// Puts `left` and `right` side by side into `out`, lasting for both eyes.
fn pair(out: &mut Frame, left: &Waiting, right: &Frame) -> Result<()> {
    // SAFETY: both are decoded frames of the same stream; out is ours to fill.
    unsafe {
        side_by_side(out.as_ptr(), left.frame.as_ptr(), right.as_ptr())?;
        let o = &mut *out.as_ptr();
        o.pts = left.pts;
        o.best_effort_timestamp = left.pts;
        o.duration = left.duration * 2;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::HwDecode;
    use crate::info::read_info;
    use crate::info::tests::{fixture, open};
    use fp_core::format::StereoLayout;
    use fp_core::source::FileSource;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn a_frame_sequential_file_is_found_by_its_labels() {
        for name in ["fs3d_block_lr.mkv", "fs3d_sei.mp4"] {
            let info = read_info(&open(name));
            let v = info.video_stream().unwrap();
            assert!(v.frame_sequential, "{name}");
            assert_eq!((v.width, v.height), (128, 36), "{name}");
            assert!((v.fps - 24.0).abs() < 0.01, "{name}: {}", v.fps);
            assert_eq!(
                info.hints.stereo,
                Some(StereoLayout::FrameSequential),
                "{name}"
            );
            assert!(!info.hints.eyes_swapped, "{name}");
        }
        assert!(read_info(&open("fs3d_block_rl.mkv")).hints.eyes_swapped);
    }

    #[test]
    fn a_file_name_can_say_frame_sequential() {
        let named = |fixture_name: &str, name: &str| {
            let src = Arc::new(FileSource::open(&fixture(fixture_name)).unwrap());
            let input = Input::open(src, name, Arc::new(AtomicBool::new(false))).unwrap();
            let info = read_info(&input);
            info.video_stream().unwrap().frame_sequential
        };
        assert!(named("vp9.webm", "clip (FS3D).webm"));
        assert!(!named("vp9.webm", "clip.webm"));
        // A label in the file comes before its name.
        assert!(!named("camera_clip.mp4", "clip (FS3D).mp4"));
    }

    #[test]
    fn the_viewer_can_say_whether_a_file_is_frame_sequential() {
        let mut labelled = open("fs3d_block_lr.mkv");
        labelled.set_frame_sequential(Some(false));
        let info = read_info(&labelled);
        let v = info.video_stream().unwrap();
        assert!(!v.frame_sequential);
        assert_eq!(v.width, 64);
        // What its label says is still what it's detected as.
        assert_eq!(info.hints.stereo, Some(StereoLayout::FrameSequential));

        let mut plain = open("vp9.webm");
        plain.set_frame_sequential(Some(true));
        let vi = read_info(&plain).video_stream().unwrap().index;
        assert!(read_info(&plain).video_stream().unwrap().frame_sequential);
        let dec = Decoder::open(&plain, vi, HwDecode::Off).unwrap();
        assert!(dec.name.contains("frame sequential"), "{}", dec.name);
    }

    #[test]
    fn a_plain_3d_file_is_not_frame_sequential() {
        let info = read_info(&open("h264_sei_sbs.mp4"));
        assert!(!info.video_stream().unwrap().frame_sequential);
    }

    #[test]
    fn eyes_come_from_timestamps() {
        let s = Stream {
            first: 1000,
            frame: 1000.0 / 48.0,
        };
        // Rounded to the millisecond, as Matroska's are, three hours in.
        let at = |n: i64| 1000 + (n as f64 * s.frame).round() as i64;
        for n in [0, 1, 2, 3, 4, 45, 46, 518_400, 518_401] {
            assert_eq!(s.first_eye(at(n)), n % 2 == 0, "frame {n}");
        }
    }

    /// Each frame's average brightness in its left and right halves.
    fn sides(frame: &Frame) -> (f64, f64) {
        // SAFETY: a decoded frame, its first plane holding height rows.
        unsafe {
            let f = &*frame.as_ptr();
            let (w, h) = (f.width as usize, f.height as usize);
            let (mut left, mut right) = (0u64, 0u64);
            for y in 0..h {
                let row = std::slice::from_raw_parts(
                    f.data[0].offset(y as isize * f.linesize[0] as isize),
                    w,
                );
                left += row[..w / 2].iter().map(|&b| u64::from(b)).sum::<u64>();
                right += row[w / 2..].iter().map(|&b| u64::from(b)).sum::<u64>();
            }
            let n = (w / 2 * h) as f64;
            (left as f64 / n, right as f64 / n)
        }
    }

    /// Decodes a fixture whose left eye is dark and right eye bright, giving
    /// each picture's time, whether it holds the two eyes in order, and its
    /// width. `lost` is a packet that never reaches the decoder.
    fn decode(name: &str, skip_to: Option<f64>, lost: Option<usize>) -> Vec<(f64, bool, i32)> {
        let mut input = open(name);
        let vi = read_info(&input).video_stream().unwrap().index;
        let mut dec = Decoder::open(&input, vi, HwDecode::Off).unwrap();
        if let Some(target) = skip_to {
            dec.skip_before(target, 1.0 / 24.0);
        }
        let (mut pkt, mut frame) = (crate::decode::Packet::new(), Frame::new());
        let mut out = Vec::new();
        let mut ended = false;
        let mut packets = 0;
        loop {
            match dec.receive(&mut frame).unwrap() {
                Some(true) => {
                    let (left, right) = sides(&frame);
                    // SAFETY: a decoded frame.
                    let width = unsafe { (*frame.as_ptr()).width };
                    out.push((dec.frame_time(&frame).unwrap(), left < right, width));
                    continue;
                }
                Some(false) => return out,
                None => {}
            }
            if !ended && input.read(pkt.as_ptr()).unwrap() {
                if pkt.stream_index() == vi && lost != Some(packets) {
                    dec.send(pkt.as_ptr()).unwrap();
                }
                packets += 1;
                pkt.unref();
            } else if !ended {
                ended = true;
                dec.send(std::ptr::null()).unwrap();
            }
        }
    }

    #[test]
    fn each_left_frame_is_paired_with_the_right_after_it() {
        for name in ["fs3d_block_lr.mkv", "fs3d_sei.mp4"] {
            let pictures = decode(name, None, None);
            assert_eq!(pictures.len(), 24, "{name}");
            for (n, &(time, in_order, width)) in pictures.iter().enumerate() {
                assert!(in_order, "{name}: picture {n}");
                assert_eq!(width, 128, "{name}");
                assert!((time - n as f64 / 24.0).abs() < 0.002, "{name}: {time}");
            }
        }
    }

    #[test]
    fn a_precise_seek_lands_on_the_pair_showing_its_target() {
        let target = 12.0 / 24.0 + 0.01;
        let pictures = decode("fs3d_block_lr.mkv", Some(target), None);
        let (first, in_order, _) = pictures[0];
        assert!((first - 12.0 / 24.0).abs() < 0.002, "{first}");
        assert!(in_order);
        assert!(pictures.iter().all(|p| p.1));
    }

    #[test]
    fn a_lost_frame_leaves_the_later_eyes_right() {
        // The third packet is the first right frame, as pictures reorder.
        let pictures = decode("fs3d_block_lr.mkv", None, Some(2));
        assert_eq!(pictures.len(), 24);
        assert!(!pictures[0].1, "the left frame goes out alone");
        assert!(pictures[1..].iter().all(|p| p.1));
    }

    /// Reads until `dec` gives a picture, and how many packets that took.
    fn first_picture(dec: &mut Decoder, input: &mut Input) -> (Frame, usize) {
        let (mut pkt, mut frame) = (crate::decode::Packet::new(), Frame::new());
        let mut packets = 0;
        while dec.receive(&mut frame).unwrap() != Some(true) {
            assert!(input.read(pkt.as_ptr()).unwrap(), "no picture");
            dec.send(pkt.as_ptr()).unwrap();
            pkt.unref();
            packets += 1;
        }
        (frame, packets)
    }

    #[test]
    fn a_still_is_one_eye_twice_without_waiting_for_the_other() {
        let name = "fs3d_block_lr.mkv";
        let mut input = open(name);
        let vi = read_info(&input).video_stream().unwrap().index;
        let mut dec = Decoder::open(&input, vi, HwDecode::Stills).unwrap();
        let (frame, packets) = first_picture(&mut dec, &mut input);
        // SAFETY: a decoded frame.
        assert_eq!(unsafe { (*frame.as_ptr()).width }, 128);
        let (left, right) = sides(&frame);
        assert!((left - right).abs() < 1.0, "{left} {right}");

        let mut input = open(name);
        let st = input.streams()[vi];
        // SAFETY: the stream belongs to the open input.
        let (par, time_base) = unsafe { ((*st).codecpar, (*st).time_base) };
        let mut plain = Decoder::open_with(par, time_base, HwDecode::Stills, false).unwrap();
        assert_eq!(packets, first_picture(&mut plain, &mut input).1);
    }
}
