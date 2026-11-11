//! Deinterlacing. An interlaced frame (1080i TV and camcorders, DVDs) holds
//! two fields, alternate lines taken half a frame apart, so anything moving
//! shows combed edges. FFmpeg's bwdif filter makes each field a whole
//! picture, filling the missing lines from its neighbours in the frame and
//! in the fields before and after, so the video plays at twice the frame
//! rate (25 frames a second of 1080i50 become 50 pictures).
//!
//! The pictures come out in a time base half the stream's (bwdif's own):
//! the field between two frames has a time of its own.

use crate::decode::{Decoder, Frame, Skip};
use crate::input::Input;
use crate::{Error, Result, check};
use fp_ffmpeg_sys as ff;

/// Which frames are deinterlaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Frames {
    /// Those the decoder marks interlaced; the rest go out whole.
    Marked,
    /// Every one: the viewer says the video is interlaced, which its
    /// frames don't say.
    All,
}

/// Which frames of stream `index` of `input` are deinterlaced, if any:
/// those marked interlaced when its headers say it is coded as fields,
/// every one when the viewer says it is interlaced
/// ([`Input::set_deinterlace`]). None for 3D Blu-ray and frame sequential
/// 3D, which pair whole frames.
pub(crate) fn wanted(input: &Input, index: usize) -> Option<Frames> {
    let &st = input.streams().get(index)?;
    // SAFETY: the stream and its parameters live as long as the input.
    let par = unsafe { &*(*st).codecpar };
    if par.codec_type != ff::AVMEDIA_TYPE_VIDEO
        || crate::mvc::stream_config(input, index).is_some()
        || crate::frame_seq::stream(input, index).is_some()
    {
        return None;
    }
    match input.deinterlace() {
        Some(true) => Some(Frames::All),
        Some(false) => None,
        None => crate::info::interlaced(par).then_some(Frames::Marked),
    }
}

/// A field's time base: the stream's halved.
pub(crate) fn field_time_base(tb: ff::AVRational) -> ff::AVRational {
    let mut out = tb;
    // SAFETY: plain arithmetic on the out-parameters.
    let exact = unsafe {
        ff::av_reduce(
            &mut out.num,
            &mut out.den,
            tb.num.into(),
            i64::from(tb.den) * 2,
            i32::MAX.into(),
        )
    };
    if exact == 0 { tb } else { out }
}

/// A buffer source, bwdif and a buffer sink, made for one picture size and
/// pixel format.
struct Graph {
    graph: *mut ff::AVFilterGraph,
    src: *mut ff::AVFilterContext,
    sink: *mut ff::AVFilterContext,
    shape: (i32, i32, i32),
}

impl Drop for Graph {
    fn drop(&mut self) {
        // SAFETY: we own the graph, which frees its filters (null-safe).
        unsafe { ff::avfilter_graph_free(&mut self.graph) };
    }
}

impl Graph {
    /// A graph deinterlacing `frames` of those like `f`, timed in
    /// `time_base`.
    fn new(f: &ff::AVFrame, time_base: ff::AVRational, frames: Frames) -> Result<Graph> {
        // SAFETY: the filters are created in, and freed with, the graph,
        // which Drop frees on every early return.
        unsafe {
            let mut g = Graph {
                graph: ff::avfilter_graph_alloc(),
                src: std::ptr::null_mut(),
                sink: std::ptr::null_mut(),
                shape: (f.width, f.height, f.format),
            };
            if g.graph.is_null() {
                return Err(Error::Unsupported("filter graph".into()));
            }
            let filter = |name: &std::ffi::CStr| {
                let f = ff::avfilter_get_by_name(name.as_ptr());
                if f.is_null() {
                    Err(Error::Unsupported(format!("no {name:?} filter")))
                } else {
                    Ok(f)
                }
            };
            g.src = ff::avfilter_graph_alloc_filter(g.graph, filter(c"buffer")?, c"in".as_ptr());
            if g.src.is_null() {
                return Err(Error::Unsupported("buffer source".into()));
            }
            let par = ff::av_buffersrc_parameters_alloc();
            if par.is_null() {
                return Err(Error::Unsupported("buffer source parameters".into()));
            }
            (*par).format = f.format;
            (*par).time_base = time_base;
            (*par).width = f.width;
            (*par).height = f.height;
            (*par).sample_aspect_ratio = f.sample_aspect_ratio;
            (*par).color_space = f.colorspace;
            (*par).color_range = f.color_range;
            let r = ff::av_buffersrc_parameters_set(g.src, par);
            ff::av_free(par.cast());
            check(r, "deinterlace input")?;
            check(
                ff::avfilter_init_str(g.src, std::ptr::null()),
                "deinterlace input",
            )?;
            // Each field a picture; its order from the frame, top first when
            // a frame doesn't say.
            let options = match frames {
                Frames::Marked => c"mode=send_field:parity=auto:deint=interlaced",
                Frames::All => c"mode=send_field:parity=auto:deint=all",
            };
            let mut bwdif = std::ptr::null_mut();
            check(
                ff::avfilter_graph_create_filter(
                    &mut bwdif,
                    filter(c"bwdif")?,
                    c"bwdif".as_ptr(),
                    options.as_ptr(),
                    std::ptr::null_mut(),
                    g.graph,
                ),
                "bwdif",
            )?;
            check(
                ff::avfilter_graph_create_filter(
                    &mut g.sink,
                    filter(c"buffersink")?,
                    c"out".as_ptr(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    g.graph,
                ),
                "deinterlace output",
            )?;
            check(ff::avfilter_link(g.src, 0, bwdif, 0), "deinterlace link")?;
            check(ff::avfilter_link(bwdif, 0, g.sink, 0), "deinterlace link")?;
            // bwdif splits each picture across threads started here.
            check(
                crate::decode::on_all_cpus(|| {
                    ff::avfilter_graph_config(g.graph, std::ptr::null_mut())
                }),
                "deinterlace setup",
            )?;
            Ok(g)
        }
    }
}

/// Decodes an interlaced stream with `inner` and deinterlaces its frames.
pub(crate) struct Deinterlacer {
    pub(crate) inner: Decoder,
    /// The stream's time base, which `inner` times frames in.
    stream_time_base: ff::AVRational,
    /// A field's, which pictures come out timed in.
    time_base: ff::AVRational,
    frames: Frames,
    graph: Option<Graph>,
    /// bwdif can't take this video (its pixel format): frames go out as
    /// decoded.
    off: bool,
    /// The end of the stream has gone into the graph.
    ended: bool,
    /// What a precise seek leaves out, in fields.
    pub(crate) skip: Option<Skip>,
    scratch: Frame,
}

impl Deinterlacer {
    pub(crate) fn new(inner: Decoder, frames: Frames) -> Deinterlacer {
        Deinterlacer {
            stream_time_base: inner.time_base,
            time_base: field_time_base(inner.time_base),
            frames,
            inner,
            graph: None,
            off: false,
            ended: false,
            skip: None,
            scratch: Frame::new(),
        }
    }

    /// The time base pictures come out in.
    pub(crate) fn time_base(&self) -> ff::AVRational {
        self.time_base
    }

    /// Receives a picture, as [`Decoder::receive`].
    pub(crate) fn receive(&mut self, frame: &mut Frame) -> Result<Option<bool>> {
        loop {
            if let Some(g) = &self.graph {
                // SAFETY: a configured graph; frame is ours to fill, emptied
                // first as the sink moves its picture in.
                let r = unsafe {
                    ff::av_frame_unref(frame.as_ptr());
                    ff::av_buffersink_get_frame(g.sink, frame.as_ptr())
                };
                if r >= 0 {
                    // SAFETY: as above.
                    let from = unsafe { ff::av_buffersink_get_time_base(g.sink) };
                    retime(frame, from, self.time_base);
                    if self.leaves_out(frame) {
                        continue;
                    }
                    return Ok(Some(true));
                }
                if r == ff::AVERROR_EOF_ {
                    self.graph = None;
                    return Ok(Some(false));
                }
                if r != ff::AVERROR_EAGAIN {
                    check(r, "deinterlace")?;
                }
            }
            if self.ended {
                return Ok(Some(false));
            }
            match self.inner.receive(&mut self.scratch)? {
                None => return Ok(None),
                Some(false) => {
                    self.ended = true;
                    if let Some(g) = &self.graph {
                        // SAFETY: a null frame ends the graph's input.
                        let r = unsafe {
                            ff::av_buffersrc_add_frame_flags(g.src, std::ptr::null_mut(), 0)
                        };
                        check(r, "deinterlace end")?;
                    }
                }
                Some(true) => {
                    if self.off || !self.filter()? {
                        std::mem::swap(frame, &mut self.scratch);
                        retime(frame, self.stream_time_base, self.time_base);
                        if self.leaves_out(frame) {
                            continue;
                        }
                        return Ok(Some(true));
                    }
                }
            }
        }
    }

    /// Sends the decoded frame in `scratch` into the graph, made for it when
    /// it's the first or its size or format changed. False when bwdif can't
    /// take it, which turns deinterlacing off.
    fn filter(&mut self) -> Result<bool> {
        // SAFETY: a decoded frame.
        let f = unsafe { &mut *self.scratch.as_ptr() };
        f.pts = f.best_effort_timestamp;
        if self
            .graph
            .as_ref()
            .is_none_or(|g| g.shape != (f.width, f.height, f.format))
        {
            self.graph = None;
            match Graph::new(f, self.stream_time_base, self.frames) {
                Ok(g) => self.graph = Some(g),
                Err(e) => {
                    log::warn!("not deinterlacing: {e}");
                    self.off = true;
                    return Ok(false);
                }
            }
        }
        let Some(g) = &self.graph else {
            return Ok(false);
        };
        // SAFETY: the graph takes the frame's reference, leaving it blank.
        let r = unsafe { ff::av_buffersrc_add_frame_flags(g.src, f, 0) };
        check(r, "deinterlace")?;
        Ok(true)
    }

    /// Whether a precise seek leaves `frame` out (and unreferences it).
    fn leaves_out(&mut self, frame: &mut Frame) -> bool {
        let Some(s) = self.skip else {
            return false;
        };
        // SAFETY: a picture we hold.
        unsafe {
            let f = &*frame.as_ptr();
            if s.drops(f.pts, f.duration) {
                ff::av_frame_unref(frame.as_ptr());
                return true;
            }
        }
        self.skip = None;
        false
    }

    pub(crate) fn flush(&mut self) {
        self.graph = None;
        self.ended = false;
        self.skip = None;
        self.inner.flush();
    }
}

/// Moves a picture's times from time base `from` to `to`.
fn retime(frame: &mut Frame, from: ff::AVRational, to: ff::AVRational) {
    // SAFETY: a picture we hold; rescaling is arithmetic.
    unsafe {
        let f = &mut *frame.as_ptr();
        if (from.num, from.den) != (to.num, to.den) {
            if f.pts != ff::AV_NOPTS_VALUE {
                f.pts = ff::av_rescale_q(f.pts, from, to);
            }
            f.duration = ff::av_rescale_q(f.duration, from, to);
        }
        f.best_effort_timestamp = f.pts;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{HwDecode, Packet};
    use crate::info::read_info;
    use crate::info::tests::open;

    /// 10 frames of 128x96 at 25 a second, coded as fields (MBAFF, top
    /// first).
    const CLIP: &str = "interlaced_h264.mp4";

    /// The times of up to `max` pictures `dec` decodes from stream `vi` of
    /// `input`.
    fn times(input: &mut Input, vi: usize, dec: &mut Decoder, max: usize) -> Vec<f64> {
        let (mut pkt, mut frame) = (Packet::new(), Frame::new());
        let (mut out, mut ended) = (Vec::new(), false);
        while out.len() < max {
            match dec.receive(&mut frame).unwrap() {
                Some(true) => {
                    out.push(dec.frame_time(&frame).unwrap());
                    continue;
                }
                Some(false) => break,
                None => assert!(!ended, "no end after the stream's"),
            }
            if input.read(pkt.as_ptr()).unwrap() {
                if pkt.stream_index() == vi {
                    dec.send(pkt.as_ptr()).unwrap();
                }
                pkt.unref();
            } else {
                ended = true;
                dec.send(std::ptr::null()).unwrap();
            }
        }
        out
    }

    /// The decoder's name and every picture's time, decoding the video of
    /// `input` from a precise seek to `skip_to` when given.
    fn decode(mut input: Input, skip_to: Option<f64>) -> (String, Vec<f64>) {
        let vi = read_info(&input).video_stream().unwrap().index;
        let mut dec = Decoder::open(&input, vi, HwDecode::Auto).unwrap();
        if let Some(target) = skip_to {
            dec.skip_before(target, 1.0 / 50.0);
        }
        let times = times(&mut input, vi, &mut dec, usize::MAX);
        (dec.name.clone(), times)
    }

    #[test]
    fn a_stream_coded_as_fields_is_found_by_its_parameter_sets() {
        // FFmpeg finds this clip's field order in its first picture, but
        // none in camcorder video coded as separate fields.
        let input = open(CLIP);
        // SAFETY: the stream belongs to the open input, not yet read.
        unsafe { (*(*input.streams()[0]).codecpar).field_order = ff::AV_FIELD_UNKNOWN };
        let info = read_info(&input);
        let v = info.video_stream().unwrap();
        assert!(v.interlaced && v.deinterlaced);
        assert!((v.fps - 50.0).abs() < 0.01, "{}", v.fps);
        assert_eq!(decode(input, None).1.len(), 20);
        let info = read_info(&open("h264_aac_180_LR.mp4"));
        let v = info.video_stream().unwrap();
        assert!(!v.interlaced && !v.deinterlaced);
    }

    #[test]
    fn each_field_becomes_a_picture() {
        let (name, times) = decode(open(CLIP), None);
        assert!(name.ends_with("(deinterlaced)"), "{name}");
        assert_eq!(times.len(), 20);
        for (n, t) in times.iter().enumerate() {
            assert!((t - n as f64 / 50.0).abs() < 0.002, "picture {n} at {t}");
        }
    }

    #[test]
    fn the_viewer_can_say_whether_a_video_is_interlaced() {
        let mut input = open(CLIP);
        input.set_deinterlace(Some(false));
        let info = read_info(&input);
        let v = info.video_stream().unwrap();
        assert!(v.interlaced && !v.deinterlaced);
        assert!((v.fps - 25.0).abs() < 0.01, "{}", v.fps);
        assert_eq!(decode(input, None).1.len(), 10);

        let mut input = open("h264_aac_180_LR.mp4");
        input.set_deinterlace(Some(true));
        assert!(read_info(&input).video_stream().unwrap().deinterlaced);
        assert_eq!(decode(input, None).1.len(), 120);
    }

    #[test]
    fn found_by_its_headers_only_marked_frames_are_deinterlaced() {
        // Progressive frames, under a header saying interlaced.
        let input = open("h264_aac_180_LR.mp4");
        // SAFETY: the stream belongs to the open input, not yet read.
        unsafe { (*(*input.streams()[0]).codecpar).field_order = ff::AV_FIELD_TT };
        assert!(read_info(&input).video_stream().unwrap().deinterlaced);
        assert_eq!(decode(input, None).1.len(), 60);
    }

    #[test]
    fn a_precise_seek_lands_on_the_field_showing_its_target() {
        let (_, times) = decode(open(CLIP), Some(5.0 / 50.0 + 0.005));
        assert_eq!(times.len(), 15);
        assert!((times[0] - 5.0 / 50.0).abs() < 0.002, "{}", times[0]);
    }

    #[test]
    fn a_flush_starts_the_fields_again() {
        let mut input = open(CLIP);
        let vi = read_info(&input).video_stream().unwrap().index;
        let mut dec = Decoder::open(&input, vi, HwDecode::Off).unwrap();
        assert_eq!(times(&mut input, vi, &mut dec, 3).len(), 3);
        dec.flush();
        input.seek(0.0).unwrap();
        let again = times(&mut input, vi, &mut dec, usize::MAX);
        assert_eq!(again.len(), 20);
        assert!(again[0].abs() < 0.002, "{}", again[0]);
    }
}
