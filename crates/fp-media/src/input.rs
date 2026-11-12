//! An opened container: format context over our I/O context.

use crate::bluray::Title;
use crate::decode::Packet;
use crate::io::IoContext;
use crate::nal::{NalReader, Packing};
use crate::second_view::{BASE_PID, SECOND_PID, SecondView};
use crate::{Result, check};
use fp_core::ByteSource;
use fp_ffmpeg_sys as ff;
use std::collections::VecDeque;
use std::ffi::{CString, c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct Input {
    pub(crate) fmt: *mut ff::AVFormatContext,
    _io: IoContext,
    _interrupt: Arc<AtomicBool>,
    pub name: String,
    origin: f64,
    /// Packets read ahead while opening, given out first.
    peeked: VecDeque<Packet>,
    /// H.264 streams whose packets carry MVC's second view (3D Blu-ray)
    /// though their configuration doesn't say so.
    mvc_in_band: Vec<usize>,
    /// How streams say their eyes are packed in their first packets.
    frame_packing: Vec<(usize, (Packing, bool))>,
    /// Whether the viewer says the video is frame sequential 3D; None goes
    /// by its labels.
    frame_sequential: Option<bool>,
    /// Whether the viewer says the video is interlaced; None goes by its
    /// headers.
    deinterlace: Option<bool>,
    /// 3D Blu-ray's second view in a stream of its own (SSIF files, and
    /// transport streams holding both views), added to the base view's
    /// packets.
    pair: Option<Pair>,
    /// A Blu-ray title, of which this input reads the first clip itself.
    title: Option<Box<Playing>>,
}

/// A title being played clip after clip.
struct Playing {
    title: Title,
    current: usize,
    /// The clip being read when not the first, and each of its streams'
    /// ours (the first clip's stream of the same PID).
    other: Option<(Input, Vec<Option<usize>>)>,
}

impl Playing {
    /// The clip holding `seconds` of the title, and that time on the clip's
    /// own clock in seconds.
    fn locate(&self, seconds: f64) -> (usize, f64) {
        let k = self
            .title
            .clips
            .iter()
            .rposition(|c| c.start <= seconds)
            .unwrap_or(0);
        let c = &self.title.clips[k];
        (
            k,
            c.in_time as f64 / 90_000.0 + (seconds - c.start).max(0.0),
        )
    }
}

/// The base view's stream, the second view's (discarded by FFmpeg) and the
/// reader of the latter.
struct Pair {
    base: usize,
    second: usize,
    view: SecondView,
}

// SAFETY: an Input is owned and used by one thread at a time.
unsafe impl Send for Input {}

unsafe extern "C" fn interrupt_cb(opaque: *mut c_void) -> c_int {
    // SAFETY: opaque points at the AtomicBool kept alive by Input's Arc.
    let flag = unsafe { &*(opaque as *const AtomicBool) };
    flag.load(Ordering::Relaxed) as c_int
}

impl Input {
    /// Opens `src`. `name` (file name or URL) helps FFmpeg pick the demuxer;
    /// a `.iso` name opens the Blu-ray image's main title.
    pub fn open(src: Arc<dyn ByteSource>, name: &str, interrupt: Arc<AtomicBool>) -> Result<Input> {
        if crate::bluray::is_disc_image(name) {
            return Input::open_title(crate::bluray::main_title(src)?, name, interrupt);
        }
        Input::open_file(src, name, interrupt)
    }

    /// A title, its first clip opened as this input.
    fn open_title(title: Title, name: &str, interrupt: Arc<AtomicBool>) -> Result<Input> {
        let first = title
            .clips
            .first()
            .ok_or_else(|| crate::Error::Disc("a title without clips".into()))?;
        let mut input = Input::open_file(first.src.clone(), &first.name, interrupt)?;
        input.name = name.to_string();
        input.origin = first.in_time as f64 / 90_000.0;
        for &st in input.streams() {
            // SAFETY: streams are valid while the input is open.
            let (id, meta) = unsafe { ((*st).id, &mut (*st).metadata) };
            if let Some((_, lang)) = title
                .languages
                .iter()
                .find(|(pid, _)| i32::from(*pid) == id)
                && let Ok(lang) = CString::new(lang.as_str())
            {
                // SAFETY: a valid dictionary slot and NUL-terminated strings.
                unsafe { ff::av_dict_set(meta, c"language".as_ptr(), lang.as_ptr(), 0) };
            }
        }
        input.title = Some(Box::new(Playing {
            title,
            current: 0,
            other: None,
        }));
        Ok(input)
    }

    /// The Blu-ray title this input plays, if it plays one.
    pub(crate) fn title(&self) -> Option<&Title> {
        self.title.as_ref().map(|t| &t.title)
    }

    fn open_file(
        src: Arc<dyn ByteSource>,
        name: &str,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Input> {
        let io = IoContext::new(src.clone(), interrupt.clone())?;
        let cname = CString::new(name.replace('\0', "")).unwrap_or_default();
        // SAFETY: standard FFmpeg open sequence with a custom pb; on failure
        // avformat_open_input frees the context itself.
        unsafe {
            let mut fmt = ff::avformat_alloc_context();
            if fmt.is_null() {
                return Err(crate::Error::Unsupported(
                    "avformat_alloc_context failed".into(),
                ));
            }
            (*fmt).pb = io.as_ptr();
            (*fmt).flags |= ff::AVFMT_FLAG_CUSTOM_IO as c_int;
            (*fmt).interrupt_callback = ff::AVIOInterruptCB {
                callback: Some(interrupt_cb),
                opaque: Arc::as_ptr(&interrupt) as *mut c_void,
            };
            check(
                ff::avformat_open_input(
                    &mut fmt,
                    cname.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                ),
                "open input",
            )?;
            let mut input = Input {
                fmt,
                _io: io,
                _interrupt: interrupt,
                name: name.to_string(),
                origin: 0.0,
                peeked: VecDeque::new(),
                mvc_in_band: Vec::new(),
                frame_packing: Vec::new(),
                frame_sequential: None,
                deinterlace: None,
                pair: None,
                title: None,
            };
            check(
                ff::avformat_find_stream_info(fmt, std::ptr::null_mut()),
                "find stream info",
            )?;
            // MPEG-TS streams appear while probing.
            input.pair_views(src);
            input.origin = input.timeline_start();
            input.read_first_packets();
            if let Some(p) = &input.pair {
                input.mvc_in_band.push(p.base);
            }
            Ok(input)
        }
    }

    pub fn streams(&self) -> &[*mut ff::AVStream] {
        // SAFETY: fmt is open; streams has nb_streams valid pointers.
        unsafe {
            let f = &*self.fmt;
            if f.streams.is_null() {
                &[]
            } else {
                std::slice::from_raw_parts(f.streams, f.nb_streams as usize)
            }
        }
    }

    /// Where the media's timeline starts, in the container's seconds. Times
    /// in and out of the player, thumbnails and seeks count from here, so
    /// a video starts at 0 whatever its first timestamp: MPEG-TS and
    /// Blu-ray (.m2ts) files often start seconds or hours in.
    pub(crate) fn origin(&self) -> f64 {
        self.origin
    }

    /// Makes the stream's first picture `start` seconds into the video: a
    /// server asked to transcode from that time begins its picture there,
    /// whatever clock it counts in (Jellyfin and feed servers start at 0,
    /// Emby keeps the file's own times, audio may begin earlier).
    pub(crate) fn start_at(&mut self, start: f64) {
        let picture = self.streams().iter().find_map(|&st| {
            // SAFETY: stream pointers and their parameters are valid while
            // the input is open.
            let (st, kind) = unsafe { (&*st, (*(*st).codecpar).codec_type) };
            let attached = st.disposition & ff::AV_DISPOSITION_ATTACHED_PIC as c_int != 0;
            (kind == ff::AVMEDIA_TYPE_VIDEO && !attached && st.start_time != ff::AV_NOPTS_VALUE)
                .then(|| st.start_time as f64 * crate::q2d(st.time_base))
        });
        self.origin = picture.unwrap_or(self.origin) - start;
    }

    /// The earliest start of an audio or video stream, when it is positive.
    /// Other streams don't count: a subtitle file's starts at its first
    /// cue. A start below 0 is an encoder's priming and is kept.
    fn timeline_start(&self) -> f64 {
        self.streams()
            .iter()
            .filter_map(|&st| {
                // SAFETY: stream pointers and their parameters are valid
                // while the input is open.
                let (st, kind) = unsafe { (&*st, (*(*st).codecpar).codec_type) };
                let picture = st.disposition & ff::AV_DISPOSITION_ATTACHED_PIC as c_int != 0;
                let media = matches!(kind, ff::AVMEDIA_TYPE_VIDEO | ff::AVMEDIA_TYPE_AUDIO);
                (media && !picture && st.start_time != ff::AV_NOPTS_VALUE)
                    .then(|| st.start_time as f64 * crate::q2d(st.time_base))
            })
            .reduce(f64::min)
            .filter(|t| *t > 0.0)
            .unwrap_or(0.0)
    }

    /// Pairs Blu-ray's base view (PID 0x1011) with its second view (0x1012)
    /// when both are H.264 streams and the second holds MVC.
    fn pair_views(&mut self, src: Arc<dyn ByteSource>) {
        let pid_stream = |pid: i32| {
            self.streams().iter().position(|&st| {
                // SAFETY: streams and their parameters are valid while open.
                let (st, par) = unsafe { (&*st, &*(*st).codecpar) };
                st.id == pid && par.codec_id == ff::AV_CODEC_ID_H264
            })
        };
        let (Some(base), Some(second)) = (pid_stream(BASE_PID), pid_stream(SECOND_PID)) else {
            return;
        };
        let Some(view) = SecondView::open(src, self._interrupt.clone(), SECOND_PID as u16) else {
            return;
        };
        // SAFETY: the stream is valid while the input is open.
        unsafe { (*self.streams()[second]).discard = ff::AVDISCARD_ALL };
        self.pair = Some(Pair { base, second, view });
    }

    /// Stream `index` is a part of another (the second view of 3D Blu-ray),
    /// not one to show or choose.
    pub(crate) fn hidden(&self, index: usize) -> bool {
        self.pair.as_ref().is_some_and(|p| p.second == index)
    }

    /// Reads up to the second packet of each H.264 and HEVC video stream,
    /// mostly from what probing buffered, to see the 3D layout it states
    /// there, and whether an H.264 stream not marked as MVC holds MVC's second
    /// view anyway (3D Blu-ray remuxed by tools that drop the `mvcC` box). The
    /// packets read are given out first by [`Input::read`].
    fn read_first_packets(&mut self) {
        // Each stream's first two packets: some muxers put the second view
        // of the first access unit at the start of the second.
        let mut unseen: Vec<(usize, NalReader, u32)> = self
            .streams()
            .iter()
            .enumerate()
            .filter_map(|(i, &st)| {
                // SAFETY: streams and their parameters are valid while open.
                let par = unsafe { &*(*st).codecpar };
                let extradata = crate::mvc::extradata(par);
                let mvc = self
                    .pair
                    .as_ref()
                    .is_some_and(|p| p.base == i || p.second == i)
                    || crate::mvc::config(par.codec_id, extradata).is_some();
                (par.codec_type == ff::AVMEDIA_TYPE_VIDEO && !mvc)
                    .then(|| NalReader::new(par.codec_id, extradata))
                    .flatten()
                    .map(|reader| (i, reader, 2))
            })
            .collect();
        while !unseen.is_empty() && self.peeked.len() < 256 {
            let mut pkt = Packet::new();
            if !matches!(self.demux(pkt.as_ptr()), Ok(true)) {
                break;
            }
            let index = pkt.stream_index();
            if let Some(at) = unseen.iter().position(|&(i, _, _)| i == index) {
                let (_, reader, left) = &mut unseen[at];
                let reader = *reader;
                *left -= 1;
                if *left == 0 {
                    unseen.swap_remove(at);
                }
                // SAFETY: a packet just read, holding size bytes at data.
                let data = unsafe {
                    let p = &*pkt.as_ptr();
                    if p.data.is_null() {
                        &[][..]
                    } else {
                        std::slice::from_raw_parts(p.data, p.size.max(0) as usize)
                    }
                };
                if !self.frame_packing.iter().any(|&(i, _)| i == index)
                    && let Some(layout) = reader.frame_packing(data)
                {
                    self.frame_packing.push((index, layout));
                }
                // SAFETY: as above, the parameters of a stream of this input.
                let h264 = unsafe {
                    (*(*self.streams()[index]).codecpar).codec_id == ff::AV_CODEC_ID_H264
                };
                if h264
                    && crate::nal::units(data, reader.length_size())
                        .any(|n| n.first().is_some_and(|h| matches!(h & 0x1f, 15 | 20)))
                {
                    self.mvc_in_band.push(index);
                    unseen.retain(|&(i, _, _)| i != index);
                }
            }
            self.peeked.push_back(pkt.take());
        }
    }

    /// How stream `index` says its eyes are packed in its first packets, and
    /// whether the right eye comes first.
    pub(crate) fn frame_packing(&self, index: usize) -> Option<(Packing, bool)> {
        self.frame_packing
            .iter()
            .find(|&&(i, _)| i == index)
            .map(|&(_, layout)| layout)
    }

    /// Whether stream `index` carries MVC's second view in its packets,
    /// configured or not.
    pub(crate) fn mvc_in_band(&self, index: usize) -> bool {
        self.mvc_in_band.contains(&index)
    }

    /// Plays the video as frame sequential 3D or not, whatever its labels
    /// say (None: as they say). Read when its info is read and its video
    /// decoder opened.
    pub(crate) fn set_frame_sequential(&mut self, choice: Option<bool>) {
        self.frame_sequential = choice;
    }

    pub(crate) fn frame_sequential(&self) -> Option<bool> {
        self.frame_sequential
    }

    /// Deinterlaces the video or not, whatever its headers say (None: as
    /// they say). Read when its info is read and its video decoder opened.
    pub(crate) fn set_deinterlace(&mut self, choice: Option<bool>) {
        self.deinterlace = choice;
    }

    pub(crate) fn deinterlace(&self) -> Option<bool> {
        self.deinterlace
    }

    /// Best stream of a media type, as FFmpeg chooses it.
    pub fn best_stream(&self, kind: ff::AVMediaType) -> Option<usize> {
        // SAFETY: fmt is open.
        let i = unsafe { ff::av_find_best_stream(self.fmt, kind, -1, -1, std::ptr::null_mut(), 0) };
        let i = (i >= 0).then_some(i as usize)?;
        match &self.pair {
            Some(p) if p.second == i => Some(p.base),
            _ => Some(i),
        }
    }

    /// Seeks so the next packets start at or before `seconds` on the media's
    /// timeline (see [`Input::origin`]).
    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        match self.title.as_ref().map(|t| t.locate(seconds)) {
            Some((k, at)) => self.seek_clip(k, at, false),
            None => self.seek_file(seconds),
        }
    }

    /// Seeks so the next packets start at the first keyframe after
    /// `seconds`.
    pub(crate) fn seek_after(&mut self, seconds: f64) -> Result<()> {
        match self.title.as_ref().map(|t| t.locate(seconds)) {
            Some((k, at)) => self.seek_clip(k, at, true),
            None => self.seek_after_file(seconds),
        }
    }

    /// Reads clip `k` of the title from `at` seconds of its clock: before
    /// it, or from the first keyframe `after`.
    fn seek_clip(&mut self, k: usize, at: f64, after: bool) -> Result<()> {
        let current = self.title.as_ref().map_or(0, |t| t.current);
        if k == 0 {
            if let Some(t) = &mut self.title {
                (t.current, t.other) = (0, None);
            }
            let at = at - self.origin;
            return if after {
                self.seek_after_file(at)
            } else {
                self.seek_file(at)
            };
        }
        if current != k {
            self.open_clip(k)?;
        }
        let Some((other, _)) = self.title.as_mut().and_then(|t| t.other.as_mut()) else {
            return Ok(());
        };
        let at = at - other.origin;
        if after {
            other.seek_after_file(at)
        } else {
            other.seek_file(at)
        }
    }

    /// Reads clip `k` (not the first) from now on, from where its play item
    /// starts.
    fn open_clip(&mut self, k: usize) -> Result<()> {
        let ours: Vec<(i32, ff::AVMediaType)> = self
            .streams()
            .iter()
            // SAFETY: streams and their parameters are valid while open.
            .map(|&st| unsafe { ((*st).id, (*(*st).codecpar).codec_type) })
            .collect();
        let interrupt = self._interrupt.clone();
        let Some(t) = &mut self.title else {
            return Ok(());
        };
        let clip = &t.title.clips[k];
        let mut other = Input::open_file(clip.src.clone(), &clip.name, interrupt)?;
        let map = other
            .streams()
            .iter()
            .map(|&st| {
                // SAFETY: as above.
                let id = unsafe { ((*st).id, (*(*st).codecpar).codec_type) };
                ours.iter().position(|o| *o == id)
            })
            .collect();
        // A play item starting into its clip.
        let start = clip.in_time as f64 / 90_000.0;
        if start > other.origin + 1.0 {
            other.seek_file(start - other.origin)?;
        }
        (t.current, t.other) = (k, Some((other, map)));
        Ok(())
    }

    fn seek_file(&mut self, seconds: f64) -> Result<()> {
        self.peeked.clear();
        let ts = ((seconds.max(0.0) + self.origin) * ff::AV_TIME_BASE as f64) as i64;
        let (stream, ts) = self.seek_stream(ts);
        // SAFETY: fmt is open.
        unsafe {
            check(
                ff::avformat_seek_file(
                    self.fmt,
                    stream,
                    i64::MIN,
                    ts,
                    ts,
                    ff::AVSEEK_FLAG_BACKWARD as c_int,
                ),
                "seek",
            )?;
        }
        Ok(())
    }

    fn seek_after_file(&mut self, seconds: f64) -> Result<()> {
        let ts = ((seconds.max(0.0) + self.origin) * ff::AV_TIME_BASE as f64) as i64;
        let (stream, ts) = self.seek_stream(ts);
        // SAFETY: fmt is open.
        unsafe {
            check(
                ff::avformat_seek_file(self.fmt, stream, ts + 1, ts + 1, i64::MAX, 0),
                "seek",
            )?;
        }
        Ok(())
    }

    /// The stream to seek by, and `ts` (microseconds) in its time base.
    /// With the views paired, the base view's: FFmpeg would seek by the
    /// first video stream, the discarded second view.
    fn seek_stream(&mut self, ts: i64) -> (c_int, i64) {
        let Some(p) = &mut self.pair else {
            return (-1, ts);
        };
        p.view.seek();
        let base = p.base;
        // SAFETY: the stream is valid while the input is open.
        let tb = unsafe { (*self.streams()[base]).time_base };
        let us = ff::AVRational {
            num: 1,
            den: ff::AV_TIME_BASE as c_int,
        };
        // SAFETY: plain arithmetic.
        (base as c_int, unsafe { ff::av_rescale_q(ts, us, tb) })
    }

    /// Reads the next packet into `pkt`. Returns false at the end.
    pub fn read(&mut self, pkt: *mut ff::AVPacket) -> Result<bool> {
        if self.title.is_none() {
            return self.read_file(pkt);
        }
        loop {
            let in_other = self.title.as_ref().is_some_and(|t| t.other.is_some());
            let more = match self.title.as_mut().and_then(|t| t.other.as_mut()) {
                Some((other, _)) if in_other => other.read_file(pkt)?,
                _ => self.read_file(pkt)?,
            };
            if !more {
                if self.next_clip()? {
                    continue;
                }
                return Ok(false);
            }
            let Some(t) = &self.title else {
                return Ok(true);
            };
            let clip = &t.title.clips[t.current];
            let first_in = t.title.clips[0].in_time;
            let shift = first_in + (clip.start * 90_000.0).round() as i64 - clip.in_time;
            let out_time = clip.out_time;
            // SAFETY: a packet just read, ours to change.
            let p = unsafe { &mut *pkt };
            if let Some((_, map)) = &t.other {
                match map.get(p.stream_index as usize).copied().flatten() {
                    Some(i) => p.stream_index = i as c_int,
                    None => {
                        // SAFETY: as above.
                        unsafe { ff::av_packet_unref(pkt) };
                        continue;
                    }
                }
            }
            let ts = if p.dts == ff::AV_NOPTS_VALUE {
                p.pts
            } else {
                p.dts
            };
            if ts != ff::AV_NOPTS_VALUE && ts >= out_time {
                // Past the play item: on to the next once its video is.
                let video =
                    Some(p.stream_index as usize) == self.best_stream(ff::AVMEDIA_TYPE_VIDEO);
                // SAFETY: as above.
                unsafe { ff::av_packet_unref(pkt) };
                if video && !self.next_clip()? {
                    return Ok(false);
                }
                continue;
            }
            for v in [&mut p.pts, &mut p.dts] {
                if *v != ff::AV_NOPTS_VALUE {
                    *v += shift;
                }
            }
            return Ok(true);
        }
    }

    /// Moves on to the title's next clip; false after the last.
    fn next_clip(&mut self) -> Result<bool> {
        let Some(t) = &self.title else {
            return Ok(false);
        };
        let next = t.current + 1;
        if next >= t.title.clips.len() {
            return Ok(false);
        }
        self.open_clip(next)?;
        Ok(true)
    }

    /// The next packet of this input's own file.
    fn read_file(&mut self, pkt: *mut ff::AVPacket) -> Result<bool> {
        if let Some(p) = self.peeked.pop_front() {
            // SAFETY: both packets are valid; pkt is the caller's to fill.
            unsafe { ff::av_packet_move_ref(pkt, p.as_ptr()) };
        } else if !self.demux(pkt)? {
            return Ok(false);
        }
        self.add_second_view(pkt)?;
        Ok(true)
    }

    /// Appends the second view's units to a packet of the base view.
    fn add_second_view(&mut self, pkt: *mut ff::AVPacket) -> Result<()> {
        let Some(p) = &mut self.pair else {
            return Ok(());
        };
        // SAFETY: a packet just read.
        let (index, dts, pos) = unsafe {
            let k = &*pkt;
            let dts = if k.dts == ff::AV_NOPTS_VALUE {
                k.pts
            } else {
                k.dts
            };
            (k.stream_index as usize, dts, k.pos)
        };
        if index != p.base || dts == ff::AV_NOPTS_VALUE {
            return Ok(());
        }
        let Some(second) = p.view.take(dts, pos.max(0) as u64) else {
            return Ok(());
        };
        // SAFETY: av_grow_packet makes room after the data (copying it when
        // shared); the second view's units then fill that room.
        unsafe {
            let at = (*pkt).size as usize;
            check(
                ff::av_grow_packet(pkt, second.len() as c_int),
                "grow packet",
            )?;
            std::ptr::copy_nonoverlapping(second.as_ptr(), (*pkt).data.add(at), second.len());
        }
        Ok(())
    }

    /// The demuxer's next packet.
    fn demux(&mut self, pkt: *mut ff::AVPacket) -> Result<bool> {
        // SAFETY: pkt is a valid allocated packet owned by the caller.
        let r = unsafe { ff::av_read_frame(self.fmt, pkt) };
        if r == ff::AVERROR_EOF_ {
            return Ok(false);
        }
        check(r, "read packet")?;
        Ok(true)
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        // SAFETY: closes the context opened in open(); the custom pb is freed
        // afterwards by IoContext's Drop.
        unsafe { ff::avformat_close_input(&mut self.fmt) };
    }
}
