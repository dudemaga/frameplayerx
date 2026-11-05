//! 3D Blu-ray video (H.264 MVC: a base view, the left eye, and a second
//! view predicted from it) decoded by an ordinary H.264 decoder, the Steam
//! Frame's among them, which has no MVC support.
//!
//! Each access unit is rewritten, without decoding it, into plain H.264
//! pictures in turn: the base view's, then the second view's, numbered as
//! one stream twice the frame rate ([`Rewriter`]). The slice data is kept
//! as it is; the slice headers are rewritten so the H.264 decoding process
//! gives the very pictures MVC's does:
//!
//! - every reference picture list is spelled out, picture by picture, as
//!   MVC builds it (each view from its own pictures, the second view also
//!   from the base picture of its access unit);
//! - reference marking keeps exactly the pictures each view's own marking
//!   keeps;
//! - a non-reference base picture the second view predicts from is kept as
//!   a long-term reference until that is decoded. A long-term reference
//!   counts as neither short-term nor inter-view where H.264 tells them
//!   apart in what the rewrite allows (spatial direct prediction's
//!   colZeroFlag), as an inter-view one does in MVC; when the inter-view
//!   reference is also a short-term reference of the base view, a copy of
//!   it (all macroblocks skipped) stands in, decoded but not shown.
//!
//! The rewrite gives up, and the left eye plays alone, on what makes the
//! pictures depend on picture order counts or long-term references (temporal
//! direct prediction, implicit weighted prediction, long-term references,
//! MMCO 5), on fields, on more than two views and on views that decode
//! their samples differently. None of that was seen on 3D Blu-ray: Blu-ray
//! 3D is progressive, and every disc tried decoded bit-exactly (each view
//! compared with edge264-mvc's) through FFmpeg's H.264 decoder.
//!
//! Both eyes are put side by side into one picture twice as wide, which the
//! renderer shows as full side-by-side 3D.

pub(crate) mod bits;
mod rewrite;
mod syntax;

use crate::decode::{Decoder, Frame, Packet, Skip};
use crate::input::Input;
use crate::{Error, Result, check};
use fp_ffmpeg_sys as ff;
use rewrite::{Rewriter, Slot};
use std::collections::{HashMap, VecDeque};

/// Why a stream, or one of its access units, can't be rewritten.
#[derive(Debug)]
pub(crate) enum Bad {
    /// Something the rewrite doesn't handle: the stream plays as 2D.
    Unsupported(String),
    /// This access unit can't be rewritten (damaged, or predicting from a
    /// picture never decoded): the rewrite starts again at the next intra
    /// picture.
    Broken(&'static str),
}

/// The parameter sets of an AVC (or MVC) decoder configuration record, and
/// its NAL unit length size.
fn record_parameter_sets(record: &[u8], sps_count_mask: u8) -> Option<(usize, Vec<&[u8]>)> {
    let [1, _, _, _, size, count, ref rest @ ..] = *record else {
        return None;
    };
    let mut rest: &[u8] = rest;
    let mut nals = Vec::new();
    take_units(&mut rest, count & sps_count_mask, &mut nals)?;
    let (&pps, mut rest) = rest.split_first()?;
    take_units(&mut rest, pps, &mut nals)?;
    Some((usize::from(size & 3) + 1, nals))
}

/// `n` units with 16-bit lengths off the front of `rest`.
fn take_units<'a>(rest: &mut &'a [u8], n: u8, out: &mut Vec<&'a [u8]>) -> Option<()> {
    for _ in 0..n {
        let [hi, lo, ref tail @ ..] = **rest else {
            return None;
        };
        let len = usize::from(u16::from_be_bytes([hi, lo]));
        out.push(tail.get(..len)?);
        *rest = &tail[len..];
    }
    Some(())
}

/// An H.264 stream's MVC configuration (Matroska and MP4 keep an `mvcC` box
/// after the `avcC` record in the extradata): the NAL unit length size, the
/// base view's parameter sets and those of the `mvcC` box. None for other
/// streams.
pub(crate) struct Config<'a> {
    length_size: usize,
    base: Vec<&'a [u8]>,
    mvc: Vec<&'a [u8]>,
}

pub(crate) fn config(codec: ff::AVCodecID, extradata: &[u8]) -> Option<Config<'_>> {
    if codec != ff::AV_CODEC_ID_H264 {
        return None;
    }
    let (length_size, base) = record_parameter_sets(extradata, 0x1f)?;
    let at = (8..=extradata.len().saturating_sub(4)).find(|&i| {
        &extradata[i..i + 4] == b"mvcC"
            && u32::from_be_bytes(extradata[i - 4..i].try_into().unwrap_or_default()) as usize
                <= extradata.len() - (i - 4)
    })?;
    let size = u32::from_be_bytes(extradata[at - 4..at].try_into().ok()?) as usize;
    let (_, mvc) = record_parameter_sets(extradata.get(at + 4..at - 4 + size)?, 0x7f)?;
    Some(Config {
        length_size,
        base,
        mvc,
    })
}

/// The MVC configuration of stream `index` of `input`: from its `mvcC` box,
/// or its base view's alone when its packets carry the second view's
/// parameter sets. None when the stream isn't MVC.
pub(crate) fn stream_config(input: &Input, index: usize) -> Option<Config<'_>> {
    let st = *input.streams().get(index)?;
    // SAFETY: the stream and its parameters live as long as the input.
    let par = unsafe { &*(*st).codecpar };
    let data = extradata(par);
    if let Some(c) = config(par.codec_id, data) {
        return Some(c);
    }
    if par.codec_id != ff::AV_CODEC_ID_H264 || !input.mvc_in_band(index) {
        return None;
    }
    let length_size = length_size(data);
    let base = match record_parameter_sets(data, 0x1f) {
        Some((_, base)) => base,
        None => crate::nal::units(data, 0).collect(),
    };
    Some(Config {
        length_size,
        base,
        mvc: Vec::new(),
    })
}

/// Bytes in each NAL unit's length prefix, by an H.264 stream's extradata:
/// its avcC record's, or 0 for Annex B.
pub(crate) fn length_size(extradata: &[u8]) -> usize {
    record_parameter_sets(extradata, 0x1f).map_or(0, |(size, _)| size)
}

pub(crate) fn extradata(par: &ff::AVCodecParameters) -> &[u8] {
    if par.extradata.is_null() || par.extradata_size <= 0 {
        &[]
    } else {
        // SAFETY: extradata holds extradata_size bytes while par lives.
        unsafe { std::slice::from_raw_parts(par.extradata, par.extradata_size as usize) }
    }
}

/// What a merged picture is, by the tag it is decoded under.
#[derive(Clone, Copy, Debug)]
struct Tag {
    pts: i64,
    duration: i64,
    au: u64,
    /// None: the base view decoded alone (2D).
    slot: Option<Slot>,
}

/// An access unit's NAL units as they arrive, timed by the packet its base
/// picture starts in.
#[derive(Clone)]
struct Unit {
    nals: Vec<Vec<u8>>,
    pts: i64,
    duration: i64,
    key: bool,
    /// Whether it has base view slices, and the second view's.
    base: bool,
    second: bool,
}

/// first_mb_in_slice of a base view slice.
fn first_mb(nal: &[u8]) -> Option<u32> {
    let head = bits::unescape(&nal[1..nal.len().min(16)]);
    bits::Reader::new(&head, 0).ue().ok()
}

/// Decodes MVC with a plain H.264 decoder: rewrites each packet into
/// merged pictures, and pairs the views decoded into side-by-side frames.
pub(crate) struct MvcDecoder {
    pub(crate) inner: Decoder,
    rewriter: Rewriter,
    length_size: usize,
    /// The base view's parameter sets, Annex B, for decoding it alone.
    base_parameter_sets: Vec<u8>,
    /// Why the stream plays as 2D, its base view on both sides.
    pub(crate) two_d: Option<String>,
    /// In 2D, waiting for a keyframe to start at.
    wait_key: bool,
    /// Merged pictures the decoder hasn't taken yet.
    queue: VecDeque<Packet>,
    /// Each merged picture's tag (its pts while decoded) and what it is.
    tags: HashMap<i64, Tag>,
    next_tag: i64,
    aus: u64,
    /// The access unit being gathered.
    unit: Option<Unit>,
    /// The access units since the last keyframe, to decode again in 2D
    /// when the rewrite gives up.
    history: Vec<Unit>,
    /// After going 2D, the frames shown up to this pts are not shown again.
    shown: Option<i64>,
    /// A base view frame waiting for its second view.
    left: Option<(Frame, Tag)>,
    scratch: Frame,
    pub(crate) skip: Option<Skip>,
}

impl MvcDecoder {
    /// Wraps `inner`, a decoder opened for the merged stream (Annex B, no
    /// extradata), for a stream of this configuration.
    pub(crate) fn new(inner: Decoder, config: &Config) -> MvcDecoder {
        let mut rewriter = Rewriter::new();
        let mut two_d = None;
        // The second view's SPS first, so the merged one makes room for
        // both views' references.
        let mut sets: Vec<&[u8]> = config.mvc.iter().chain(&config.base).copied().collect();
        sets.sort_by_key(|n| match n.first().map(|h| h & 0x1f) {
            Some(15) => 0,
            Some(7) => 1,
            _ => 2,
        });
        for nal in sets {
            match rewriter.parameter_set(nal) {
                Ok(()) => {}
                Err(Bad::Unsupported(why)) => {
                    two_d.get_or_insert(why);
                }
                Err(Bad::Broken(why)) => log::debug!("MVC configuration: {why}"),
            }
        }
        let mut base_parameter_sets = Vec::new();
        for nal in &config.base {
            base_parameter_sets.extend_from_slice(&[0, 0, 0, 1]);
            base_parameter_sets.extend_from_slice(nal);
        }
        if let Some(why) = &two_d {
            log::warn!("3D Blu-ray video (MVC): {why}; playing its left eye alone");
        }
        MvcDecoder {
            inner,
            rewriter,
            length_size: config.length_size,
            base_parameter_sets,
            wait_key: two_d.is_some(),
            two_d,
            queue: VecDeque::new(),
            tags: HashMap::new(),
            next_tag: 0,
            aus: 0,
            unit: None,
            history: Vec::new(),
            shown: None,
            left: None,
            scratch: Frame::new(),
            skip: None,
        }
    }

    /// Sends a packet (null: the end), as [`Decoder::send`].
    pub(crate) fn send(&mut self, pkt: *const ff::AVPacket) -> Result<bool> {
        if !self.send_queued()? {
            return Ok(false);
        }
        if pkt.is_null() {
            if let Some(au) = self.unit.take() {
                self.access_unit(au)?;
                if !self.send_queued()? {
                    return Ok(false);
                }
            }
            return self.inner.send(pkt);
        }
        // SAFETY: a valid packet holding `size` bytes at `data`.
        let (data, pts, duration, key) = unsafe {
            let p = &*pkt;
            let data = if p.data.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(p.data, p.size.max(0) as usize)
            };
            let pts = if p.pts == ff::AV_NOPTS_VALUE {
                p.dts
            } else {
                p.pts
            };
            (
                data,
                pts,
                p.duration,
                p.flags & ff::AV_PKT_FLAG_KEY as i32 != 0,
            )
        };
        // Access units are told apart by their NAL units: a packet usually
        // holds one, but some muxers put the second view's units at the
        // start of the next packet.
        let mut whole = false;
        for nal in crate::nal::units(data, self.length_size) {
            let Some(&h) = nal.first() else {
                continue;
            };
            let kind = h & 0x1f;
            let starts = matches!(kind, 1 | 5) && first_mb(nal) == Some(0);
            if starts && self.unit.as_ref().is_some_and(|u| u.base) {
                let done = self.unit.take().expect("checked");
                self.access_unit(done)?;
            }
            let unit = self.unit.get_or_insert_with(|| Unit {
                nals: Vec::new(),
                pts,
                duration,
                key,
                base: false,
                second: false,
            });
            if starts {
                (unit.pts, unit.duration, unit.key) = (pts, duration, key);
                whole = true;
            }
            unit.base |= matches!(kind, 1 | 5);
            unit.second |= kind == 20;
            unit.nals.push(nal.to_vec());
        }
        // A packet holding a whole access unit, base view first, ends it;
        // otherwise the next base picture does.
        if whole && self.unit.as_ref().is_some_and(|u| u.base && u.second) {
            let done = self.unit.take().expect("checked");
            self.access_unit(done)?;
        }
        self.send_queued()?;
        Ok(true)
    }

    /// Rewrites an access unit into merged pictures (or in 2D, takes its
    /// base view), queued for the decoder.
    fn access_unit(&mut self, unit: Unit) -> Result<()> {
        if self.two_d.is_none() {
            if unit.key || self.history.len() >= 600 {
                self.history.clear();
            }
            self.history.push(unit.clone());
        }
        let au = self.aus;
        self.aus += 1;
        let tag = |slot| Tag {
            pts: unit.pts,
            duration: unit.duration,
            au,
            slot,
        };
        if self.two_d.is_none() {
            match self
                .rewriter
                .access_unit(unit.nals.iter().map(Vec::as_slice))
            {
                Ok(pictures) => {
                    for p in pictures {
                        self.queue_picture(p.data, tag(Some(p.slot)), p.idr)?;
                    }
                    return Ok(());
                }
                Err(Bad::Broken(why)) => {
                    log::debug!("MVC: {why}; starting again at the next intra picture");
                    self.rewriter.restart();
                    return Ok(());
                }
                Err(Bad::Unsupported(why)) => {
                    log::warn!("3D Blu-ray video (MVC): {why}; playing its left eye alone");
                    self.two_d = Some(why);
                    self.inner.flush();
                    self.queue.clear();
                    self.tags.clear();
                    self.left = None;
                    self.wait_key = true;
                    // Again from the last keyframe, base view alone.
                    let history = std::mem::take(&mut self.history);
                    if let Some((current, earlier)) = history.split_last() {
                        for u in earlier {
                            self.access_unit(u.clone())?;
                        }
                        return self.access_unit(current.clone());
                    }
                }
            }
        }
        if self.wait_key && !unit.key {
            return Ok(());
        }
        let mut out = Vec::new();
        if self.wait_key {
            out.extend_from_slice(&self.base_parameter_sets);
            self.wait_key = false;
        }
        for nal in &unit.nals {
            // The second view's: prefix, subset SPS, slices, delimiter.
            if !matches!(nal[0] & 0x1f, 14 | 15 | 20 | 21 | 24) {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(nal);
            }
        }
        self.queue_picture(out, tag(None), unit.key)
    }

    fn queue_picture(&mut self, data: Vec<u8>, tag: Tag, key: bool) -> Result<()> {
        let pkt = Packet::new();
        let t = self.next_tag;
        self.next_tag += 1;
        // SAFETY: a fresh packet given a buffer of data's size, filled.
        unsafe {
            let p = pkt.as_ptr();
            check(ff::av_new_packet(p, data.len() as i32), "MVC packet")?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), (*p).data, data.len());
            (*p).pts = t;
            (*p).dts = t;
            (*p).duration = tag.duration;
            if key {
                (*p).flags |= ff::AV_PKT_FLAG_KEY as i32;
            }
        }
        self.tags.insert(t, tag);
        // Tags of pictures the decoder never gave back (damaged ones).
        if self.tags.len() > 512 {
            self.tags.retain(|&k, _| k > t - 256);
        }
        self.queue.push_back(pkt);
        Ok(())
    }

    /// Hands queued pictures to the decoder; false when it is full.
    fn send_queued(&mut self) -> Result<bool> {
        while let Some(p) = self.queue.front() {
            if !self.inner.send(p.as_ptr())? {
                return Ok(false);
            }
            self.queue.pop_front();
        }
        Ok(true)
    }

    /// Receives a side-by-side frame, as [`Decoder::receive`].
    pub(crate) fn receive(&mut self, frame: &mut Frame) -> Result<Option<bool>> {
        loop {
            self.send_queued()?;
            match self.inner.receive(&mut self.scratch)? {
                None => return Ok(None),
                Some(false) => {
                    // The end: a base picture still waiting goes out alone.
                    if let Some((left, tag)) = self.left.take()
                        && self.pair(frame, &left, &left, tag)?
                    {
                        return Ok(Some(true));
                    }
                    return Ok(Some(false));
                }
                Some(true) => {}
            }
            // SAFETY: a decoded frame.
            let t = unsafe { (*self.scratch.as_ptr()).pts };
            let decoded = Frame(self.scratch.take_raw());
            let Some(tag) = self.tags.remove(&t) else {
                continue;
            };
            match tag.slot {
                Some(Slot::Copy) => {}
                Some(Slot::Base) => {
                    if let Some((left, ltag)) = self.left.replace((decoded, tag))
                        && self.pair(frame, &left, &left, ltag)?
                    {
                        return Ok(Some(true));
                    }
                }
                Some(Slot::Dependent) => match self.left.take() {
                    Some((left, ltag)) if ltag.au == tag.au => {
                        if self.pair(frame, &left, &decoded, ltag)? {
                            return Ok(Some(true));
                        }
                    }
                    other => self.left = other,
                },
                None => {
                    if self.pair(frame, &decoded, &decoded, tag)? {
                        return Ok(Some(true));
                    }
                }
            }
        }
    }

    /// Puts `left` and `right` side by side into `out`, timed as `tag`;
    /// false when a precise seek leaves the pair out.
    fn pair(&mut self, out: &mut Frame, left: &Frame, right: &Frame, tag: Tag) -> Result<bool> {
        if let Some(s) = self.skip {
            if s.drops(tag.pts, tag.duration) {
                return Ok(false);
            }
            self.skip = None;
        }
        match self.shown {
            Some(t) if self.two_d.is_some() && tag.pts <= t => return Ok(false),
            _ => self.shown = Some(tag.pts),
        }
        // SAFETY: both are decoded frames of the same size and format; out
        // is ours to fill.
        unsafe { side_by_side(out.as_ptr(), left.as_ptr(), right.as_ptr())? };
        // SAFETY: as above.
        unsafe {
            let o = &mut *out.as_ptr();
            o.pts = tag.pts;
            o.best_effort_timestamp = tag.pts;
            o.duration = tag.duration;
        }
        Ok(true)
    }

    pub(crate) fn flush(&mut self) {
        self.inner.flush();
        self.unit = None;
        self.history.clear();
        self.shown = None;
        self.queue.clear();
        self.tags.clear();
        self.left = None;
        self.skip = None;
        if self.two_d.is_some() {
            self.wait_key = true;
        } else {
            self.rewriter.restart();
        }
    }
}

/// `left` and `right` side by side in `out`, one picture twice as wide.
///
/// # Safety
/// `left` and `right` are decoded frames of the same size and planar (or
/// semi-planar) format; `out` is a valid frame.
unsafe fn side_by_side(
    out: *mut ff::AVFrame,
    left: *const ff::AVFrame,
    right: *const ff::AVFrame,
) -> Result<()> {
    // SAFETY: per the contract.
    unsafe {
        let (l, r) = (&*left, &*right);
        if (l.width, l.height, l.format) != (r.width, r.height, r.format) {
            return Err(Error::Unsupported("MVC views of different sizes".into()));
        }
        let desc = ff::av_pix_fmt_desc_get(l.format);
        if desc.is_null() {
            return Err(Error::Unsupported("MVC pixel format".into()));
        }
        ff::av_frame_unref(out);
        (*out).format = l.format;
        (*out).width = l.width * 2;
        (*out).height = l.height;
        check(ff::av_frame_get_buffer(out, 0), "side-by-side frame")?;
        check(ff::av_frame_copy_props(out, left), "side-by-side frame")?;
        let o = &*out;
        for p in 0..ff::av_pix_fmt_count_planes(l.format).max(0) as usize {
            let bytes = ff::av_image_get_linesize(l.format, l.width, p as i32);
            let rows = if p == 0 {
                l.height
            } else {
                -((-l.height) >> (*desc).log2_chroma_h)
            };
            if bytes <= 0 {
                continue;
            }
            for (src, at) in [(l, 0), (r, bytes as isize)] {
                ff::av_image_copy_plane(
                    o.data[p].offset(at),
                    o.linesize[p],
                    src.data[p],
                    src.linesize[p],
                    bytes,
                    rows,
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
