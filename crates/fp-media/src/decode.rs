//! Thin RAII wrappers over FFmpeg packets, frames and decoders.

use crate::input::Input;
use crate::nal::NalReader;
use crate::{Error, Result, check};
use fp_ffmpeg_sys as ff;
use std::ffi::{CStr, CString};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub(crate) static HW_SESSIONS: AtomicUsize = AtomicUsize::new(0);

/// Hardware decoder sessions this process holds (native V4L2 decoders and
/// FFmpeg hardware decoders). The driver admits only a few at a time, Steam
/// holding some, so one a closed video never gave back leaves later videos
/// in software ("Hardware decoder busy").
pub fn hardware_sessions() -> usize {
    HW_SESSIONS.load(Ordering::Relaxed)
}

static HEVC_10BIT_HARDWARE: AtomicBool = AtomicBool::new(false);

/// Lets 10-bit HEVC decode in hardware. The Frame's decoder writes those
/// pictures only as [`PixelLayout::Q10c`](crate::PixelLayout::Q10c), so this
/// stays off unless the renderer can show them.
pub fn set_hevc_10bit_hardware(on: bool) {
    HEVC_10BIT_HARDWARE.store(on, Ordering::Relaxed);
}

/// Whether 10-bit HEVC decodes in hardware (see [`set_hevc_10bit_hardware`]).
pub fn hevc_10bit_hardware() -> bool {
    HEVC_10BIT_HARDWARE.load(Ordering::Relaxed)
}

static SHARED_FRAMES: AtomicBool = AtomicBool::new(false);

/// Lets the player's hardware decoder hand its pictures over in its own
/// buffers ([`VideoFrame::shared`](crate::VideoFrame::shared)) instead of
/// copies. The renderer turns this on when it can import them, and off if
/// an import fails, after which pictures are copied again.
/// `FRAMEPLAYERX_ZERO_COPY=0` keeps it off.
pub fn set_shared_frames(on: bool) {
    let allowed = std::env::var("FRAMEPLAYERX_ZERO_COPY").map_or(true, |v| v.trim() != "0");
    SHARED_FRAMES.store(on && allowed, Ordering::Relaxed);
}

/// Whether hardware-decoded pictures are shared (see [`set_shared_frames`]).
pub fn shared_frames() -> bool {
    SHARED_FRAMES.load(Ordering::Relaxed)
}

/// An owned `AVPacket`.
pub struct Packet(pub(crate) *mut ff::AVPacket);
// SAFETY: packets are reference counted buffers; ownership moves between threads.
unsafe impl Send for Packet {}

impl Packet {
    pub fn new() -> Packet {
        // SAFETY: allocation; checked by users through as_ptr non-null.
        Packet(unsafe { ff::av_packet_alloc() })
    }
    pub fn as_ptr(&self) -> *mut ff::AVPacket {
        self.0
    }
    pub fn stream_index(&self) -> usize {
        // SAFETY: valid packet.
        unsafe { (*self.0).stream_index.max(0) as usize }
    }
    /// Releases the data, leaving the packet empty for the next read.
    pub fn unref(&mut self) {
        // SAFETY: valid packet.
        unsafe { ff::av_packet_unref(self.0) };
    }
    /// Moves the contents into a fresh packet, leaving this one empty.
    pub fn take(&mut self) -> Packet {
        let p = Packet::new();
        // SAFETY: both packets are valid.
        unsafe { ff::av_packet_move_ref(p.0, self.0) };
        p
    }
    pub fn size(&self) -> usize {
        // SAFETY: valid packet.
        unsafe { (*self.0).size.max(0) as usize }
    }
}

impl Default for Packet {
    fn default() -> Self {
        Packet::new()
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        // SAFETY: we own the packet.
        unsafe { ff::av_packet_free(&mut self.0) };
    }
}

/// An owned `AVFrame`.
pub struct Frame(pub(crate) *mut ff::AVFrame);
// SAFETY: as for Packet.
unsafe impl Send for Frame {}

impl Frame {
    pub fn new() -> Frame {
        // SAFETY: allocation.
        Frame(unsafe { ff::av_frame_alloc() })
    }
    pub fn as_ptr(&self) -> *mut ff::AVFrame {
        self.0
    }
    /// Gives the underlying reference away and allocates a fresh empty frame.
    pub fn take_raw(&mut self) -> *mut ff::AVFrame {
        // SAFETY: allocation, as in `new`.
        std::mem::replace(&mut self.0, unsafe { ff::av_frame_alloc() })
    }
}

impl Default for Frame {
    fn default() -> Self {
        Frame::new()
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: we own the frame.
        unsafe { ff::av_frame_free(&mut self.0) };
    }
}

/// Hardware decoding preference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HwDecode {
    /// V4L2 hardware decoder when one exists for the codec, else software.
    #[default]
    Auto,
    Off,
    /// Software, on one thread, keyframes only and without in-loop
    /// filtering: stills for thumbnails, cheap in the background while the
    /// library is browsed.
    Stills,
}

/// FFmpeg's V4L2 decoder for a codec. The Frame's decoder also takes VP9, but
/// its VP9 sessions can bring the firmware down (ValveSoftware/SteamOS#2865).
fn v4l2_name(codec: ff::AVCodecID) -> Option<&'static CStr> {
    match codec {
        ff::AV_CODEC_ID_HEVC => Some(c"hevc_v4l2m2m"),
        ff::AV_CODEC_ID_H264 => Some(c"h264_v4l2m2m"),
        _ => None,
    }
}

/// One way of decoding a stream, in the order [`candidates`] tries them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Candidate {
    /// Our own V4L2 decoder ([`crate::v4l2`]).
    NativeV4l2,
    /// An FFmpeg decoder by name, and whether it is hardware.
    Named(&'static CStr, bool),
    /// FFmpeg's default decoder for the codec (software).
    Default,
}

/// Decoders to try for a stream: the native V4L2 path, FFmpeg's V4L2
/// wrapper, dav1d for AV1, then FFmpeg's default software decoder.
/// `ten_bit_hevc`: whether 10-bit HEVC may use the native path (see
/// [`set_hevc_10bit_hardware`]); FFmpeg's wrapper only gets 8-bit streams,
/// as it would ask for NV12, which the decoder fills with clamped samples.
/// `interlaced`: coded as fields ([`crate::info::interlaced`]), which the
/// hardware decoder doesn't take for H.264: FFmpeg's wrapper is refused every
/// packet of 1080i, and the native path gives up after seconds.
fn candidates(
    video: bool,
    codec: ff::AVCodecID,
    format: ff::AVPixelFormat,
    hw: HwDecode,
    native: bool,
    ten_bit_hevc: bool,
    interlaced: bool,
) -> Vec<Candidate> {
    let mut c = Vec::new();
    let hw = if hw == HwDecode::Stills || (interlaced && codec == ff::AV_CODEC_ID_H264) {
        HwDecode::Off
    } else {
        hw
    };
    let eight_bit = matches!(
        format,
        ff::AV_PIX_FMT_NONE | ff::AV_PIX_FMT_YUV420P | ff::AV_PIX_FMT_YUVJ420P
    );
    if video && hw == HwDecode::Auto && eight_bit {
        if native && crate::v4l2::Codec::from_ffmpeg(codec).is_some() {
            c.push(Candidate::NativeV4l2);
        }
        if let Some(n) = v4l2_name(codec) {
            c.push(Candidate::Named(n, true));
        }
    }
    if video
        && hw == HwDecode::Auto
        && native
        && ten_bit_hevc
        && codec == ff::AV_CODEC_ID_HEVC
        && format == ff::AV_PIX_FMT_YUV420P10LE
    {
        c.push(Candidate::NativeV4l2);
    }
    if codec == ff::AV_CODEC_ID_AV1 {
        c.push(Candidate::Named(c"libdav1d", false));
    }
    c.push(Candidate::Default);
    c
}

/// Runs `f` with the calling thread allowed on every CPU, so the threads it
/// starts inherit that: a software decoder sizes its pool from the CPUs it
/// may use and starts its workers when opened. Steam launches games on the
/// Frame's five big cores only (`STEAM_LAUNCH_WRAPPER_AFFINITY_LIST=0xf8`),
/// where 8K60 AV1 decodes at 61 fps with nothing else running and falls
/// behind once the compositor and renderer share them; the other three add
/// ~7%. SteamVR's tracking there runs at nice -10, so it still comes first.
pub(crate) fn on_all_cpus<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(target_os = "linux")]
    // SAFETY: plain affinity calls on the calling thread with owned sets.
    unsafe {
        let size = size_of::<libc::cpu_set_t>();
        let mut old: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, size, &mut old) == 0 {
            let mut all: libc::cpu_set_t = std::mem::zeroed();
            let cpus = libc::sysconf(libc::_SC_NPROCESSORS_CONF).clamp(1, 1024) as usize;
            for cpu in 0..cpus {
                libc::CPU_SET(cpu, &mut all);
            }
            if libc::sched_setaffinity(0, size, &all) == 0 {
                let r = f();
                libc::sched_setaffinity(0, size, &old);
                return r;
            }
        }
    }
    f()
}

/// Replaces a frame that references a hardware decoder's buffer with a copy
/// in ordinary memory, giving the buffer straight back to the decoder.
/// FFmpeg's V4L2 decoders hand out their CAPTURE buffers themselves, and the
/// player holds more frames (queue, picture on screen) than the 8 buffers
/// they get: with the picture standing still (paused, or the video closing)
/// the decoder then waits forever for a free one ("All capture buffers
/// returned to userspace"), the video never finishes closing and its
/// hardware session stays taken, so later videos decode in software.
fn copy_out(frame: &mut Frame) -> Result<()> {
    // SAFETY: frame holds a decoded picture; the copy is allocated with its
    // format and size before av_frame_copy fills it.
    unsafe {
        let src = frame.0;
        if (*src).format == ff::AV_PIX_FMT_DRM_PRIME {
            return Ok(()); // descriptors, not pixels (never requested)
        }
        let mut dst = ff::av_frame_alloc();
        if dst.is_null() {
            return Err(Error::Unsupported("out of memory for a frame".into()));
        }
        (*dst).format = (*src).format;
        (*dst).width = (*src).width;
        (*dst).height = (*src).height;
        let copied = check(ff::av_frame_get_buffer(dst, 0), "frame copy buffer")
            .and_then(|_| check(ff::av_frame_copy(dst, src), "frame copy"))
            .and_then(|_| check(ff::av_frame_copy_props(dst, src), "frame copy props"));
        if let Err(e) = copied {
            ff::av_frame_free(&mut dst);
            return Err(e);
        }
        ff::av_frame_unref(src);
        ff::av_frame_move_ref(src, dst);
        ff::av_frame_free(&mut dst);
    }
    Ok(())
}

/// Packets kept while the native decoder starts up, replayed into the
/// fallback decoder if it fails before its first frame.
#[cfg(target_os = "linux")]
const STARTUP_PACKETS: usize = 240;

#[cfg(target_os = "linux")]
struct Native {
    dec: crate::v4l2::V4l2Decoder,
    /// Owned copy of the stream parameters, for opening a fallback.
    par: *mut ff::AVCodecParameters,
    hw: HwDecode,
    /// Packets sent before the first frame (None once frames flow or the
    /// startup window is exceeded).
    startup: Option<Vec<Packet>>,
}

#[cfg(target_os = "linux")]
impl Drop for Native {
    fn drop(&mut self) {
        // SAFETY: we own the parameters copy.
        unsafe { ff::avcodec_parameters_free(&mut self.par) };
    }
}

pub struct Decoder {
    /// FFmpeg decoder; null while the native decoder is active.
    ctx: *mut ff::AVCodecContext,
    #[cfg(target_os = "linux")]
    native: Option<Box<Native>>,
    /// Packets to feed the FFmpeg decoder before new ones (after a fallback).
    replay: std::collections::VecDeque<Packet>,
    /// End of stream goes in after `replay`, from `receive`: the fallback
    /// came after it was sent, so nothing more will be.
    replay_end: bool,
    pub name: String,
    pub hardware: bool,
    /// Why a hardware decoder that was tried is not the one decoding: its
    /// open failed, or it failed once running.
    pub hardware_error: Option<String>,
    /// Who held the driver when it found no room for this stream.
    pub hardware_holders: crate::SessionHolders,
    pub time_base: ff::AVRational,
    /// The input's [`Input::origin`], taken off every time.
    pub(crate) origin: f64,
    /// Reads H.264 and HEVC packets' NAL units.
    nal: Option<NalReader>,
    skip: Option<Skip>,
    /// 3D video decoded by the decoder this holds, its eyes paired.
    paired: Option<Paired>,
    /// Packets precise seeks left out.
    pub(crate) left_out: u64,
}

/// The ways of decoding through another decoder and reworking its pictures:
/// 3D's eyes paired, or interlaced fields made whole.
enum Paired {
    /// 3D Blu-ray (MVC).
    Mvc(Box<crate::mvc::MvcDecoder>),
    FrameSequential(Box<crate::frame_seq::FrameSeqDecoder>),
    Deinterlace(Box<crate::deinterlace::Deinterlacer>),
}

impl Paired {
    fn inner(&mut self) -> &mut Decoder {
        match self {
            Paired::Mvc(m) => &mut m.inner,
            Paired::FrameSequential(f) => &mut f.inner,
            Paired::Deinterlace(d) => &mut d.inner,
        }
    }
}

/// What a precise seek leaves out: frames shown mostly before its target.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Skip {
    /// The target in the decoder's time base.
    target: f64,
    /// The duration of frames without one, in the same units.
    frame_duration: f64,
    /// The media timeline's start in the same units, where frames without
    /// a timestamp count as shown.
    start: f64,
}

impl Skip {
    /// Whether a frame at `pts` lasting `duration` (0 when unknown), both
    /// in the decoder's time base, shows mostly before the target.
    pub(crate) fn drops(self, pts: i64, duration: i64) -> bool {
        let pts = if pts == ff::AV_NOPTS_VALUE {
            self.start
        } else {
            pts as f64
        };
        let duration = if duration > 0 {
            duration as f64
        } else {
            self.frame_duration
        };
        pts + duration * 0.5 < self.target
    }
}

// SAFETY: one thread drives a decoder at a time.
unsafe impl Send for Decoder {}

impl Drop for Decoder {
    fn drop(&mut self) {
        if self.hardware && !self.ctx.is_null() {
            HW_SESSIONS.fetch_sub(1, Ordering::Relaxed);
        }
        // SAFETY: we own the context (null-safe).
        unsafe { ff::avcodec_free_context(&mut self.ctx) };
    }
}

impl Decoder {
    /// Opens a decoder for stream `index` of `input`. Video tries the
    /// hardware decoders first when `hw` allows, and AV1 uses dav1d in
    /// software. Video to deinterlace decodes in software, as bwdif takes
    /// pictures in memory; stills aren't deinterlaced, being keyframes
    /// alone with no fields around them.
    pub(crate) fn open(input: &Input, index: usize, hw: HwDecode) -> Result<Decoder> {
        let stream = input.streams()[index];
        // SAFETY: stream belongs to an open input.
        let (par, time_base) = unsafe { ((*stream).codecpar, (*stream).time_base) };
        if let Some(config) = crate::mvc::stream_config(input, index) {
            let mut dec = Self::open_mvc(par, time_base, hw, &config)?;
            dec.origin = input.origin();
            return Ok(dec);
        }
        let deinterlace = (hw != HwDecode::Stills)
            .then(|| crate::deinterlace::wanted(input, index))
            .flatten();
        let decode_hw = if deinterlace.is_some() {
            HwDecode::Off
        } else {
            hw
        };
        let mut dec = Self::open_with(par, time_base, decode_hw, crate::v4l2::enabled())?;
        dec.origin = input.origin();
        // SAFETY: as above; extradata holds extradata_size bytes.
        dec.nal = unsafe {
            let p = &*par;
            let extradata = if p.extradata.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(p.extradata, p.extradata_size.max(0) as usize)
            };
            NalReader::new(p.codec_id, extradata)
        };
        if let Some(stream) = crate::frame_seq::stream(input, index) {
            return Ok(Self::open_frame_sequential(
                dec,
                stream,
                hw == HwDecode::Stills,
            ));
        }
        if let Some(frames) = deinterlace {
            return Ok(Self::open_deinterlaced(dec, frames));
        }
        Ok(dec)
    }

    /// A decoder deinterlacing what `inner` decodes, its pictures timed in
    /// fields.
    fn open_deinterlaced(inner: Decoder, frames: crate::deinterlace::Frames) -> Decoder {
        let d = crate::deinterlace::Deinterlacer::new(inner, frames);
        let mut dec = Decoder {
            ctx: std::ptr::null_mut(),
            #[cfg(target_os = "linux")]
            native: None,
            replay: Default::default(),
            replay_end: false,
            name: String::new(),
            hardware: false,
            hardware_error: None,
            hardware_holders: Default::default(),
            time_base: d.time_base(),
            origin: d.inner.origin,
            nal: None,
            skip: None,
            paired: Some(Paired::Deinterlace(Box::new(d))),
            left_out: 0,
        };
        dec.follow_paired();
        dec
    }

    /// A decoder pairing a frame sequential stream's eyes, around `inner`.
    fn open_frame_sequential(
        inner: Decoder,
        stream: crate::frame_seq::Stream,
        stills: bool,
    ) -> Decoder {
        let mut dec = Decoder {
            ctx: std::ptr::null_mut(),
            #[cfg(target_os = "linux")]
            native: None,
            replay: Default::default(),
            replay_end: false,
            name: String::new(),
            hardware: false,
            hardware_error: None,
            hardware_holders: Default::default(),
            time_base: inner.time_base,
            origin: inner.origin,
            nal: None,
            skip: None,
            paired: Some(Paired::FrameSequential(Box::new(
                crate::frame_seq::FrameSeqDecoder::new(inner, stream, stills),
            ))),
            left_out: 0,
        };
        dec.follow_paired();
        dec
    }

    /// A decoder of 3D Blu-ray video (MVC): an H.264 decoder of the stream
    /// [`crate::mvc`] rewrites it into, the same size, without extradata.
    fn open_mvc(
        par: *const ff::AVCodecParameters,
        time_base: ff::AVRational,
        hw: HwDecode,
        config: &crate::mvc::Config,
    ) -> Result<Decoder> {
        // SAFETY: copies the parameters into a fresh allocation, freed below
        // (the decoders keep copies of their own).
        let inner = unsafe {
            let mut p = ff::avcodec_parameters_alloc();
            if p.is_null() || ff::avcodec_parameters_copy(p, par) < 0 {
                ff::avcodec_parameters_free(&mut p);
                return Err(Error::Unsupported("codec parameters copy".into()));
            }
            ff::av_freep((&raw mut (*p).extradata).cast());
            (*p).extradata_size = 0;
            (*p).codec_tag = 0;
            (*p).profile = ff::AV_PROFILE_H264_HIGH as i32;
            (*p).level = 51;
            let inner = Self::open_with(p, time_base, hw, crate::v4l2::enabled());
            ff::avcodec_parameters_free(&mut p);
            inner?
        };
        let mvc = crate::mvc::MvcDecoder::new(inner, config);
        let mut dec = Decoder {
            ctx: std::ptr::null_mut(),
            #[cfg(target_os = "linux")]
            native: None,
            replay: Default::default(),
            replay_end: false,
            name: String::new(),
            hardware: false,
            hardware_error: None,
            hardware_holders: Default::default(),
            time_base,
            origin: 0.0,
            nal: None,
            skip: None,
            paired: Some(Paired::Mvc(Box::new(mvc))),
            left_out: 0,
        };
        dec.follow_paired();
        Ok(dec)
    }

    /// Takes the pairing decoder's inner decoder's name and hardware state
    /// (which change when the hardware decoder fails and software takes
    /// over).
    fn follow_paired(&mut self) {
        let (inner, what) = match &self.paired {
            None => return,
            Some(Paired::Mvc(m)) if m.two_d.is_some() => (&m.inner, "3D Blu-ray as 2D"),
            Some(Paired::Mvc(m)) => (&m.inner, "3D Blu-ray"),
            Some(Paired::FrameSequential(f)) => (&f.inner, "frame sequential 3D"),
            Some(Paired::Deinterlace(d)) => (&d.inner, "deinterlaced"),
        };
        self.name = format!("{} ({what})", inner.name);
        self.hardware = inner.hardware;
        self.hardware_error = inner.hardware_error.clone();
        self.hardware_holders = inner.hardware_holders.clone();
    }

    pub(crate) fn open_with(
        par: *const ff::AVCodecParameters,
        time_base: ff::AVRational,
        hw: HwDecode,
        native: bool,
    ) -> Result<Decoder> {
        // SAFETY: par is valid for the call.
        let (codec_id, kind, format) =
            unsafe { ((*par).codec_id, (*par).codec_type, (*par).format) };
        let mut last_err = Error::Unsupported(format!("no decoder for codec id {codec_id}"));
        let mut hardware_error = None;
        let mut hardware_holders = crate::SessionHolders::default();
        let video = kind == ff::AVMEDIA_TYPE_VIDEO;
        // SAFETY: as above.
        let interlaced = video && crate::info::interlaced(unsafe { &*par });
        for cand in candidates(
            video,
            codec_id,
            format,
            hw,
            native,
            hevc_10bit_hardware(),
            interlaced,
        ) {
            let hardware = matches!(cand, Candidate::NativeV4l2 | Candidate::Named(_, true));
            let r = match cand {
                Candidate::NativeV4l2 => Self::open_native(par, time_base, hw),
                Candidate::Named(name, hardware) => {
                    // SAFETY: lookup returns a static descriptor or null.
                    let c = unsafe { ff::avcodec_find_decoder_by_name(name.as_ptr()) };
                    if c.is_null() {
                        continue;
                    }
                    Self::try_open(c, par, time_base, hardware, hw == HwDecode::Stills)
                }
                Candidate::Default => {
                    // SAFETY: as above.
                    let c = unsafe { ff::avcodec_find_decoder(codec_id) };
                    if c.is_null() {
                        continue;
                    }
                    Self::try_open(c, par, time_base, false, hw == HwDecode::Stills)
                }
            };
            match r {
                Ok(mut d) => {
                    if !d.hardware {
                        d.hardware_error = hardware_error;
                        d.hardware_holders = hardware_holders;
                    }
                    return Ok(d);
                }
                Err(e) => {
                    log::info!("decoder unavailable: {e}");
                    // The first hardware failure says most (the wrapper
                    // tried next fails the same way, less clearly).
                    if hardware && hardware_error.is_none() {
                        hardware_error = Some(e.to_string());
                        if let Error::HardwareBusy(_, holders) = &e {
                            hardware_holders = holders.clone();
                        }
                    }
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    #[cfg(target_os = "linux")]
    fn open_native(
        par: *const ff::AVCodecParameters,
        time_base: ff::AVRational,
        hw: HwDecode,
    ) -> Result<Decoder> {
        let dec = crate::v4l2::V4l2Decoder::open(par, time_base)?;
        // SAFETY: copies the parameters into a fresh allocation we own.
        let own = unsafe {
            let mut p = ff::avcodec_parameters_alloc();
            if p.is_null() || ff::avcodec_parameters_copy(p, par) < 0 {
                ff::avcodec_parameters_free(&mut p);
                return Err(Error::Unsupported("codec parameters copy".into()));
            }
            p
        };
        let name = format!("v4l2 ({})", dec.device);
        Ok(Decoder {
            ctx: std::ptr::null_mut(),
            native: Some(Box::new(Native {
                dec,
                par: own,
                hw,
                startup: Some(Vec::new()),
            })),
            replay: Default::default(),
            replay_end: false,
            name,
            hardware: true,
            hardware_error: None,
            hardware_holders: Default::default(),
            time_base,
            origin: 0.0,
            nal: None,
            skip: None,
            paired: None,
            left_out: 0,
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn open_native(
        _par: *const ff::AVCodecParameters,
        _time_base: ff::AVRational,
        _hw: HwDecode,
    ) -> Result<Decoder> {
        Err(Error::Unsupported("V4L2 needs Linux".into()))
    }

    fn try_open(
        codec: *const ff::AVCodec,
        par: *const ff::AVCodecParameters,
        time_base: ff::AVRational,
        hardware: bool,
        stills: bool,
    ) -> Result<Decoder> {
        // SAFETY: standard decoder setup; ctx freed by Decoder::drop or here.
        unsafe {
            let name = CStr::from_ptr((*codec).name).to_string_lossy().into_owned();
            let ctx = ff::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(Error::Unsupported("avcodec_alloc_context3".into()));
            }
            let mut dec = Decoder {
                ctx,
                #[cfg(target_os = "linux")]
                native: None,
                replay: Default::default(),
                replay_end: false,
                name,
                hardware,
                hardware_error: None,
                hardware_holders: Default::default(),
                time_base,
                origin: 0.0,
                nal: None,
                skip: None,
                paired: None,
                left_out: 0,
            };
            if hardware {
                HW_SESSIONS.fetch_add(1, Ordering::Relaxed); // until Drop
            }
            check(
                ff::avcodec_parameters_to_context(ctx, par),
                "decoder parameters",
            )?;
            (*ctx).pkt_timebase = time_base;
            if stills {
                // Only keyframes (the first picture after a seek is one),
                // skipping the rest unparsed past their headers, and no
                // deblocking: invisible once scaled down to a thumbnail.
                // One thread: frame threads would hold back pictures to
                // decode several at once, for nothing here.
                (*ctx).thread_count = 1;
                (*ctx).skip_frame = ff::AVDISCARD_NONINTRA;
                (*ctx).skip_loop_filter = ff::AVDISCARD_ALL;
                (*ctx).flags2 |= ff::AV_CODEC_FLAG2_FAST as i32;
            } else if !hardware {
                (*ctx).thread_count = 0; // auto
                (*ctx).thread_type = (ff::FF_THREAD_FRAME | ff::FF_THREAD_SLICE) as i32;
            }
            let mut opts: *mut ff::AVDictionary = std::ptr::null_mut();
            if hardware {
                // An 8K capture buffer is 48 MiB and the player's frame queue
                // holds its own copies, so a few beyond the driver's minimum
                // are enough (24 used to exhaust the decoder's memory).
                let k = CString::new("num_capture_buffers").unwrap_or_default();
                let v = CString::new("8").unwrap_or_default();
                ff::av_dict_set(&mut opts, k.as_ptr(), v.as_ptr(), 0);
            }
            let video = (*par).codec_type == ff::AVMEDIA_TYPE_VIDEO;
            let r = if video && !hardware && !stills {
                on_all_cpus(|| ff::avcodec_open2(ctx, codec, &mut opts))
            } else {
                ff::avcodec_open2(ctx, codec, &mut opts)
            };
            ff::av_dict_free(&mut opts);
            check(
                r,
                if hardware {
                    "open hardware decoder"
                } else {
                    "open decoder"
                },
            )?;
            dec.time_base = time_base;
            Ok(dec)
        }
    }

    pub fn as_ptr(&self) -> *mut ff::AVCodecContext {
        self.ctx
    }

    /// Replaces a failed native decoder with the next candidates (FFmpeg's
    /// V4L2 wrapper, then software), replaying startup packets into it.
    #[cfg(target_os = "linux")]
    fn fall_back(&mut self, why: &Error) -> Result<()> {
        let Some(mut native) = self.native.take() else {
            return Ok(());
        };
        log::warn!(
            "{}: {why}; falling back to FFmpeg decoding{}",
            self.name,
            if native.startup.is_some() {
                ""
            } else {
                " (resumes at the next keyframe)"
            }
        );
        let startup = native.startup.take().unwrap_or_default();
        // FFmpeg's V4L2 wrapper feeds the device the same way, so a stream
        // the hardware flags as broken goes straight to software.
        let hw = if native.dec.pictures_broken() {
            HwDecode::Off
        } else {
            native.hw
        };
        // Close the failed device session before opening the next decoder:
        // FFmpeg's V4L2 wrapper needs a session of its own, and the driver
        // refuses one more while Steam holds others, so it went to software.
        // SAFETY: allocation, freed below.
        let mut par = unsafe { ff::avcodec_parameters_alloc() };
        // SAFETY: par is non-null when copied into; native.par is valid
        // while native is.
        if par.is_null() || unsafe { ff::avcodec_parameters_copy(par, native.par) } < 0 {
            // SAFETY: null-safe free of our own allocation.
            unsafe { ff::avcodec_parameters_free(&mut par) };
            return Err(Error::Unsupported("codec parameters copy".into()));
        }
        drop(native);
        let next = Self::open_with(par, self.time_base, hw, false);
        // SAFETY: our copy; open_with copied what it needs.
        unsafe { ff::avcodec_parameters_free(&mut par) };
        let mut next = next?;
        std::mem::swap(&mut self.ctx, &mut next.ctx);
        self.name = std::mem::take(&mut next.name);
        self.hardware = next.hardware;
        self.hardware_error = (!next.hardware).then(|| why.to_string());
        self.replay = startup.into();
        log::info!("now decoding with {}", self.name);
        Ok(())
    }

    /// Sends a packet (or end of stream when `pkt` is null). Returns false if
    /// the decoder is full and frames must be received first.
    pub(crate) fn send(&mut self, pkt: *const ff::AVPacket) -> Result<bool> {
        let sent = match self.paired.as_mut() {
            Some(Paired::Mvc(m)) => Some(m.send(pkt)),
            Some(Paired::FrameSequential(f)) => Some(f.inner.send(pkt)),
            Some(Paired::Deinterlace(d)) => Some(d.inner.send(pkt)),
            None => None,
        };
        if let Some(r) = sent {
            self.follow_paired();
            return r;
        }
        if self.leaves_out(pkt) {
            self.left_out += 1;
            return Ok(true);
        }
        #[cfg(target_os = "linux")]
        if let Some(n) = self.native.as_mut() {
            match n.dec.send(pkt) {
                Ok(accepted) => {
                    if accepted
                        && !pkt.is_null()
                        && let Some(s) = n.startup.as_mut()
                    {
                        if s.len() < STARTUP_PACKETS {
                            let copy = Packet::new();
                            // SAFETY: adds a reference to the caller's packet.
                            if unsafe { ff::av_packet_ref(copy.as_ptr(), pkt) } >= 0 {
                                s.push(copy);
                            }
                        } else {
                            n.startup = None;
                        }
                    }
                    return Ok(accepted);
                }
                Err(e) => self.fall_back(&e)?,
            }
        }
        while let Some(p) = self.replay.front() {
            if !self.send_ffmpeg(p.as_ptr())? {
                return Ok(false);
            }
            self.replay.pop_front();
        }
        self.send_ffmpeg(pkt)
    }

    fn send_ffmpeg(&mut self, pkt: *const ff::AVPacket) -> Result<bool> {
        if self.ctx.is_null() {
            return Err(Error::Unsupported("decoder closed after a failure".into()));
        }
        // SAFETY: ctx is open; pkt valid or null.
        let r = unsafe { ff::avcodec_send_packet(self.ctx, pkt) };
        if r == ff::AVERROR_EAGAIN || r == ff::AVERROR_EOF_ {
            return Ok(false);
        }
        if r == ff::AVERROR_INVALIDDATA_ {
            log::debug!("{}: skipping invalid packet", self.name);
            return Ok(true);
        }
        check(r, "send packet")?;
        Ok(true)
    }

    /// Receives a frame. `Ok(None)` means more input is needed, `Ok(Some(false))`
    /// means the decoder is fully drained.
    pub fn receive(&mut self, frame: &mut Frame) -> Result<Option<bool>> {
        let received = match self.paired.as_mut() {
            Some(Paired::Mvc(m)) => Some(m.receive(frame)),
            Some(Paired::FrameSequential(f)) => Some(f.receive(frame)),
            Some(Paired::Deinterlace(d)) => Some(d.receive(frame)),
            None => None,
        };
        if let Some(r) = received {
            self.follow_paired();
            return r;
        }
        #[cfg(target_os = "linux")]
        if let Some(n) = self.native.as_mut() {
            let skip = self.skip;
            match n.dec.receive(frame.0, |pts, duration| {
                skip.is_none_or(|s| !s.drops(pts, duration))
            }) {
                Ok(r) => {
                    if r == Some(true) {
                        n.startup = None;
                        self.skip = None;
                    }
                    return Ok(r);
                }
                Err(e) => {
                    let ended = n.dec.ended();
                    self.fall_back(&e)?;
                    if !ended {
                        return Ok(None);
                    }
                    self.replay_end = true;
                }
            }
        }
        if self.ctx.is_null() {
            return Err(Error::Unsupported("decoder closed after a failure".into()));
        }
        loop {
            // SAFETY: ctx is open; frame valid.
            let r = unsafe { ff::avcodec_receive_frame(self.ctx, frame.0) };
            if r == ff::AVERROR_EAGAIN {
                if self.replay_end {
                    if self.send(std::ptr::null())? {
                        self.replay_end = false;
                    }
                    continue;
                }
                return Ok(None);
            }
            if r == ff::AVERROR_EOF_ {
                return Ok(Some(false));
            }
            check(r, "receive frame")?;
            if let Some(s) = self.skip {
                // SAFETY: a decoded frame, our reference to it dropped when
                // it is left out.
                unsafe {
                    let f = &*frame.0;
                    if s.drops(f.best_effort_timestamp, f.duration) {
                        ff::av_frame_unref(frame.0);
                        continue;
                    }
                }
                self.skip = None;
            }
            if self.hardware {
                copy_out(frame)?;
            }
            return Ok(Some(true));
        }
    }

    /// Leaves out what a precise seek to `target` (media seconds) throws
    /// away: frames shown mostly before it are dropped before they are
    /// copied, and the pictures among them that no other picture is decoded
    /// from are not decoded at all. Frames without a duration count as
    /// lasting `frame_duration`. Ends at the first frame kept, or a flush.
    pub(crate) fn skip_before(&mut self, target: f64, frame_duration: f64) {
        let tb = crate::q2d(self.time_base);
        self.skip = (tb > 0.0).then(|| Skip {
            target: (target + self.origin) / tb,
            frame_duration: frame_duration / tb,
            start: self.origin / tb,
        });
        match self.paired.as_mut() {
            Some(Paired::Mvc(m)) => m.skip = self.skip.take(),
            // Half a picture early: the pair showing the target starts with
            // a frame that may be mostly before it.
            Some(Paired::FrameSequential(f)) => {
                self.skip = None;
                f.inner
                    .skip_before(target - frame_duration / 2.0, frame_duration);
            }
            // Fields are left out as they come; frames a frame early, as
            // bwdif takes in the frame before the one it makes fields of.
            // `frame_duration` is then a field's.
            Some(Paired::Deinterlace(d)) => {
                d.skip = self.skip.take();
                d.inner
                    .skip_before(target - 2.0 * frame_duration, 2.0 * frame_duration);
            }
            None => {}
        }
    }

    /// Whether `pkt` holds only pictures a precise seek skips that no other
    /// picture needs.
    fn leaves_out(&mut self, pkt: *const ff::AVPacket) -> bool {
        let (Some(skip), Some(nal)) = (self.skip, self.nal.as_mut()) else {
            return false;
        };
        if pkt.is_null() {
            return false;
        }
        // SAFETY: a valid packet holding `size` bytes at `data`.
        unsafe {
            let p = &*pkt;
            if p.pts == ff::AV_NOPTS_VALUE || p.data.is_null() || !skip.drops(p.pts, p.duration) {
                return false;
            }
            nal.disposable(std::slice::from_raw_parts(p.data, p.size.max(0) as usize))
        }
    }

    /// Lets the native hardware decoder lend its buffers as shared frames
    /// while [`shared_frames`] is on: the player's video decoder only, whose
    /// frames go to the renderer alone. Nothing for 3D pairing, which works
    /// on copies.
    pub(crate) fn share_frames(&mut self) {
        if self.paired.is_some() {
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(n) = self.native.as_mut() {
            n.dec.share_frames();
        }
    }

    /// A flag that, once set, cuts the native decoder's waits for the
    /// device short (the player sets it when closing).
    pub(crate) fn set_interrupt(&mut self, flag: Arc<AtomicBool>) {
        if let Some(p) = self.paired.as_mut() {
            p.inner().set_interrupt(flag);
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(n) = self.native.as_mut() {
            n.dec.set_interrupt(flag);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = flag;
    }

    pub fn flush(&mut self) {
        self.skip = None;
        match self.paired.as_mut() {
            Some(Paired::Mvc(m)) => return m.flush(),
            Some(Paired::FrameSequential(f)) => return f.flush(),
            Some(Paired::Deinterlace(d)) => return d.flush(),
            None => {}
        }
        #[cfg(target_os = "linux")]
        if let Some(n) = self.native.as_mut() {
            n.dec.flush();
            if n.startup.is_some() {
                n.startup = Some(Vec::new());
            }
            return;
        }
        self.replay.clear();
        self.replay_end = false;
        if !self.ctx.is_null() {
            // SAFETY: ctx is open.
            unsafe { ff::avcodec_flush_buffers(self.ctx) };
        }
    }

    /// Best-effort presentation time of a decoded frame in seconds on the
    /// media's timeline.
    pub fn frame_time(&self, frame: &Frame) -> Option<f64> {
        // SAFETY: valid frame.
        let ts = unsafe { (*frame.0).best_effort_timestamp };
        (ts != ff::AV_NOPTS_VALUE).then(|| ts as f64 * crate::q2d(self.time_base) - self.origin)
    }

    pub fn frame_duration(&self, frame: &Frame) -> f64 {
        // SAFETY: valid frame.
        let d = unsafe { (*frame.0).duration };
        if d > 0 {
            d as f64 * crate::q2d(self.time_base)
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{FrameConverter, PixelLayout};
    use crate::input::Input;
    use fp_core::ByteSource;
    use fp_core::source::FileSource;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn decode_all(name: &str, max: usize) -> (String, Vec<crate::VideoFrame>) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name);
        let mut input = Input::open(
            Arc::new(FileSource::open(&path).unwrap()),
            name,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let vi = input.best_stream(ff::AVMEDIA_TYPE_VIDEO).unwrap();
        let mut dec = Decoder::open(&input, vi, HwDecode::Auto).unwrap();
        let mut conv = FrameConverter::default();
        let mut pkt = Packet::new();
        let mut frame = Frame::new();
        let mut out = Vec::new();
        let mut eof = false;
        while out.len() < max {
            if !eof {
                if input.read(pkt.as_ptr()).unwrap() {
                    if pkt.stream_index() != vi {
                        pkt.unref();
                        continue;
                    }
                    dec.send(pkt.as_ptr()).unwrap();
                    pkt.unref();
                } else {
                    eof = true;
                    dec.send(std::ptr::null()).unwrap();
                }
            }
            loop {
                match dec.receive(&mut frame).unwrap() {
                    Some(true) => {
                        let t = dec.frame_time(&frame).unwrap_or(0.0);
                        out.push(conv.convert(frame.take_raw(), t, 0.0, 0).unwrap());
                    }
                    Some(false) => return (dec.name.clone(), out),
                    None => break,
                }
            }
        }
        (dec.name.clone(), out)
    }

    struct Decoded {
        time: f64,
        duration: f64,
        /// A hash of the luma plane.
        luma: u64,
    }

    /// Every frame `src` decodes to in software, after skipping to `target`
    /// as a precise seek does when given, how many packets before the target
    /// hold pictures nothing else needs, and how many the decoder left out.
    fn decode_skipping(src: Arc<dyn ByteSource>, target: Option<f64>) -> (Vec<Decoded>, u64, u64) {
        use std::hash::{DefaultHasher, Hasher};
        let mut input = Input::open(src, "clip", Arc::new(AtomicBool::new(false))).unwrap();
        let vi = input.best_stream(ff::AVMEDIA_TYPE_VIDEO).unwrap();
        let mut dec = Decoder::open(&input, vi, HwDecode::Off).unwrap();
        let mut nal = dec.nal.unwrap();
        if let Some(t) = target {
            dec.skip_before(t, 1.0 / 30.0);
        }
        let (mut pkt, mut frame) = (Packet::new(), Frame::new());
        let (mut out, mut needless, mut eof) = (Vec::new(), 0, false);
        loop {
            if !eof {
                if input.read(pkt.as_ptr()).unwrap() {
                    if pkt.stream_index() != vi {
                        pkt.unref();
                        continue;
                    }
                    // SAFETY: a packet just read.
                    let (pts, data) = unsafe {
                        let p = &*pkt.as_ptr();
                        (p.pts, std::slice::from_raw_parts(p.data, p.size as usize))
                    };
                    let time = pts as f64 * crate::q2d(dec.time_base) - dec.origin;
                    if target.is_some_and(|t| time < t) && nal.disposable(data) {
                        needless += 1;
                    }
                    dec.send(pkt.as_ptr()).unwrap();
                    pkt.unref();
                } else {
                    eof = true;
                    dec.send(std::ptr::null()).unwrap();
                }
            }
            while let Some(more) = dec.receive(&mut frame).unwrap() {
                if !more {
                    return (out, needless, dec.left_out);
                }
                let mut h = DefaultHasher::new();
                // SAFETY: a decoded 8- or 10-bit planar picture.
                unsafe {
                    let f = &*frame.as_ptr();
                    let sample = if f.format == ff::AV_PIX_FMT_YUV420P {
                        1
                    } else {
                        2
                    };
                    for row in 0..f.height as usize {
                        let line = f.data[0].add(row * f.linesize[0] as usize);
                        h.write(std::slice::from_raw_parts(line, f.width as usize * sample));
                    }
                }
                out.push(Decoded {
                    time: dec.frame_time(&frame).unwrap(),
                    duration: dec.frame_duration(&frame),
                    luma: h.finish(),
                });
                // SAFETY: our reference.
                unsafe { ff::av_frame_unref(frame.as_ptr()) };
            }
        }
    }

    #[test]
    fn a_precise_seek_leaves_out_only_what_it_does_not_show() {
        let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
        // MP4 and MPEG-TS (Annex B) H.264, and HEVC in Matroska.
        for name in ["h264_aac_180_LR.mp4", "x.ts", "hevc10_tb.mkv"] {
            let src = || -> Arc<dyn ByteSource> {
                if name == "x.ts" {
                    crate::ts_fixture::h264_aac_ts(10.0)
                } else {
                    Arc::new(FileSource::open(&data.join(name)).unwrap())
                }
            };
            let (all, _, _) = decode_skipping(src(), None);
            let target = all[all.len() / 2].time - 0.001;
            let (kept, needless, left_out) = decode_skipping(src(), Some(target));
            assert!(needless > 0, "{name}: nothing to leave out");
            assert_eq!(left_out, needless, "{name}");
            let shown: Vec<_> = all
                .iter()
                .filter(|f| {
                    let duration = if f.duration > 0.0 {
                        f.duration
                    } else {
                        1.0 / 30.0
                    };
                    f.time + duration * 0.5 >= target
                })
                .collect();
            assert_eq!(kept.len(), shown.len(), "{name}");
            for (k, s) in kept.iter().zip(shown) {
                assert_eq!((k.time, k.luma), (s.time, s.luma), "{name}");
            }
        }
    }

    #[test]
    fn candidate_order_and_fallbacks() {
        use Candidate::*;
        let hevc = ff::AV_CODEC_ID_HEVC;
        let yuv420 = ff::AV_PIX_FMT_YUV420P;
        let yuv420p10 = ff::AV_PIX_FMT_YUV420P10LE;
        let auto = HwDecode::Auto;
        assert_eq!(
            candidates(true, hevc, yuv420, auto, true, false, false),
            vec![NativeV4l2, Named(c"hevc_v4l2m2m", true), Default]
        );
        // 10-bit HEVC: the native decoder (Q10C) when the renderer shows it,
        // never FFmpeg's wrapper.
        assert_eq!(
            candidates(true, hevc, yuv420p10, auto, true, false, false),
            vec![Default]
        );
        assert_eq!(
            candidates(true, hevc, yuv420p10, auto, true, true, false),
            vec![NativeV4l2, Default]
        );
        assert_eq!(
            candidates(true, hevc, yuv420p10, auto, false, true, false),
            vec![Default]
        );
        assert_eq!(
            candidates(true, hevc, yuv420p10, HwDecode::Off, true, true, false),
            vec![Default]
        );
        let h264 = ff::AV_CODEC_ID_H264;
        assert_eq!(
            candidates(true, h264, yuv420p10, auto, true, true, false),
            vec![Default]
        );
        // After the native decoder fails (or with it disabled).
        assert_eq!(
            candidates(true, hevc, yuv420, auto, false, false, false),
            vec![Named(c"hevc_v4l2m2m", true), Default]
        );
        assert_eq!(
            candidates(true, h264, yuv420, auto, true, false, false)[0],
            NativeV4l2
        );
        // Interlaced H.264 (1080i): software only.
        assert_eq!(
            candidates(true, h264, yuv420, auto, true, false, true),
            vec![Default]
        );
        assert_eq!(
            candidates(true, hevc, yuv420, HwDecode::Off, true, false, false),
            vec![Default]
        );
        // Only HEVC and H.264 have a hardware path.
        for codec in [
            ff::AV_CODEC_ID_VP9,
            ff::AV_CODEC_ID_VP8,
            ff::AV_CODEC_ID_MPEG4,
        ] {
            assert_eq!(
                candidates(true, codec, yuv420, auto, true, false, false),
                vec![Default]
            );
        }
        assert_eq!(
            candidates(true, ff::AV_CODEC_ID_AV1, yuv420, auto, true, false, false),
            vec![Named(c"libdav1d", false), Default]
        );
        let aac = ff::AV_CODEC_ID_AAC;
        assert_eq!(
            candidates(false, aac, ff::AV_PIX_FMT_NONE, auto, true, false, false),
            vec![Default]
        );
    }

    #[test]
    fn decodes_h264() {
        let (name, frames) = decode_all("h264_aac_180_LR.mp4", 100);
        assert_eq!(name, "h264", "no V4L2 device here, so software");
        assert_eq!(frames.len(), 60);
        let f = &frames[10];
        assert_eq!(
            (f.width, f.height, f.layout),
            (640, 320, PixelLayout::I420 { bits: 8 })
        );
        assert!((frames[30].pts - 1.0).abs() < 0.04, "{}", frames[30].pts);
        let planes = f.packed_planes();
        assert_eq!(planes[0].len(), 640 * 320);
        assert_eq!(planes[1].len(), 320 * 160);
        // testsrc2 is colourful: luma must vary.
        let (mn, mx) = planes[0]
            .iter()
            .fold((255u8, 0u8), |(a, b), &v| (a.min(v), b.max(v)));
        assert!(mx - mn > 100);
    }

    #[test]
    fn stills_decode_only_keyframes() {
        // Keyframes at 0 and 1 s of 60 frames; one of 25 frames.
        for (name, keyframes) in [("h264_aac_180_LR.mp4", 2), ("hevc10_tb.mkv", 1)] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/data")
                .join(name);
            let mut input = Input::open(
                Arc::new(FileSource::open(&path).unwrap()),
                name,
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
            let vi = input.best_stream(ff::AVMEDIA_TYPE_VIDEO).unwrap();
            let mut dec = Decoder::open(&input, vi, HwDecode::Stills).unwrap();
            assert!(!dec.hardware);
            let (mut pkt, mut frame) = (Packet::new(), Frame::new());
            let mut frames = 0;
            let mut more = true;
            while more {
                more = input.read(pkt.as_ptr()).unwrap();
                if !more {
                    dec.send(std::ptr::null()).unwrap();
                } else if pkt.stream_index() == vi {
                    dec.send(pkt.as_ptr()).unwrap();
                }
                pkt.unref();
                while let Some(true) = dec.receive(&mut frame).unwrap() {
                    frames += 1;
                }
            }
            assert_eq!(frames, keyframes, "{name}");
        }
    }

    #[test]
    fn failed_hardware_opens_hold_no_session() {
        // No V4L2 device here: FFmpeg's hardware decoder is tried and fails,
        // and software decodes. Nothing may stay counted afterwards (other
        // tests open decoders at the same time, so allow them to finish).
        let (name, _) = decode_all("h264_aac_180_LR.mp4", 5);
        assert_eq!(name, "h264");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while hardware_sessions() != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(hardware_sessions(), 0);
    }

    #[test]
    fn hardware_frames_are_copied_out_unchanged() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/h264_aac_180_LR.mp4");
        let mut input = Input::open(
            Arc::new(FileSource::open(&path).unwrap()),
            "x.mp4",
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let vi = input.best_stream(ff::AVMEDIA_TYPE_VIDEO).unwrap();
        let mut dec = Decoder::open(&input, vi, HwDecode::Off).unwrap();
        let mut pkt = Packet::new();
        let mut frame = Frame::new();
        while dec.receive(&mut frame).unwrap() != Some(true) {
            assert!(input.read(pkt.as_ptr()).unwrap());
            if pkt.stream_index() == vi {
                dec.send(pkt.as_ptr()).unwrap();
            }
            pkt.unref();
        }
        let mut conv = FrameConverter::default();
        let mut copy = Frame::new();
        // SAFETY: both frames are valid allocations; frame holds a decoded
        // picture.
        let before = unsafe {
            ff::av_frame_ref(copy.as_ptr(), frame.as_ptr());
            (*frame.as_ptr()).buf[0]
        };
        copy_out(&mut copy).unwrap();
        // SAFETY: as above.
        let (buf, pts, frame_pts) = unsafe {
            (
                (*copy.as_ptr()).buf[0],
                (*copy.as_ptr()).pts,
                (*frame.as_ptr()).pts,
            )
        };
        assert_ne!(buf, before, "own buffer");
        assert_eq!(pts, frame_pts);
        let a = conv.convert(frame.take_raw(), 0.0, 0.0, 0).unwrap();
        let b = conv.convert(copy.take_raw(), 0.0, 0.0, 0).unwrap();
        assert_eq!((a.width, a.height, a.layout), (b.width, b.height, b.layout));
        assert_eq!(a.packed_planes(), b.packed_planes());
    }

    #[test]
    fn decodes_hevc_10bit() {
        let (_, frames) = decode_all("hevc10_tb.mkv", 5);
        assert_eq!(frames[0].layout, PixelLayout::I420 { bits: 10 });
        let y = &frames[0].packed_planes()[0];
        assert_eq!(y.len(), 512 * 512 * 2);
        let max = y
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .max()
            .unwrap();
        assert!(
            max <= 1023 && max > 600,
            "10-bit values in low bits, got max {max}"
        );
    }

    #[test]
    fn decodes_av1_with_dav1d_and_vp9() {
        let (name, frames) = decode_all("av1_opus.webm", 24);
        assert_eq!(name, "libdav1d");
        assert_eq!(frames.len(), 24);
        let (_, v) = decode_all("vp9.webm", 5);
        assert_eq!(v[0].width, 320);
    }
}
