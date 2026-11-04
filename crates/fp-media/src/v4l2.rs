//! Native V4L2 stateful (memory-to-memory) video decoder for H.264 and HEVC.
//!
//! FFmpeg's `*_v4l2m2m` wrappers cannot open large streams on the Steam
//! Frame: the qcom iris driver's admission check (`check_core_mbps_mbpf`)
//! multiplies the *new* session's macroblocks per frame by the number of
//! open instances, idle ones and encoders too (Steam's own `vrlink` and its
//! recording among them), so an 8192x4096 stream fails `VIDIOC_REQBUFS` with
//! `ENOMEM` as soon as one other instance exists, although the hardware
//! decodes it at well over 60 fps. Past [`MAX_SESSION_MACROBLOCKS`] it fails
//! with nothing else open.
//!
//! This decoder drives the device directly and works around the check:
//!
//! 1. The OUTPUT (bitstream) queue is configured for the stream's size. When
//!    `REQBUFS` fails with `ENOMEM`, the declared height is lowered to the
//!    largest 32-aligned height the driver accepts ([`capped_height`]).
//! 2. In that case an embedded IDR picture (the "primer") is queued ahead of
//!    the stream: a black HEVC frame of 8192x4080, which has just few enough
//!    macroblocks to pass the check when CAPTURE is allocated, yet needs the
//!    same 8192x4096 buffers as the stream ([`primer_matches`]).
//! 3. When the real stream's `SOURCE_CHANGE` arrives, the existing CAPTURE
//!    buffers are reused ([`can_reuse_capture`]): decoding resumes with
//!    `V4L2_DEC_CMD_START`, which skips the check `STREAMON` would run again.
//!
//! Reuse is only allowed when the new CAPTURE format is exactly the one the
//! buffers were allocated for. Resuming a much larger stream on buffers of a
//! smaller one (even with big enough CAPTURE buffers, e.g. imported DMA-BUFs)
//! makes the firmware write past its internal buffers: SMMU faults and a
//! firmware restart that takes down every other decoder session (tested).
//! Any other resolution change reallocates CAPTURE the standard way.
//!
//! Seeks never restart a queue (that would re-run the check): queued
//! bitstream from before the seek is decoded and its frames are dropped by
//! timestamp. Decoded frames are copied out of the driver's buffers into
//! pooled `AVFrame`s, so the CAPTURE buffers go straight back to the decoder.
//!
//! 10-bit HEVC comes out correctly only as Q10C, Qualcomm's compressed TP10
//! ([`Q10cLayout`]): asked for NV12, the decoder clamps the 10-bit samples
//! to 8 bits instead of scaling them. Q10C pictures are copied out whole
//! and only the GPU reads them. They get no primer: the driver sizes Q10C
//! CAPTURE buffers by the exact picture height (8192x4080 and 8192x4096
//! differ) and ignores a requested one, so a 10-bit 8K stream the admission
//! check refuses decodes in software.

/// Bitstream (OUTPUT) buffers to request.
pub(crate) const OUTPUT_BUFFERS: u32 = 6;
/// Decoded-picture (CAPTURE) buffers to request (at least the driver minimum).
pub(crate) const CAPTURE_BUFFERS: u32 = 8;
/// Heights are lowered in steps the driver keeps without rounding up.
pub(crate) const HEIGHT_STEP: u32 = 32;
/// The smallest declared OUTPUT height worth trying: below this the
/// bitstream buffers get too small for large streams' packets.
pub(crate) const MIN_DECLARED_HEIGHT: u32 = 512;
/// What the admission check takes for one session, whatever else is open
/// (8192x4320, measured on the Frame).
pub(crate) const MAX_SESSION_MACROBLOCKS: u64 = 138_240;
/// What it takes for the new session's macroblocks times the sessions open.
pub(crate) const MAX_CORE_MACROBLOCKS: u64 = 261_120;

/// Codecs this decoder handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
    H264,
    Hevc,
}

impl Codec {
    pub(crate) fn from_ffmpeg(id: fp_ffmpeg_sys::AVCodecID) -> Option<Codec> {
        match id {
            fp_ffmpeg_sys::AV_CODEC_ID_H264 => Some(Codec::H264),
            fp_ffmpeg_sys::AV_CODEC_ID_HEVC => Some(Codec::Hevc),
            _ => None,
        }
    }

    pub(crate) fn fourcc(self) -> u32 {
        match self {
            Codec::H264 => fourcc(b"H264"),
            Codec::Hevc => fourcc(b"HEVC"),
        }
    }

    /// The bitstream filter turning MP4/MKV length-prefixed NAL units into
    /// the Annex B byte stream V4L2 decoders take.
    pub(crate) fn annexb_filter(self) -> &'static std::ffi::CStr {
        match self {
            Codec::H264 => c"h264_mp4toannexb",
            Codec::Hevc => c"hevc_mp4toannexb",
        }
    }

    /// The primer: one black IDR picture (Annex B with parameter sets, no
    /// SEI) and its size. Only HEVC has one; 8K H.264 is not something the
    /// Frame needs. Generated with
    /// `ffmpeg -f lavfi -i color=c=black:s=8192x4080:r=1 -frames:v 1
    /// -pix_fmt yuv420p -c:v libx265 -profile:v main -x265-params info=0 -f hevc`.
    pub(crate) fn primer(self) -> Option<(&'static [u8], (u32, u32))> {
        match self {
            Codec::H264 => None,
            Codec::Hevc => Some((
                include_bytes!("../assets/primer_8192x4080.hevc"),
                (8192, 4080),
            )),
        }
    }
}

pub(crate) const fn fourcc(c: &[u8; 4]) -> u32 {
    (c[0] as u32) | ((c[1] as u32) << 8) | ((c[2] as u32) << 16) | ((c[3] as u32) << 24)
}

/// 16x16 macroblocks in a frame, as the driver's admission check counts them.
pub(crate) fn macroblocks(width: u32, height: u32) -> u64 {
    width.div_ceil(16) as u64 * height.div_ceil(16) as u64
}

/// The largest height, a multiple of [`HEIGHT_STEP`] below `height` and at
/// least `min_height`, for which `fits` holds. `fits` is assumed monotonic
/// (if a height fits, every smaller one does too); it is called O(log n)
/// times. `None` when not even `min_height` fits.
pub(crate) fn capped_height(
    height: u32,
    min_height: u32,
    mut fits: impl FnMut(u32) -> bool,
) -> Option<u32> {
    let lo_steps = min_height.div_ceil(HEIGHT_STEP).max(1);
    // Strictly below `height`: the full height already failed.
    let hi_steps = height.saturating_sub(1) / HEIGHT_STEP;
    if hi_steps < lo_steps || !fits(lo_steps * HEIGHT_STEP) {
        return None;
    }
    let (mut good, mut bad) = (lo_steps, hi_steps + 1);
    while bad - good > 1 {
        let mid = good + (bad - good) / 2;
        if fits(mid * HEIGHT_STEP) {
            good = mid;
        } else {
            bad = mid;
        }
    }
    Some(good * HEIGHT_STEP)
}

/// Whether a primer of size `primer` can stand in for a stream of size
/// `stream`: same width, the same height once aligned to 64-line coding tree
/// blocks (so CAPTURE and internal buffers are identical), and fewer
/// macroblocks (so it passes the admission check where the stream fails).
pub(crate) fn primer_matches(stream: (u32, u32), primer: (u32, u32)) -> bool {
    stream.0 == primer.0
        && stream.1.next_multiple_of(64) == primer.1.next_multiple_of(64)
        && macroblocks(primer.0, primer.1) < macroblocks(stream.0, stream.1)
}

/// Whether the admission check takes a stream beside `others` other sessions:
/// as it is or, when it has a `primer`, at the largest aligned height up to
/// the primer's.
pub(crate) fn fits_beside(stream: (u32, u32), primer: Option<(u32, u32)>, others: u64) -> bool {
    let admits = |w, h| {
        let mb = macroblocks(w, h);
        mb <= MAX_SESSION_MACROBLOCKS && (others + 1) * mb <= MAX_CORE_MACROBLOCKS
    };
    admits(stream.0, stream.1)
        || primer.is_some_and(|p| admits(p.0, p.1 / HEIGHT_STEP * HEIGHT_STEP))
}

/// A program with sessions open on the driver, and on which of its devices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Holder {
    pub name: String,
    pub decoder: u64,
    pub encoder: u64,
}

impl Holder {
    fn sessions(&self) -> u64 {
        self.decoder + self.encoder
    }
}

/// Pictures in a row the decoder may flag as broken before it counts as
/// failed. A seek can leave a few leading pictures it can't decode; a stream
/// the hardware misreads has every picture flagged.
pub(crate) const MAX_BROKEN_PICTURES: u32 = 48;

/// The part of a packet to rotate right by `extradata.len()` so the
/// parameter sets `hevc_mp4toannexb` copied in from the hvcC (`extradata`,
/// Annex B) go ahead of the VPS, SPS and PPS the packet carries itself,
/// and those are the ones the decoder ends up with. `None` when nothing
/// needs to move.
///
/// The filter inserts the hvcC's right before a packet's first IRAP
/// picture, after any in-band ones, whether or not the packet is flagged
/// as a keyframe. Software decoders read the hvcC once,
/// so in-band parameter sets replace it, and a file whose hvcC doesn't
/// match its pictures (some Matroska re-encodes) plays everywhere else; the
/// Frame's decoder takes the stale copy and flags every picture as broken.
pub(crate) fn hvcc_copy_after_own(data: &[u8], extradata: &[u8]) -> Option<std::ops::Range<usize>> {
    if extradata.is_empty() {
        return None;
    }
    // From start code to start code, reading only each unit's header, up
    // to the first picture: the rest is the bulk of the packet, and only an
    // IRAP picture gets the hvcC's copy.
    let (mut own, mut at) = (None, 0);
    let irap = loop {
        let code = at + crate::nal::start_code(&data[at..])?;
        let begin = if code > 0 && data[code - 1] == 0 {
            code - 1
        } else {
            code
        };
        match (data.get(code + 3)? >> 1) & 0x3f {
            16..=23 => break begin,
            0..=15 | 24..=31 => return None,
            32..=34 => own = own.or(Some(begin)),
            _ => {}
        }
        at = code + 3;
    };
    let inserted = irap
        .checked_sub(extradata.len())
        .filter(|&at| data[at..irap] == *extradata)?;
    let own = own.filter(|&own| own < inserted)?;
    // Most files repeat the hvcC's own parameter sets: nothing to move.
    (data[own..inserted] != *extradata).then_some(own..irap)
}

/// The error for a stream of size `stream` the admission check refuses,
/// `admitted` being the largest height it takes at that width beside the
/// `holders`' sessions, if any.
pub(crate) fn refused(
    stream: (u32, u32),
    ten_bit: bool,
    admitted: Option<u32>,
    primer: Option<(u32, u32)>,
    holders: &[Holder],
) -> crate::Error {
    let (w, h) = stream;
    let depth = if ten_bit { " 10-bit" } else { "" };
    if macroblocks(w, h) > MAX_SESSION_MACROBLOCKS {
        return crate::Error::HardwareTooBig(format!(
            "{w}x{h}{depth}, even with nothing else open"
        ));
    }
    let names = holders
        .iter()
        .map(|p| match p.sessions() {
            1 => p.name.clone(),
            n => format!("{} x{n}", p.name),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let message = match (names.is_empty(), admitted) {
        (true, Some(c)) => {
            format!("other video sessions leave room for {w}x{c}, not {w}x{h}{depth}")
        }
        (true, None) => "no room for another video session".into(),
        (false, Some(c)) => {
            format!("held by {names}, leaving room for {w}x{c}, not {w}x{h}{depth}")
        }
        (false, None) => format!("held by {names}, leaving no room for another video session"),
    };
    let sessions: u64 = holders.iter().map(Holder::sessions).sum();
    let steam_encoder: u64 = holders
        .iter()
        .filter(|p| p.name == "steam")
        .map(|p| p.encoder)
        .sum();
    crate::Error::HardwareBusy(
        message,
        crate::SessionHolders {
            programs: holders.iter().map(|p| p.name.clone()).collect(),
            recording_blocks: steam_encoder > 0
                && fits_beside(stream, primer, sessions - steam_encoder),
        },
    )
}

/// Whether CAPTURE buffers already allocated can serve the new stream format
/// after a resolution change, so decoding can resume without `REQBUFS`: the
/// coded size must be the one they were allocated for, every buffer large
/// enough, and enough of them.
pub(crate) fn can_reuse_capture(
    allocated_for: (u32, u32),
    have_count: u32,
    smallest_buffer: usize,
    new_format: (u32, u32),
    need_min_count: u32,
    need_size: usize,
) -> bool {
    allocated_for == new_format
        && have_count > 0
        && have_count >= need_min_count
        && need_size > 0
        && smallest_buffer >= need_size
}

/// How an NV12 picture sits in a single-plane CAPTURE buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Nv12Layout {
    /// Visible size.
    pub width: u32,
    pub height: u32,
    /// Bytes per row of both planes.
    pub stride: usize,
    /// Offset of the interleaved UV plane.
    pub uv_offset: usize,
}

impl Nv12Layout {
    /// From the CAPTURE format (`fmt_height` is the buffer's luma height,
    /// already aligned by the driver) and the visible (compose) size.
    pub(crate) fn new(
        stride: usize,
        fmt_height: u32,
        visible: (u32, u32),
        sizeimage: usize,
    ) -> Option<Nv12Layout> {
        let (width, height) = visible;
        let l = Nv12Layout {
            width,
            height,
            stride,
            uv_offset: stride * fmt_height.max(height) as usize,
        };
        (width > 0 && height > 0 && stride >= width as usize && l.end() <= sizeimage).then_some(l)
    }

    /// Bytes from the start of the buffer to the end of the visible chroma.
    pub(crate) fn end(&self) -> usize {
        self.uv_offset + self.stride * self.height.div_ceil(2) as usize
    }

    /// Bytes of a tightly stacked copy (Y rows then UV rows, same stride).
    pub(crate) fn copy_size(&self) -> usize {
        self.stride * (self.height as usize + self.height.div_ceil(2) as usize)
    }
}

/// How a Q10C picture sits in a single-plane CAPTURE buffer: Y metadata, Y,
/// UV metadata and UV, each 4096-aligned, three 10-bit samples in four
/// bytes. The driver reports only the stride and the total; the plane sizes
/// follow Qualcomm's rules for the format (as in the kernel's venus and iris
/// drivers), checked against that total.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Q10cLayout {
    /// Visible size.
    pub width: u32,
    pub height: u32,
    /// Bytes per row of both planes.
    pub stride: usize,
    /// Luma rows of the buffer (the CAPTURE format's height).
    pub rows: u32,
    /// Offset of the UV plane's metadata.
    pub uv_offset: usize,
    /// Bytes of the whole picture.
    pub size: usize,
}

impl Q10cLayout {
    /// From the CAPTURE format (`fmt` its width and height, already aligned
    /// by the driver, then its stride and size) and the visible (compose)
    /// size.
    pub(crate) fn new(
        fmt: (u32, u32),
        stride: usize,
        sizeimage: usize,
        visible: (u32, u32),
    ) -> Option<Q10cLayout> {
        let a = usize::next_multiple_of;
        let (w, h) = (fmt.0 as usize, fmt.1 as usize);
        let y_meta = a(a(w.div_ceil(48), 64) * a(h.div_ceil(4), 16), 4096);
        let y = a(stride * a(h, 16), 4096);
        let uv_meta = a(
            a(w.div_ceil(2).div_ceil(24), 64) * a(h.div_ceil(2).div_ceil(4), 16),
            4096,
        );
        let uv = a(stride * a(h.div_ceil(2), 16), 4096);
        let l = Q10cLayout {
            width: visible.0,
            height: visible.1,
            stride,
            rows: fmt.1,
            uv_offset: y_meta + y,
            size: y_meta + y + uv_meta + uv,
        };
        let fits = visible.0 > 0 && visible.1 > 0 && visible.0 <= fmt.0 && visible.1 <= fmt.1;
        (fits && stride == a(a(w, 192) * 4 / 3, 256) && l.size == sizeimage).then_some(l)
    }
}

/// Maps the V4L2 buffer timestamps we hand out to packet timing. Ids count
/// up from 1; 0 marks the primer. Seeks raise a floor below which ids are
/// stale.
#[derive(Default)]
pub(crate) struct Timestamps {
    next: u64,
    floor: u64,
    map: std::collections::BTreeMap<u64, (i64, i64)>,
}

/// Ids kept before the oldest entries are forgotten (packets that never
/// produced a frame, e.g. dropped by the decoder).
const TIMESTAMPS_KEPT: usize = 256;

impl Timestamps {
    pub(crate) fn insert(&mut self, pts: i64, duration: i64) -> u64 {
        self.next = self.next.max(self.floor) + 1;
        self.map.insert(self.next, (pts, duration));
        while self.map.len() > TIMESTAMPS_KEPT {
            self.map.pop_first();
        }
        self.next
    }

    /// Timing of a decoded frame; `None` for the primer and stale ids.
    pub(crate) fn get(&self, id: u64) -> Option<(i64, i64)> {
        if id <= self.floor {
            return None;
        }
        self.map.get(&id).copied()
    }

    /// Forgets everything queued so far (a seek).
    pub(crate) fn invalidate(&mut self) {
        self.floor = self.next;
        self.map.clear();
    }

    /// V4L2 timestamps are a `timeval`; ids travel in it unchanged.
    pub(crate) fn to_timeval(id: u64) -> (i64, i64) {
        ((id / 1_000_000) as i64, (id % 1_000_000) as i64)
    }

    pub(crate) fn from_timeval(sec: i64, usec: i64) -> u64 {
        (sec.max(0) as u64) * 1_000_000 + usec.max(0) as u64
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::V4l2Decoder;

/// Whether the native decoder may be tried at all: Linux only, and
/// `FRAMEPLAYERX_V4L2=0` turns it off.
pub(crate) fn enabled() -> bool {
    cfg!(target_os = "linux") && !matches!(std::env::var("FRAMEPLAYERX_V4L2").as_deref(), Ok("0"))
}

#[cfg(target_os = "linux")]
mod sys {
    //! The slice of `linux/videodev2.h` this decoder uses (64-bit layouts).

    pub const BUF_TYPE_CAPTURE: u32 = 1;
    pub const BUF_TYPE_CAPTURE_MPLANE: u32 = 9;
    pub const BUF_TYPE_OUTPUT_MPLANE: u32 = 10;
    pub const MEMORY_MMAP: u32 = 1;
    pub const FIELD_NONE: u32 = 1;

    pub const CAP_VIDEO_M2M_MPLANE: u32 = 0x4000;
    pub const CAP_STREAMING: u32 = 0x0400_0000;
    pub const CAP_DEVICE_CAPS: u32 = 0x8000_0000;

    pub const BUF_FLAG_ERROR: u32 = 0x40;
    pub const BUF_FLAG_LAST: u32 = 0x0010_0000;

    pub const EVENT_EOS: u32 = 2;
    pub const EVENT_SOURCE_CHANGE: u32 = 5;

    pub const DEC_CMD_START: u32 = 0;
    pub const DEC_CMD_STOP: u32 = 1;

    pub const CID_MIN_BUFFERS_FOR_CAPTURE: u32 = 0x0098_0900 + 39;
    pub const SEL_TGT_COMPOSE: u32 = 0x0100;

    #[repr(C)]
    #[derive(Default)]
    pub struct Capability {
        pub driver: [u8; 16],
        pub card: [u8; 32],
        pub bus_info: [u8; 32],
        pub version: u32,
        pub capabilities: u32,
        pub device_caps: u32,
        pub reserved: [u32; 3],
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct FmtDesc {
        pub index: u32,
        pub type_: u32,
        pub flags: u32,
        pub description: [u8; 32],
        pub pixelformat: u32,
        pub mbus_code: u32,
        pub reserved: [u32; 3],
    }

    /// `struct v4l2_format`; the union starts at offset 8 on 64-bit.
    #[repr(C)]
    pub struct Format {
        pub type_: u32,
        pub pad: u32,
        pub raw: [u8; 200],
    }

    impl Format {
        pub fn new(type_: u32) -> Format {
            Format {
                type_,
                pad: 0,
                raw: [0; 200],
            }
        }
        fn u32_at(&self, o: usize) -> u32 {
            u32::from_ne_bytes([
                self.raw[o],
                self.raw[o + 1],
                self.raw[o + 2],
                self.raw[o + 3],
            ])
        }
        fn set_u32_at(&mut self, o: usize, v: u32) {
            self.raw[o..o + 4].copy_from_slice(&v.to_ne_bytes());
        }
        // struct v4l2_pix_format_mplane (packed): width, height, pixelformat,
        // field, colorspace, plane_fmt[8] {sizeimage, bytesperline, u16[6]},
        // num_planes, ...
        pub fn width(&self) -> u32 {
            self.u32_at(0)
        }
        pub fn height(&self) -> u32 {
            self.u32_at(4)
        }
        pub fn pixelformat(&self) -> u32 {
            self.u32_at(8)
        }
        pub fn sizeimage(&self, plane: usize) -> u32 {
            self.u32_at(20 + 20 * plane)
        }
        pub fn bytesperline(&self, plane: usize) -> u32 {
            self.u32_at(24 + 20 * plane)
        }
        pub fn num_planes(&self) -> u8 {
            self.raw[180]
        }
        pub fn set_size(&mut self, w: u32, h: u32) {
            self.set_u32_at(0, w);
            self.set_u32_at(4, h);
        }
        pub fn set_pixelformat(&mut self, f: u32) {
            self.set_u32_at(8, f);
        }
        pub fn set_field(&mut self, f: u32) {
            self.set_u32_at(12, f);
        }
        pub fn set_num_planes(&mut self, n: u8) {
            self.raw[180] = n;
        }
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct RequestBuffers {
        pub count: u32,
        pub type_: u32,
        pub memory: u32,
        pub capabilities: u32,
        pub flags: u8,
        pub reserved: [u8; 3],
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct Plane {
        pub bytesused: u32,
        pub length: u32,
        /// Union of mem_offset (u32), userptr and fd; mem_offset is the low
        /// 32 bits on little-endian machines.
        pub m: u64,
        pub data_offset: u32,
        pub reserved: [u32; 11],
    }

    #[repr(C)]
    pub struct Buffer {
        pub index: u32,
        pub type_: u32,
        pub bytesused: u32,
        pub flags: u32,
        pub field: u32,
        pub pad0: u32,
        /// struct timeval {tv_sec, tv_usec}.
        pub timestamp: [i64; 2],
        pub timecode: [u32; 4],
        pub sequence: u32,
        pub memory: u32,
        /// Pointer to the planes array for multi-planar buffers.
        pub m: usize,
        pub length: u32,
        pub reserved2: u32,
        pub request_fd: u32,
        pub pad1: u32,
    }

    impl Buffer {
        /// A multi-planar MMAP buffer descriptor pointing at `planes`.
        pub fn new(type_: u32, index: u32, planes: &mut [Plane]) -> Buffer {
            Buffer {
                index,
                type_,
                bytesused: 0,
                flags: 0,
                field: 0,
                pad0: 0,
                timestamp: [0; 2],
                timecode: [0; 4],
                sequence: 0,
                memory: MEMORY_MMAP,
                m: planes.as_mut_ptr() as usize,
                length: planes.len() as u32,
                reserved2: 0,
                request_fd: 0,
                pad1: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct EventSubscription {
        pub type_: u32,
        pub id: u32,
        pub flags: u32,
        pub reserved: [u32; 5],
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct Event {
        pub type_: u32,
        pub pad: u32,
        pub u: [u64; 8],
        pub pending: u32,
        pub sequence: u32,
        pub timestamp: [i64; 2],
        pub id: u32,
        pub reserved: [u32; 8],
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct DecoderCmd {
        pub cmd: u32,
        pub flags: u32,
        pub raw: [u32; 16],
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct Control {
        pub id: u32,
        pub value: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct Selection {
        pub type_: u32,
        pub target: u32,
        pub flags: u32,
        pub left: i32,
        pub top: i32,
        pub width: u32,
        pub height: u32,
        pub reserved: [u32; 9],
    }

    const WRITE: u64 = 1;
    const READ: u64 = 2;
    const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
        (dir << 30) | ((size as u64) << 16) | ((b'V' as u64) << 8) | nr
    }
    const fn ior<T>(nr: u64) -> u64 {
        ioc(READ, nr, size_of::<T>())
    }
    const fn iow<T>(nr: u64) -> u64 {
        ioc(WRITE, nr, size_of::<T>())
    }
    const fn iowr<T>(nr: u64) -> u64 {
        ioc(READ | WRITE, nr, size_of::<T>())
    }

    pub const VIDIOC_QUERYCAP: u64 = ior::<Capability>(0);
    pub const VIDIOC_ENUM_FMT: u64 = iowr::<FmtDesc>(2);
    pub const VIDIOC_G_FMT: u64 = iowr::<Format>(4);
    pub const VIDIOC_S_FMT: u64 = iowr::<Format>(5);
    pub const VIDIOC_REQBUFS: u64 = iowr::<RequestBuffers>(8);
    pub const VIDIOC_QUERYBUF: u64 = iowr::<Buffer>(9);
    pub const VIDIOC_QBUF: u64 = iowr::<Buffer>(15);
    pub const VIDIOC_DQBUF: u64 = iowr::<Buffer>(17);
    pub const VIDIOC_STREAMON: u64 = iow::<i32>(18);
    pub const VIDIOC_STREAMOFF: u64 = iow::<i32>(19);
    pub const VIDIOC_G_CTRL: u64 = iowr::<Control>(27);
    pub const VIDIOC_DQEVENT: u64 = ior::<Event>(89);
    pub const VIDIOC_SUBSCRIBE_EVENT: u64 = iow::<EventSubscription>(90);
    pub const VIDIOC_G_SELECTION: u64 = iowr::<Selection>(94);
    pub const VIDIOC_DECODER_CMD: u64 = iowr::<DecoderCmd>(96);

    // The kernel's sizes; the ioctl numbers encode them.
    const _: () = assert!(size_of::<Capability>() == 104);
    const _: () = assert!(size_of::<FmtDesc>() == 64);
    const _: () = assert!(size_of::<Format>() == 208);
    const _: () = assert!(size_of::<RequestBuffers>() == 20);
    const _: () = assert!(size_of::<Plane>() == 64);
    const _: () = assert!(size_of::<Buffer>() == 88);
    const _: () = assert!(size_of::<EventSubscription>() == 32);
    const _: () = assert!(size_of::<Event>() == 136);
    const _: () = assert!(size_of::<DecoderCmd>() == 72);
    const _: () = assert!(size_of::<Control>() == 8);
    const _: () = assert!(size_of::<Selection>() == 64);

    // Known values from videodev2.h, as a check on the encoding.
    const _: () = assert!(VIDIOC_QUERYCAP == 0x8068_5600);
    const _: () = assert!(VIDIOC_S_FMT == 0xc0d0_5605);
    const _: () = assert!(VIDIOC_QBUF == 0xc058_560f);
    const _: () = assert!(VIDIOC_STREAMON == 0x4004_5612);
    const _: () = assert!(VIDIOC_DQEVENT == 0x8088_5659);
}

#[cfg(target_os = "linux")]
mod linux {
    use super::sys::*;
    use super::*;
    use crate::decode::Packet;
    use crate::{Error, Result};
    use fp_ffmpeg_sys as ff;
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    const NV12: u32 = fourcc(b"NV12");
    const Q10C: u32 = fourcc(b"Q10C");
    /// Give up when the device makes no progress for this long.
    const STALL: Duration = Duration::from_secs(5);

    fn ioctl<T>(fd: &OwnedFd, req: u64, arg: &mut T) -> std::io::Result<()> {
        loop {
            // SAFETY: `arg` is the kernel struct matching `req` (sizes are
            // asserted in `sys`); any pointers inside it outlive the call.
            let r = unsafe {
                libc::ioctl(
                    fd.as_raw_fd(),
                    req as libc::Ioctl,
                    arg as *mut T as *mut c_void,
                )
            };
            if r >= 0 {
                return Ok(());
            }
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(e);
            }
        }
    }

    fn err(what: &str, e: std::io::Error) -> Error {
        Error::Unsupported(format!("V4L2 {what}: {e}"))
    }

    fn fourcc_str(f: u32) -> String {
        f.to_le_bytes().iter().map(|&b| b as char).collect()
    }

    /// An mmapped V4L2 buffer plane.
    struct Mapping {
        ptr: *mut u8,
        len: usize,
    }

    impl Mapping {
        fn new(fd: &OwnedFd, len: usize, offset: u32, write: bool) -> std::io::Result<Mapping> {
            let prot = if write {
                libc::PROT_READ | libc::PROT_WRITE
            } else {
                libc::PROT_READ
            };
            // SAFETY: maps a buffer the driver exported at `offset`.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    prot,
                    libc::MAP_SHARED,
                    fd.as_raw_fd(),
                    offset as libc::off_t,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Mapping {
                ptr: ptr as *mut u8,
                len,
            })
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: unmaps our own mapping.
            unsafe { libc::munmap(self.ptr as *mut c_void, self.len) };
        }
    }

    /// Where a picture sits in a CAPTURE buffer.
    #[derive(Clone, Copy, Debug)]
    enum Layout {
        Nv12(Nv12Layout),
        Q10c(Q10cLayout),
    }

    impl Layout {
        /// Bytes from the start of the buffer to the end of the picture.
        fn end(&self) -> usize {
            match self {
                Layout::Nv12(l) => l.end(),
                Layout::Q10c(l) => l.size,
            }
        }

        fn visible(&self) -> (u32, u32) {
            match self {
                Layout::Nv12(l) => (l.width, l.height),
                Layout::Q10c(l) => (l.width, l.height),
            }
        }

        fn stride(&self) -> usize {
            match self {
                Layout::Nv12(l) => l.stride,
                Layout::Q10c(l) => l.stride,
            }
        }
    }

    struct Capture {
        bufs: Vec<Mapping>,
        /// Coded size (CAPTURE format) the buffers were allocated for.
        allocated_for: (u32, u32),
        layout: Layout,
        /// Buffer we hold after a LAST flag until decoding resumes.
        held: Option<u32>,
    }

    /// One dequeued CAPTURE buffer.
    struct Dequeued {
        index: u32,
        id: u64,
        bytesused: usize,
        flags: u32,
    }

    pub(crate) struct V4l2Decoder {
        // Field order matters for drop: mappings before the fd.
        out_bufs: Vec<Mapping>,
        cap: Option<Capture>,
        fd: OwnedFd,
        pub device: String,
        codec: Codec,
        stream_size: (u32, u32),
        /// The CAPTURE format: NV12, or Q10C for 10-bit streams.
        pixelformat: u32,
        /// The primer trick is active (declared OUTPUT size was lowered).
        primed: bool,
        bsf: *mut ff::AVBSFContext,
        /// The parameter sets the filter inserts before IRAP pictures when it
        /// converts an hvcC (Annex B), else empty.
        hvcc_parameter_sets: Vec<u8>,
        /// IRAP pictures with parameter sets unlike the hvcC's were seen
        /// (logged).
        in_band_parameter_sets: bool,
        /// CAPTURE buffers flagged as broken since the last good picture.
        broken_in_a_row: u32,
        out_free: Vec<u32>,
        /// Filtered packets waiting for a free OUTPUT buffer, with their ids.
        pending: VecDeque<(Packet, u64)>,
        ts: Timestamps,
        /// After a seek, frames before the first new packet's pts are
        /// leading pictures of the new random access point: dropped.
        pts_floor: Option<i64>,
        arm_floor: bool,
        /// A resolution change was signalled; waiting for the LAST buffer.
        drc_pending: bool,
        /// End of stream: bitstream filter drained / STOP sent / LAST seen.
        eos_input: bool,
        stop_sent: bool,
        finished: bool,
        /// A LAST buffer was dequeued; handled on the next receive.
        last_seen: bool,
        pool: *mut ff::AVBufferPool,
        pool_size: usize,
        color: (
            ff::AVColorRange,
            ff::AVColorSpace,
            ff::AVColorTransferCharacteristic,
            ff::AVColorPrimaries,
        ),
        pub frames: u64,
        /// Set when the player closes: waits for the device stop early.
        interrupt: Option<Arc<AtomicBool>>,
    }

    // SAFETY: one thread drives a decoder at a time; the raw pointers are
    // owned FFmpeg objects.
    unsafe impl Send for V4l2Decoder {}

    impl Drop for V4l2Decoder {
        fn drop(&mut self) {
            crate::decode::HW_SESSIONS.fetch_sub(1, Ordering::Relaxed);
            for t in [BUF_TYPE_OUTPUT_MPLANE, BUF_TYPE_CAPTURE_MPLANE] {
                let mut t = t as i32;
                let _ = ioctl(&self.fd, VIDIOC_STREAMOFF, &mut t);
            }
            // SAFETY: frees our own filter and pool (the pool lives on until
            // frames still holding its buffers are freed).
            unsafe {
                ff::av_bsf_free(&mut self.bsf);
                ff::av_buffer_pool_uninit(&mut self.pool);
            }
        }
    }

    fn query_caps(fd: &OwnedFd) -> Option<u32> {
        let mut c = Capability::default();
        ioctl(fd, VIDIOC_QUERYCAP, &mut c).ok()?;
        Some(if c.capabilities & CAP_DEVICE_CAPS != 0 {
            c.device_caps
        } else {
            c.capabilities
        })
    }

    fn formats(fd: &OwnedFd, type_: u32) -> Vec<u32> {
        let mut out = Vec::new();
        for index in 0..64 {
            let mut d = FmtDesc {
                index,
                type_,
                ..Default::default()
            };
            if ioctl(fd, VIDIOC_ENUM_FMT, &mut d).is_err() {
                break;
            }
            out.push(d.pixelformat);
        }
        out
    }

    fn open_node(path: &str) -> std::io::Result<OwnedFd> {
        let c = std::ffi::CString::new(path).unwrap_or_default();
        // SAFETY: plain open(2) of a device node.
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: we own the new descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Whether `fd` is a multi-planar M2M decoder for `codec` with NV12 out.
    fn suitable(fd: &OwnedFd, codec: Codec) -> bool {
        let Some(caps) = query_caps(fd) else {
            return false;
        };
        caps & CAP_VIDEO_M2M_MPLANE != 0
            && caps & CAP_STREAMING != 0
            && formats(fd, BUF_TYPE_OUTPUT_MPLANE).contains(&codec.fourcc())
            && formats(fd, BUF_TYPE_CAPTURE_MPLANE).contains(&NV12)
    }

    /// `/dev/video-dec0` (the Frame's decoder), else the first `/dev/video*`
    /// M2M device decoding `codec`. `FRAMEPLAYERX_V4L2_DEVICE` overrides.
    fn find_device(codec: Codec) -> Result<(String, OwnedFd)> {
        let mut paths: Vec<String> = Vec::new();
        if let Ok(p) = std::env::var("FRAMEPLAYERX_V4L2_DEVICE") {
            paths.push(p);
        } else {
            paths.push("/dev/video-dec0".into());
            let mut nodes: Vec<(u32, String)> = std::fs::read_dir("/dev")
                .map(|d| {
                    d.flatten()
                        .filter_map(|e| {
                            let n = e.file_name().to_string_lossy().into_owned();
                            let num = n.strip_prefix("video")?.parse().ok()?;
                            Some((num, format!("/dev/{n}")))
                        })
                        .collect()
                })
                .unwrap_or_default();
            nodes.sort();
            paths.extend(nodes.into_iter().map(|(_, p)| p));
        }
        for p in paths {
            if let Ok(fd) = open_node(&p)
                && suitable(&fd, codec)
            {
                return Ok((p, fd));
            }
        }
        Err(Error::Unsupported(format!(
            "no V4L2 decoder for {codec:?} with NV12 output"
        )))
    }

    /// The other programs with the driver's `decoder` or `encoder` open, from
    /// `/proc`. Another user's processes can't be read and are left out.
    pub(super) fn session_holders(decoder: &Path, encoder: &Path) -> Vec<Holder> {
        let me = std::process::id().to_string();
        let mut holders: Vec<Holder> = Vec::new();
        let Ok(procs) = std::fs::read_dir("/proc") else {
            return holders;
        };
        for process in procs.flatten() {
            let pid = process.file_name();
            let pid = pid.to_string_lossy();
            if pid == me || !pid.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let Ok(fds) = std::fs::read_dir(process.path().join("fd")) else {
                continue;
            };
            let (mut on_decoder, mut on_encoder) = (0, 0);
            for fd in fds.flatten() {
                match std::fs::read_link(fd.path()) {
                    Ok(target) if target == decoder => on_decoder += 1,
                    Ok(target) if target == encoder => on_encoder += 1,
                    _ => {}
                }
            }
            if on_decoder + on_encoder == 0 {
                continue;
            }
            let name = program_name(&process.path());
            match holders.iter_mut().find(|h| h.name == name) {
                Some(h) => {
                    h.decoder += on_decoder;
                    h.encoder += on_encoder;
                }
                None => holders.push(Holder {
                    name,
                    decoder: on_decoder,
                    encoder: on_encoder,
                }),
            }
        }
        holders.sort_by(|a, b| a.name.cmp(&b.name));
        holders
    }

    /// The executable's file name, else the process's own name for itself.
    fn program_name(process: &Path) -> String {
        std::fs::read_link(process.join("exe"))
            .ok()
            .and_then(|exe| {
                let name = exe.file_name()?.to_string_lossy();
                Some(name.trim_end_matches(" (deleted)").to_string())
            })
            .or_else(|| std::fs::read_to_string(process.join("comm")).ok())
            .map(|name| name.trim().to_string())
            .unwrap_or_default()
    }

    impl V4l2Decoder {
        /// Opens the device and configures the bitstream queue. CAPTURE is
        /// set up later, when the decoder reports the stream's format.
        pub(crate) fn open(
            par: *const ff::AVCodecParameters,
            time_base: ff::AVRational,
        ) -> Result<V4l2Decoder> {
            // SAFETY: par belongs to an open stream.
            let p = unsafe { &*par };
            let codec = Codec::from_ffmpeg(p.codec_id)
                .ok_or_else(|| Error::Unsupported("codec not handled by V4L2".into()))?;
            let eight_bit_420 = [
                ff::AV_PIX_FMT_NONE,
                ff::AV_PIX_FMT_YUV420P,
                ff::AV_PIX_FMT_YUVJ420P,
            ];
            let pixelformat = if eight_bit_420.contains(&p.format) {
                NV12
            } else if p.format == ff::AV_PIX_FMT_YUV420P10LE && codec == Codec::Hevc {
                Q10C
            } else {
                return Err(Error::Unsupported(format!(
                    "V4L2 path takes 8-bit 4:2:0, or 10-bit 4:2:0 HEVC (pixel format {})",
                    p.format
                )));
            };
            if p.width <= 0 || p.height <= 0 {
                return Err(Error::Unsupported("V4L2: stream size unknown".into()));
            }
            // The queues are set up for whole frames (FIELD_NONE); fields
            // from an interlaced H.264 stream (1080i Blu-ray) are not
            // something this path was built or tested for. Software decodes
            // 1080 H.264 easily.
            if codec == Codec::H264 && crate::info::interlaced(p) {
                return Err(Error::Unsupported(
                    "V4L2 path takes progressive H.264 only (this is interlaced)".into(),
                ));
            }
            let stream_size = (p.width as u32, p.height as u32);
            let (device, fd) = find_device(codec)?;
            let mut dec = V4l2Decoder {
                out_bufs: Vec::new(),
                cap: None,
                fd,
                device,
                codec,
                stream_size,
                pixelformat,
                primed: false,
                bsf: std::ptr::null_mut(),
                hvcc_parameter_sets: Vec::new(),
                in_band_parameter_sets: false,
                broken_in_a_row: 0,
                out_free: Vec::new(),
                pending: VecDeque::new(),
                ts: Timestamps::default(),
                pts_floor: None,
                arm_floor: false,
                drc_pending: false,
                eos_input: false,
                stop_sent: false,
                finished: false,
                last_seen: false,
                pool: std::ptr::null_mut(),
                pool_size: 0,
                color: (p.color_range, p.color_space, p.color_trc, p.color_primaries),
                frames: 0,
                interrupt: None,
            };
            crate::decode::HW_SESSIONS.fetch_add(1, Ordering::Relaxed); // until Drop
            dec.init_bsf(par, time_base)?;
            for t in [EVENT_SOURCE_CHANGE, EVENT_EOS] {
                let mut s = EventSubscription {
                    type_: t,
                    ..Default::default()
                };
                ioctl(&dec.fd, VIDIOC_SUBSCRIBE_EVENT, &mut s)
                    .map_err(|e| err("subscribe event", e))?;
            }
            dec.setup_output()?;
            let mut t = BUF_TYPE_OUTPUT_MPLANE as i32;
            ioctl(&dec.fd, VIDIOC_STREAMON, &mut t).map_err(|e| err("STREAMON output", e))?;
            if dec.primed
                && let Some((primer, _)) = codec.primer()
            {
                dec.queue_bitstream(primer, 0)?;
            }
            log::info!(
                "V4L2 decoder {} for {codec:?} {}x{} to {}{}",
                dec.device,
                stream_size.0,
                stream_size.1,
                fourcc_str(pixelformat),
                if dec.primed {
                    " (admission workaround: primer)"
                } else {
                    ""
                }
            );
            Ok(dec)
        }

        fn init_bsf(
            &mut self,
            par: *const ff::AVCodecParameters,
            time_base: ff::AVRational,
        ) -> Result<()> {
            // SAFETY: standard bitstream filter setup on our own context.
            unsafe {
                let f = ff::av_bsf_get_by_name(self.codec.annexb_filter().as_ptr());
                if f.is_null() {
                    return Err(Error::Unsupported(
                        "Annex B bitstream filter missing".into(),
                    ));
                }
                crate::check(ff::av_bsf_alloc(f, &mut self.bsf), "bsf alloc")?;
                crate::check(
                    ff::avcodec_parameters_copy((*self.bsf).par_in, par),
                    "bsf parameters",
                )?;
                (*self.bsf).time_base_in = time_base;
                crate::check(ff::av_bsf_init(self.bsf), "bsf init")?;
            }
            if self.codec != Codec::Hevc {
                return Ok(());
            }
            // SAFETY: par, the filter and its par_out are null or valid; each
            // extradata holds extradata_size bytes.
            unsafe {
                let (Some(p), Some(out)) = (
                    par.as_ref(),
                    self.bsf.as_ref().and_then(|b| b.par_out.as_ref()),
                ) else {
                    return Ok(());
                };
                if p.extradata.is_null() || p.extradata_size < 3 || out.extradata.is_null() {
                    return Ok(());
                }
                let ext = std::slice::from_raw_parts(p.extradata, p.extradata_size as usize);
                // Annex B extradata (MPEG-TS) passes through unchanged; an
                // hvcC comes out as the parameter sets the filter inserts.
                if !ext.starts_with(&[0, 0, 1]) && !ext.starts_with(&[0, 0, 0, 1]) {
                    self.hvcc_parameter_sets = std::slice::from_raw_parts(
                        out.extradata,
                        out.extradata_size.max(0) as usize,
                    )
                    .to_vec();
                }
            }
            Ok(())
        }

        /// The decoder flagged too many pictures in a row as broken: it
        /// can't decode this stream, however it is fed.
        pub(crate) fn pictures_broken(&self) -> bool {
            self.broken_in_a_row >= MAX_BROKEN_PICTURES
        }

        /// End of stream was sent (until a flush).
        pub(crate) fn ended(&self) -> bool {
            self.eos_input
        }

        /// S_FMT + REQBUFS on the OUTPUT queue with the declared size.
        fn try_output(&self, width: u32, height: u32) -> std::io::Result<u32> {
            let mut f = Format::new(BUF_TYPE_OUTPUT_MPLANE);
            f.set_size(width, height);
            f.set_pixelformat(self.codec.fourcc());
            f.set_field(FIELD_NONE);
            f.set_num_planes(1);
            ioctl(&self.fd, VIDIOC_S_FMT, &mut f)?;
            let mut r = RequestBuffers {
                count: OUTPUT_BUFFERS,
                type_: BUF_TYPE_OUTPUT_MPLANE,
                memory: MEMORY_MMAP,
                ..Default::default()
            };
            ioctl(&self.fd, VIDIOC_REQBUFS, &mut r)?;
            Ok(r.count)
        }

        /// [`refused`], with who holds the driver, logging the driver's error.
        fn busy(
            &self,
            admitted: Option<u32>,
            primer: Option<(u32, u32)>,
            e: &std::io::Error,
        ) -> Error {
            let holders = session_holders(
                &std::fs::canonicalize(&self.device).unwrap_or_default(),
                &std::fs::canonicalize("/dev/video-enc0").unwrap_or_default(),
            );
            let refused = refused(
                self.stream_size,
                self.pixelformat == Q10C,
                admitted,
                primer,
                &holders,
            );
            log::info!("V4L2 REQBUFS output: {e}; {refused}");
            refused
        }

        fn free_output(&self) {
            let mut r = RequestBuffers {
                count: 0,
                type_: BUF_TYPE_OUTPUT_MPLANE,
                memory: MEMORY_MMAP,
                ..Default::default()
            };
            let _ = ioctl(&self.fd, VIDIOC_REQBUFS, &mut r);
        }

        fn setup_output(&mut self) -> Result<()> {
            let (w, h) = self.stream_size;
            let count = match self.try_output(w, h) {
                Ok(n) => n,
                Err(e) if e.raw_os_error() == Some(libc::ENOMEM) => {
                    // The admission check: find the largest height admitted
                    // beside the sessions open, then declare that and open
                    // CAPTURE through the primer, if one fits (NV12 only,
                    // see the module docs).
                    let mut fits = |cand: u32| {
                        let ok = self.try_output(w, cand).is_ok();
                        if ok {
                            self.free_output();
                        }
                        ok
                    };
                    let admitted = capped_height(h, MIN_DECLARED_HEIGHT.min(h), &mut fits);
                    let primer =
                        self.codec.primer().map(|(_, size)| size).filter(|size| {
                            self.pixelformat == NV12 && primer_matches((w, h), *size)
                        });
                    // No primer for this stream, or not even it is admitted.
                    let Some(capped) =
                        admitted.filter(|&c| primer.is_some_and(|p| c + HEIGHT_STEP > p.1))
                    else {
                        return Err(self.busy(admitted, primer, &e));
                    };
                    log::info!(
                        "V4L2 {w}x{h} refused by the driver's admission check ({e}); \
                         declaring {w}x{capped} and priming"
                    );
                    self.primed = true;
                    self.try_output(w, capped)
                        .map_err(|e| err("REQBUFS output (lowered)", e))?
                }
                Err(e) => return Err(err("REQBUFS output", e)),
            };
            for i in 0..count {
                let mut planes = [Plane::default(); 1];
                let mut b = Buffer::new(BUF_TYPE_OUTPUT_MPLANE, i, &mut planes);
                ioctl(&self.fd, VIDIOC_QUERYBUF, &mut b).map_err(|e| err("QUERYBUF output", e))?;
                let m = Mapping::new(
                    &self.fd,
                    planes[0].length as usize,
                    planes[0].m as u32,
                    true,
                )
                .map_err(|e| err("mmap output", e))?;
                self.out_bufs.push(m);
                self.out_free.push(i);
            }
            self.out_free.reverse();
            Ok(())
        }

        /// Copies `data` into a free OUTPUT buffer and queues it.
        fn queue_bitstream(&mut self, data: &[u8], id: u64) -> Result<()> {
            let Some(index) = self.out_free.pop() else {
                return Err(Error::Unsupported("V4L2: no free output buffer".into()));
            };
            let m = &self.out_bufs[index as usize];
            if data.len() > m.len {
                return Err(Error::Unsupported(format!(
                    "V4L2: packet of {} bytes exceeds the {} byte bitstream buffer",
                    data.len(),
                    m.len
                )));
            }
            // SAFETY: the buffer is mapped writable for m.len bytes and is
            // not queued (owned by us until QBUF).
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), m.ptr, data.len()) };
            let mut planes = [Plane {
                bytesused: data.len() as u32,
                length: m.len as u32,
                ..Default::default()
            }];
            let mut b = Buffer::new(BUF_TYPE_OUTPUT_MPLANE, index, &mut planes);
            let (sec, usec) = Timestamps::to_timeval(id);
            b.timestamp = [sec, usec];
            if let Err(e) = ioctl(&self.fd, VIDIOC_QBUF, &mut b) {
                self.out_free.push(index);
                return Err(err("QBUF output", e));
            }
            Ok(())
        }

        /// Reclaims consumed OUTPUT buffers and queues pending packets.
        fn pump_output(&mut self) -> Result<()> {
            loop {
                let mut planes = [Plane::default(); 1];
                let mut b = Buffer::new(BUF_TYPE_OUTPUT_MPLANE, 0, &mut planes);
                match ioctl(&self.fd, VIDIOC_DQBUF, &mut b) {
                    Ok(()) => self.out_free.push(b.index),
                    Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => break,
                    Err(e) if e.raw_os_error() == Some(libc::EPIPE) => break,
                    Err(e) => return Err(err("DQBUF output", e)),
                }
            }
            while !self.out_free.is_empty() {
                let Some((pkt, id)) = self.pending.pop_front() else {
                    break;
                };
                // SAFETY: a valid filtered packet.
                let data = unsafe {
                    let p = &*pkt.as_ptr();
                    std::slice::from_raw_parts(p.data, p.size.max(0) as usize)
                };
                self.queue_bitstream(data, id)?;
            }
            if self.eos_input && self.pending.is_empty() && !self.stop_sent {
                let mut c = DecoderCmd {
                    cmd: DEC_CMD_STOP,
                    ..Default::default()
                };
                ioctl(&self.fd, VIDIOC_DECODER_CMD, &mut c).map_err(|e| err("STOP", e))?;
                self.stop_sent = true;
            }
            Ok(())
        }

        /// Moves everything the bitstream filter has ready into `pending`.
        fn drain_bsf(&mut self) -> Result<()> {
            loop {
                let out = Packet::new();
                // SAFETY: our filter; out is a fresh packet.
                let r = unsafe { ff::av_bsf_receive_packet(self.bsf, out.as_ptr()) };
                if r == ff::AVERROR_EAGAIN || r == ff::AVERROR_EOF_ {
                    return Ok(());
                }
                crate::check(r, "bitstream filter")?;
                // SAFETY: a valid packet from the filter.
                let (pts, dts, duration, size) = unsafe {
                    let p = &*out.as_ptr();
                    (p.pts, p.dts, p.duration, p.size)
                };
                if size <= 0 {
                    // V4L2 would read an empty OUTPUT buffer as a full one.
                    continue;
                }
                if !self.hvcc_parameter_sets.is_empty() {
                    let sets = &self.hvcc_parameter_sets;
                    // SAFETY: the filter's packet holds size bytes at data.
                    let moved = unsafe {
                        let p = &*out.as_ptr();
                        hvcc_copy_after_own(std::slice::from_raw_parts(p.data, size as usize), sets)
                    };
                    if let Some(range) = moved {
                        // SAFETY: a writable packet holds size bytes at data.
                        unsafe {
                            crate::check(
                                ff::av_packet_make_writable(out.as_ptr()),
                                "packet writable",
                            )?;
                            let p = &*out.as_ptr();
                            std::slice::from_raw_parts_mut(p.data, size as usize)[range]
                                .rotate_right(sets.len());
                        }
                        if !self.in_band_parameter_sets {
                            self.in_band_parameter_sets = true;
                            log::info!(
                                "V4L2: IRAP pictures carry parameter sets unlike the hvcC's; theirs go last"
                            );
                        }
                    }
                }
                let pts = if pts == ff::AV_NOPTS_VALUE { dts } else { pts };
                if self.arm_floor {
                    self.arm_floor = false;
                    self.pts_floor = (pts != ff::AV_NOPTS_VALUE).then_some(pts);
                }
                let id = self.ts.insert(pts, duration);
                self.pending.push_back((out, id));
            }
        }

        /// Sends a packet, or end of stream when `pkt` is null. Returns false
        /// when the decoder is full: receive frames, then send it again.
        pub(crate) fn send(&mut self, pkt: *const ff::AVPacket) -> Result<bool> {
            self.pump_output()?;
            if !self.pending.is_empty() {
                return Ok(false);
            }
            if pkt.is_null() {
                if !self.eos_input {
                    // SAFETY: a null packet signals EOF to the filter.
                    unsafe { ff::av_bsf_send_packet(self.bsf, std::ptr::null_mut()) };
                    self.drain_bsf()?;
                    self.eos_input = true;
                }
                self.pump_output()?;
                return Ok(true);
            }
            if self.eos_input {
                return Ok(true); // after EOS until a flush
            }
            let copy = Packet::new();
            // SAFETY: references the caller's packet; the filter takes over
            // our reference.
            unsafe {
                crate::check(ff::av_packet_ref(copy.as_ptr(), pkt), "packet ref")?;
                crate::check(ff::av_bsf_send_packet(self.bsf, copy.as_ptr()), "bsf send")?;
            }
            self.drain_bsf()?;
            self.pump_output()?;
            Ok(true)
        }

        fn dequeue_events(&mut self) -> Result<()> {
            loop {
                let mut ev = Event::default();
                match ioctl(&self.fd, VIDIOC_DQEVENT, &mut ev) {
                    Ok(()) => {}
                    Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
                    Err(e) => return Err(err("DQEVENT", e)),
                }
                if ev.type_ == EVENT_SOURCE_CHANGE {
                    if self.cap.is_none() {
                        self.setup_capture()?;
                    } else {
                        log::debug!("V4L2 source change");
                        self.drc_pending = true;
                    }
                }
            }
        }

        fn g_fmt_capture(&self) -> Result<Format> {
            let mut f = Format::new(BUF_TYPE_CAPTURE_MPLANE);
            ioctl(&self.fd, VIDIOC_G_FMT, &mut f).map_err(|e| err("G_FMT capture", e))?;
            Ok(f)
        }

        fn min_capture_buffers(&self) -> u32 {
            let mut c = Control {
                id: CID_MIN_BUFFERS_FOR_CAPTURE,
                value: 0,
            };
            match ioctl(&self.fd, VIDIOC_G_CTRL, &mut c) {
                Ok(()) if c.value > 0 => c.value as u32,
                _ => 1,
            }
        }

        /// The CAPTURE layout for format `f` (visible size from COMPOSE).
        fn layout(&self, f: &Format) -> Result<Layout> {
            if f.pixelformat() != self.pixelformat || f.num_planes() != 1 {
                return Err(Error::Unsupported(format!(
                    "V4L2 capture format {} with {} planes",
                    fourcc_str(f.pixelformat()),
                    f.num_planes()
                )));
            }
            let mut s = Selection {
                type_: BUF_TYPE_CAPTURE,
                target: SEL_TGT_COMPOSE,
                ..Default::default()
            };
            let visible = match ioctl(&self.fd, VIDIOC_G_SELECTION, &mut s) {
                Ok(()) if s.width > 0 && s.height > 0 => (s.width, s.height),
                _ => (f.width(), f.height()),
            };
            let (stride, size) = (f.bytesperline(0) as usize, f.sizeimage(0) as usize);
            let layout = if self.pixelformat == Q10C {
                Q10cLayout::new((f.width(), f.height()), stride, size, visible).map(Layout::Q10c)
            } else {
                Nv12Layout::new(stride, f.height(), visible, size).map(Layout::Nv12)
            };
            layout.ok_or_else(|| {
                Error::Unsupported(format!(
                    "V4L2 capture layout {} {}x{} (visible {}x{}) stride {} size {}",
                    fourcc_str(f.pixelformat()),
                    f.width(),
                    f.height(),
                    visible.0,
                    visible.1,
                    f.bytesperline(0),
                    f.sizeimage(0)
                ))
            })
        }

        /// First source change: choose the format, allocate, map and queue
        /// CAPTURE.
        fn setup_capture(&mut self) -> Result<()> {
            let mut f = self.g_fmt_capture()?;
            if f.pixelformat() != self.pixelformat {
                f.set_pixelformat(self.pixelformat);
                ioctl(&self.fd, VIDIOC_S_FMT, &mut f).map_err(|e| err("S_FMT capture", e))?;
                f = self.g_fmt_capture()?;
            }
            let layout = self.layout(&f)?;
            let min = self.min_capture_buffers();
            let mut r = RequestBuffers {
                count: CAPTURE_BUFFERS.max(min),
                type_: BUF_TYPE_CAPTURE_MPLANE,
                memory: MEMORY_MMAP,
                ..Default::default()
            };
            ioctl(&self.fd, VIDIOC_REQBUFS, &mut r).map_err(|e| err("REQBUFS capture", e))?;
            let mut bufs = Vec::new();
            for i in 0..r.count {
                let mut planes = [Plane::default(); 1];
                let mut b = Buffer::new(BUF_TYPE_CAPTURE_MPLANE, i, &mut planes);
                ioctl(&self.fd, VIDIOC_QUERYBUF, &mut b).map_err(|e| err("QUERYBUF capture", e))?;
                bufs.push(
                    Mapping::new(
                        &self.fd,
                        planes[0].length as usize,
                        planes[0].m as u32,
                        false,
                    )
                    .map_err(|e| err("mmap capture", e))?,
                );
            }
            self.cap = Some(Capture {
                bufs,
                allocated_for: (f.width(), f.height()),
                layout,
                held: None,
            });
            for i in 0..r.count {
                self.queue_capture(i)?;
            }
            let mut t = BUF_TYPE_CAPTURE_MPLANE as i32;
            ioctl(&self.fd, VIDIOC_STREAMON, &mut t).map_err(|e| err("STREAMON capture", e))?;
            let visible = layout.visible();
            log::info!(
                "V4L2 capture {} {}x{} (visible {}x{}, stride {}) x{} buffers of {} bytes",
                fourcc_str(f.pixelformat()),
                f.width(),
                f.height(),
                visible.0,
                visible.1,
                layout.stride(),
                r.count,
                f.sizeimage(0)
            );
            Ok(())
        }

        fn queue_capture(&self, index: u32) -> Result<()> {
            let Some(cap) = &self.cap else {
                return Ok(());
            };
            let mut planes = [Plane {
                length: cap.bufs[index as usize].len as u32,
                ..Default::default()
            }];
            let mut b = Buffer::new(BUF_TYPE_CAPTURE_MPLANE, index, &mut planes);
            ioctl(&self.fd, VIDIOC_QBUF, &mut b).map_err(|e| err("QBUF capture", e))
        }

        fn dequeue_capture(&self) -> Result<Option<Dequeued>> {
            if self.cap.is_none() {
                return Ok(None);
            }
            let mut planes = [Plane::default(); 1];
            let mut b = Buffer::new(BUF_TYPE_CAPTURE_MPLANE, 0, &mut planes);
            match ioctl(&self.fd, VIDIOC_DQBUF, &mut b) {
                Ok(()) => Ok(Some(Dequeued {
                    index: b.index,
                    id: Timestamps::from_timeval(b.timestamp[0], b.timestamp[1]),
                    bytesused: planes[0].bytesused as usize,
                    flags: b.flags,
                })),
                Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => Ok(None),
                // After LAST, until decoding resumes.
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => Ok(None),
                Err(e) => Err(err("DQBUF capture", e)),
            }
        }

        /// Resumes decoding after a LAST buffer (resolution change or drain).
        fn resume(&mut self) -> Result<()> {
            let mut c = DecoderCmd {
                cmd: DEC_CMD_START,
                ..Default::default()
            };
            ioctl(&self.fd, VIDIOC_DECODER_CMD, &mut c).map_err(|e| err("START", e))?;
            if let Some(i) = self.cap.as_mut().and_then(|c| c.held.take()) {
                self.queue_capture(i)?;
            }
            Ok(())
        }

        /// The LAST buffer of a resolution change: reuse the buffers when
        /// they fit (no admission check), else reallocate.
        fn change_resolution(&mut self) -> Result<()> {
            self.drc_pending = false;
            let f = self.g_fmt_capture()?;
            let min = self.min_capture_buffers();
            let (allocated_for, count, smallest) = self
                .cap
                .as_ref()
                .map(|c| {
                    (
                        c.allocated_for,
                        c.bufs.len() as u32,
                        c.bufs.iter().map(|m| m.len).min().unwrap_or(0),
                    )
                })
                .unwrap_or(((0, 0), 0, 0));
            let need = f.sizeimage(0) as usize;
            let new_format = (f.width(), f.height());
            if f.pixelformat() == self.pixelformat
                && can_reuse_capture(allocated_for, count, smallest, new_format, min, need)
            {
                let layout = self.layout(&f)?;
                if layout.end() <= smallest {
                    let (w, h) = layout.visible();
                    log::info!("V4L2 stream now {w}x{h}: reusing {count} capture buffers");
                    if let Some(c) = self.cap.as_mut() {
                        c.layout = layout;
                    }
                    return self.resume();
                }
            }
            log::info!(
                "V4L2 stream now {}x{} {}: reallocating capture ({count} x {smallest} bytes, \
                 need {min} x {need})",
                f.width(),
                f.height(),
                fourcc_str(f.pixelformat())
            );
            let mut t = BUF_TYPE_CAPTURE_MPLANE as i32;
            ioctl(&self.fd, VIDIOC_STREAMOFF, &mut t).map_err(|e| err("STREAMOFF capture", e))?;
            self.cap = None; // unmaps
            let mut r = RequestBuffers {
                count: 0,
                type_: BUF_TYPE_CAPTURE_MPLANE,
                memory: MEMORY_MMAP,
                ..Default::default()
            };
            ioctl(&self.fd, VIDIOC_REQBUFS, &mut r).map_err(|e| err("REQBUFS capture 0", e))?;
            self.primed = false;
            self.setup_capture()
        }

        /// Copies a decoded picture into a pooled `AVFrame`: NV12 with its
        /// two planes stacked, or the whole Q10C buffer.
        fn export(
            &mut self,
            index: u32,
            frame: *mut ff::AVFrame,
            pts: i64,
            duration: i64,
        ) -> Result<()> {
            let Some(cap) = &self.cap else {
                return Err(Error::Unsupported("V4L2: no capture buffers".into()));
            };
            let layout = cap.layout;
            let src = &cap.bufs[index as usize];
            if layout.end() > src.len {
                return Err(Error::Unsupported(
                    "V4L2: picture exceeds its buffer".into(),
                ));
            }
            let size = match layout {
                Layout::Nv12(l) => l.copy_size(),
                Layout::Q10c(l) => l.size,
            };
            // SAFETY: pool buffers hold `size` bytes; the source mapping holds
            // layout.end() bytes; frame is a valid AVFrame we refill.
            unsafe {
                if self.pool.is_null() || self.pool_size != size {
                    ff::av_buffer_pool_uninit(&mut self.pool);
                    self.pool = ff::av_buffer_pool_init(size + 64, None);
                    self.pool_size = size;
                }
                let buf = ff::av_buffer_pool_get(self.pool);
                if buf.is_null() {
                    return Err(Error::Unsupported("V4L2: out of memory for a frame".into()));
                }
                let dst = (*buf).data;
                ff::av_frame_unref(frame);
                let f = &mut *frame;
                match layout {
                    Layout::Nv12(l) => {
                        let y_len = l.stride * l.height as usize;
                        let uv_len = l.stride * l.height.div_ceil(2) as usize;
                        std::ptr::copy_nonoverlapping(src.ptr, dst, y_len);
                        std::ptr::copy_nonoverlapping(
                            src.ptr.add(l.uv_offset),
                            dst.add(y_len),
                            uv_len,
                        );
                        f.buf[0] = buf;
                        f.data[0] = dst;
                        f.data[1] = dst.add(y_len);
                        f.linesize[0] = l.stride as i32;
                        f.linesize[1] = l.stride as i32;
                        f.format = ff::AV_PIX_FMT_NV12;
                        f.width = l.width as i32;
                        f.height = l.height as i32;
                    }
                    Layout::Q10c(l) => {
                        std::ptr::copy_nonoverlapping(src.ptr, dst, l.size);
                        // The pool's buffers are 64 bytes longer.
                        (*buf).size = l.size;
                        crate::frame::set_q10c(
                            f,
                            buf,
                            l.stride as u32,
                            l.rows,
                            l.uv_offset,
                            (l.width, l.height),
                        );
                    }
                }
                f.pts = pts;
                f.best_effort_timestamp = pts;
                f.duration = duration;
                (f.color_range, f.colorspace, f.color_trc, f.color_primaries) = self.color;
            }
            Ok(())
        }

        /// Receives a frame. `Ok(None)`: send more input. `Ok(Some(false))`:
        /// fully drained after end of stream. Pictures `keep` refuses (by
        /// pts and duration) go back to the decoder without being copied.
        pub(crate) fn receive(
            &mut self,
            frame: *mut ff::AVFrame,
            keep: impl Fn(i64, i64) -> bool,
        ) -> Result<Option<bool>> {
            let mut idle_since: Option<Instant> = None;
            loop {
                if self.finished {
                    return Ok(Some(false));
                }
                if self.last_seen {
                    // The LAST buffer (its picture, if any, went out already).
                    self.last_seen = false;
                    // Pick up a source change signalled with it.
                    self.dequeue_events()?;
                    if self.stop_sent && !self.drc_pending {
                        self.finished = true;
                        return Ok(Some(false));
                    }
                    self.change_resolution()?;
                    continue;
                }
                self.pump_output()?;
                self.dequeue_events()?;
                if let Some(d) = self.dequeue_capture()? {
                    idle_since = None;
                    let last = d.flags & BUF_FLAG_LAST != 0;
                    // Broken pictures keep coming, so the stall check never
                    // fires: count them, but not the primer's or those from
                    // before a seek.
                    if self.ts.get(d.id).is_some() {
                        if d.flags & BUF_FLAG_ERROR != 0 {
                            self.broken_in_a_row += 1;
                        } else if d.bytesused > 0 {
                            self.broken_in_a_row = 0;
                        }
                    }
                    let timing = self.ts.get(d.id).filter(|&(pts, duration)| {
                        d.bytesused > 0
                            && d.flags & BUF_FLAG_ERROR == 0
                            && self
                                .pts_floor
                                .is_none_or(|floor| pts == ff::AV_NOPTS_VALUE || pts >= floor)
                            && keep(pts, duration)
                    });
                    let result = timing.map(|(pts, dur)| self.export(d.index, frame, pts, dur));
                    if last {
                        // Held until decoding resumes.
                        if let Some(c) = self.cap.as_mut() {
                            c.held = Some(d.index);
                        }
                        self.last_seen = true;
                    } else {
                        self.queue_capture(d.index)?;
                    }
                    if self.pictures_broken() {
                        return Err(Error::Unsupported(format!(
                            "V4L2 decoder flagged {MAX_BROKEN_PICTURES} pictures in a row as broken"
                        )));
                    }
                    match result {
                        Some(r) => {
                            r?;
                            self.frames += 1;
                            return Ok(Some(true));
                        }
                        None => continue,
                    }
                }
                // Nothing decoded yet. Return for more input unless the
                // bitstream queue is full or we are draining.
                let waiting = !self.pending.is_empty() || self.eos_input || self.drc_pending;
                if !waiting {
                    return Ok(None);
                }
                if self.interrupted() {
                    return Ok(None);
                }
                let since = *idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() > STALL {
                    if self.eos_input {
                        log::warn!("V4L2: no end-of-stream marker from the decoder");
                        self.finished = true;
                        return Ok(Some(false));
                    }
                    return Err(Error::Unsupported("V4L2 decoder stalled".into()));
                }
                self.wait(50);
            }
        }

        pub(crate) fn set_interrupt(&mut self, flag: Arc<AtomicBool>) {
            self.interrupt = Some(flag);
        }

        fn interrupted(&self) -> bool {
            self.interrupt
                .as_ref()
                .is_some_and(|f| f.load(Ordering::Acquire))
        }

        fn wait(&self, ms: i32) {
            let mut p = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN | libc::POLLOUT | libc::POLLPRI,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let r = unsafe { libc::poll(&mut p, 1, ms) };
            if r > 0 && p.revents & libc::POLLERR != 0 {
                // Nothing queued on either side yet: avoid a hot loop.
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        /// Discards everything before a seek. Queues keep streaming (a
        /// restart would re-run the driver's admission check): bitstream
        /// already queued still decodes, and its frames are dropped.
        pub(crate) fn flush(&mut self) {
            self.pending.clear();
            // SAFETY: our filter.
            unsafe { ff::av_bsf_flush(self.bsf) };
            self.ts.invalidate();
            self.pts_floor = None;
            self.arm_floor = true;
            self.broken_in_a_row = 0;
            if !self.stop_sent || self.drc_pending {
                // Mid-stream (a pending resolution change is handled by the
                // next receive): nothing to restart.
                self.eos_input = false;
                return;
            }
            if self.last_seen {
                self.last_seen = false;
                self.finished = true;
            }
            if !self.finished {
                // Let the drain finish so decoding can be restarted.
                let start = Instant::now();
                while !self.finished
                    && start.elapsed() < Duration::from_secs(2)
                    && !self.interrupted()
                {
                    match self.dequeue_capture() {
                        Ok(Some(d)) if d.flags & BUF_FLAG_LAST != 0 => {
                            if let Some(c) = self.cap.as_mut() {
                                c.held = Some(d.index);
                            }
                            self.finished = true;
                        }
                        Ok(Some(d)) => {
                            let _ = self.queue_capture(d.index);
                        }
                        Ok(None) => self.wait(20),
                        Err(_) => break,
                    }
                }
            }
            if let Err(e) = self.resume() {
                log::warn!("V4L2 restart after end of stream: {e}");
            }
            self.eos_input = false;
            self.stop_sent = false;
            self.finished = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The iris driver's check: every open instance counts with the new
    /// session's macroblocks, and one session has a limit of its own.
    fn iris_admits(instances: u64, width: u32) -> impl Fn(u32) -> bool {
        move |h| {
            let mb = macroblocks(width, h);
            mb <= MAX_SESSION_MACROBLOCKS && instances * mb <= MAX_CORE_MACROBLOCKS
        }
    }

    fn holder(name: &str, decoder: u64, encoder: u64) -> Holder {
        Holder {
            name: name.into(),
            decoder,
            encoder,
        }
    }

    /// `refused`'s text and who it found, panicking if it says too big.
    fn busy(e: crate::Error) -> (String, crate::SessionHolders) {
        let text = e.to_string();
        match e {
            crate::Error::HardwareBusy(_, holders) => (text, holders),
            e => panic!("not busy: {e}"),
        }
    }

    #[test]
    fn the_model_matches_what_the_frame_admits() {
        // Tallest admitted by other sessions open (0 to 3), measured with
        // S_FMT and REQBUFS on OUTPUT.
        let measured: [(u32, [u32; 4]); 5] = [
            (8192, [4320, 4064, 2720, 2016]),
            (7680, [4608, 4352, 2880, 2176]),
            (7200, [4896, 4640, 3072, 2304]),
            (6144, [5760, 5440, 3616, 2720]),
            (5760, [6144, 5792, 3840, 2880]),
        ];
        for (width, tallest) in measured {
            for (others, want) in tallest.into_iter().enumerate() {
                let fits = iris_admits(others as u64 + 1, width);
                assert_eq!(
                    capped_height(8192, 512, fits),
                    Some(want),
                    "{width} wide beside {others}"
                );
            }
        }
    }

    #[test]
    fn caps_height_like_the_frame_needs() {
        // 8192x4096 with Steam's vrlink open: 2 x 131072 > 261120.
        assert!(!iris_admits(2, 8192)(4096));
        let mut calls = 0;
        let fits = iris_admits(2, 8192);
        let h = capped_height(4096, 512, |c| {
            calls += 1;
            fits(c)
        });
        // 4080 would fit the macroblock budget but is not 32-aligned (the
        // driver rounds it up to 4096); 4064 is the answer measured on the
        // headset.
        assert_eq!(h, Some(4064));
        assert!(calls <= 8, "{calls} probes");
        // Three instances.
        assert_eq!(capped_height(4096, 512, iris_admits(3, 8192)), Some(2720));
        // Nothing small enough.
        assert_eq!(capped_height(4096, 512, |_| false), None);
        // Everything fits: the largest aligned height strictly below.
        assert_eq!(capped_height(4096, 512, |_| true), Some(4064));
        assert_eq!(capped_height(4100, 512, |_| true), Some(4096));
        // Floor above the stream: nothing to try.
        assert_eq!(capped_height(300, 512, |_| true), None);
        assert_eq!(capped_height(1080, 1080, |_| true), None);
    }

    #[test]
    fn a_refused_stream_says_how_much_room_was_left() {
        // 10-bit 8K gets no primer: beside one other session it is refused.
        let admitted = capped_height(4096, 512, iris_admits(2, 8192));
        assert_eq!(
            refused((8192, 4096), true, admitted, None, &[]).to_string(),
            "hardware decoder busy: other video sessions leave room for 8192x4064, \
             not 8192x4096 10-bit"
        );
        assert!(iris_admits(2, 7680)(4320) && iris_admits(2, 5760)(5760));
        assert_eq!(
            refused((8192, 4096), false, None, None, &[]).to_string(),
            "hardware decoder busy: no room for another video session"
        );
    }

    #[test]
    fn a_refused_stream_names_who_holds_the_driver() {
        let holders = [holder("steam", 0, 1), holder("steamwebhelper", 2, 0)];
        let admitted = capped_height(4096, 512, iris_admits(4, 8192));
        let (text, found) = busy(refused((8192, 4096), true, admitted, None, &holders));
        assert_eq!(
            text,
            "hardware decoder busy: held by steam, steamwebhelper x2, leaving room for \
             8192x2016, not 8192x4096 10-bit"
        );
        assert_eq!(found.programs, ["steam", "steamwebhelper"]);
        let (text, _) = busy(refused((8192, 4096), true, None, None, &holders));
        assert_eq!(
            text,
            "hardware decoder busy: held by steam, steamwebhelper x2, leaving no room for \
             another video session"
        );
    }

    #[test]
    fn too_big_is_not_busy_whoever_is_open() {
        // Measured on the Frame with nothing else open: 8192x4320 is the
        // largest 8192-wide stream it takes, and 6K top-bottom is refused.
        assert!(iris_admits(1, 8192)(4320) && !iris_admits(1, 8192)(4352));
        assert!(!iris_admits(1, 6144)(6144) && iris_admits(1, 5760)(5760));
        assert!(!primer_matches((6144, 6144), (8192, 4080)));
        let holders = [holder("steam", 0, 1)];
        for (stream, text) in [
            (
                (6144, 6144),
                "too big for the hardware decoder: 6144x6144, even with nothing else open",
            ),
            (
                (8192, 8192),
                "too big for the hardware decoder: 8192x8192, even with nothing else open",
            ),
            (
                (8192, 4352),
                "too big for the hardware decoder: 8192x4352, even with nothing else open",
            ),
        ] {
            let e = refused(stream, false, Some(2016), None, &holders);
            assert!(matches!(e, crate::Error::HardwareTooBig(_)), "{e}");
            assert_eq!(e.to_string(), text);
        }
        assert!(matches!(
            refused((8192, 4320), false, Some(4064), None, &holders),
            crate::Error::HardwareBusy(..)
        ));
    }

    #[test]
    fn recording_only_blocks_what_would_fit_without_it() {
        let blocks = |stream, ten_bit, primer, holders: &[Holder]| {
            busy(refused(stream, ten_bit, None, primer, holders))
                .1
                .recording_blocks
        };
        let recording = holder("steam", 0, 1);
        // Measured on the Frame: these went to hardware when it was turned off.
        assert!(blocks(
            (8192, 4096),
            true,
            None,
            std::slice::from_ref(&recording)
        ));
        assert!(blocks(
            (8192, 4320),
            false,
            None,
            std::slice::from_ref(&recording)
        ));
        // Still refused beside two more sessions once it stops.
        let browser = holder("steamwebhelper", 2, 0);
        assert!(!blocks(
            (8192, 4096),
            true,
            None,
            &[recording.clone(), browser.clone()]
        ));
        // The primer fits beside one more; beside two it doesn't.
        let primer = Some((8192, 4080));
        let one = holder("steamwebhelper", 1, 0);
        assert!(blocks(
            (8192, 4096),
            false,
            primer,
            &[recording.clone(), one.clone()]
        ));
        assert!(!blocks(
            (8192, 4096),
            false,
            primer,
            &[recording.clone(), browser.clone()]
        ));
        // Not Steam's recording: others on the encoder, Steam on the decoder.
        assert!(!blocks((8192, 4096), true, None, &[holder("obs", 0, 1)]));
        assert!(!blocks((8192, 4096), true, None, &[holder("steam", 1, 0)]));
    }

    #[test]
    fn fits_beside_follows_the_driver_and_the_primer() {
        let primer = Some((8192, 4080));
        assert!(fits_beside((8192, 4096), None, 0));
        assert!(!fits_beside((8192, 4096), None, 1));
        assert!(fits_beside((8192, 4096), primer, 1));
        assert!(!fits_beside((8192, 4096), primer, 2));
        assert!(fits_beside((7680, 4320), None, 1));
        assert!(!fits_beside((7680, 4320), None, 2));
        assert!(fits_beside((8192, 4320), None, 0));
        assert!(!fits_beside((8192, 4352), None, 0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finds_who_holds_the_devices_by_their_open_files() {
        let dir = std::env::temp_dir().join(format!("fpx-holders-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (decoder, encoder) = (dir.join("video22"), dir.join("video23"));
        std::fs::write(&decoder, b"").unwrap();
        std::fs::write(&encoder, b"").unwrap();
        let (decoder, encoder) = (
            std::fs::canonicalize(decoder).unwrap(),
            std::fs::canonicalize(encoder).unwrap(),
        );
        // This process's own files don't count; a child holds the stand-ins.
        let _own = std::fs::File::open(&decoder).unwrap();
        let script = format!(
            "exec 3<{0} 4<{0} 5<{1}; echo ready; exec sleep 30",
            decoder.display(),
            encoder.display()
        );
        let mut child = std::process::Command::new("sh")
            .args(["-c", &script])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut ready,
        )
        .unwrap();
        let found = linux::session_holders(&decoder, &encoder);
        child.kill().unwrap();
        child.wait().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].decoder, found[0].encoder), (2, 1));
        assert!(!found[0].name.is_empty());
    }

    #[test]
    fn capped_height_results_are_aligned_and_admitted() {
        for inst in 2..6 {
            for &(w, h) in &[(8192u32, 4096u32), (7680, 3840), (5760, 2880), (8192, 8192)] {
                let fits = iris_admits(inst, w);
                if let Some(c) = capped_height(h, 512, &fits) {
                    assert_eq!(c % HEIGHT_STEP, 0);
                    assert!(c < h && fits(c));
                    assert!(c + HEIGHT_STEP >= h || !fits(c + HEIGHT_STEP));
                }
            }
        }
    }

    #[test]
    fn reuse_decision() {
        let nv12_8k = 8192 * 4096 * 3 / 2;
        let k8 = (8192, 4096);
        // Measured: primer 8192x4080 and stream 8192x4096 both report an
        // 8192x4096 CAPTURE format; 8 buffers of 50331648 bytes, minimum 7.
        assert!(can_reuse_capture(k8, 8, nv12_8k, k8, 7, nv12_8k));
        // A different coded size never reuses, even when the buffers are big
        // enough: the firmware's internal buffers would not match (it faults).
        assert!(!can_reuse_capture(
            k8,
            8,
            nv12_8k,
            (8192, 2304),
            7,
            8192 * 2304 * 3 / 2
        ));
        assert!(!can_reuse_capture((256, 128), 8, nv12_8k, k8, 7, nv12_8k));
        // Too small or too few: reallocate.
        assert!(!can_reuse_capture(k8, 8, 256 * 128 * 3 / 2, k8, 7, nv12_8k));
        assert!(!can_reuse_capture(k8, 6, nv12_8k, k8, 7, nv12_8k));
        assert!(!can_reuse_capture(k8, 0, nv12_8k, k8, 1, nv12_8k));
        assert!(!can_reuse_capture(k8, 8, nv12_8k, k8, 7, 0));
    }

    #[test]
    fn primer_only_for_streams_with_its_buffers() {
        let primer = Codec::Hevc.primer().unwrap().1;
        assert_eq!(primer, (8192, 4080));
        assert!(primer_matches((8192, 4096), primer));
        assert!(primer_matches((8192, 4088), primer));
        // Smaller than the primer: the primer would not help.
        assert!(!primer_matches((8192, 4080), primer));
        assert!(!primer_matches((8192, 4064), primer));
        // Other sizes need other buffers.
        assert!(!primer_matches((8192, 4320), primer));
        assert!(!primer_matches((7680, 4096), primer));
        assert!(!primer_matches((8192, 8192), primer));
        assert!(Codec::H264.primer().is_none());
    }

    #[test]
    fn nv12_layout_from_driver_format() {
        // What iris reports for an 8192x4080 picture.
        let l = Nv12Layout::new(8192, 4096, (8192, 4080), 50_331_648).unwrap();
        assert_eq!(l.uv_offset, 8192 * 4096);
        assert_eq!(l.end(), 8192 * 4096 + 8192 * 2040);
        assert_eq!(l.copy_size(), 8192 * (4080 + 2040));
        // A buffer too small for the picture is rejected.
        assert!(Nv12Layout::new(8192, 4096, (8192, 4096), 1 << 20).is_none());
        // Stride narrower than the picture is rejected.
        assert!(Nv12Layout::new(1024, 1088, (1920, 1080), 1 << 24).is_none());
        // Odd heights round the chroma up.
        let l = Nv12Layout::new(128, 64, (100, 51), 128 * 96).unwrap();
        assert_eq!(l.end(), 128 * 64 + 128 * 26);
    }

    #[test]
    fn q10c_layout_matches_the_driver() {
        // (CAPTURE format, stride, sizeimage) as iris reports them, and
        // where the UV plane starts by the GPU's own layout of the Y plane.
        let measured = [
            ((1920, 1088), 2560, 4_210_688, (1920, 1080), 2_805_760),
            ((3840, 2160), 5120, 16_736_256, (3840, 2160), 11_128_832),
            ((5760, 2880), 7680, 33_320_960, (5760, 2880), 22_212_608),
            ((7680, 3840), 10240, 59_260_928, (7680, 3840), 39_505_920),
            ((8256, 4096), 11008, 67_928_064, (8192, 4096), 45_285_376),
        ];
        for (fmt, stride, size, visible, uv_offset) in measured {
            let l = Q10cLayout::new(fmt, stride, size, visible).unwrap();
            assert_eq!((l.uv_offset, l.size, l.rows), (uv_offset, size, fmt.1));
            assert_eq!((l.width, l.height), visible);
        }
        // The 8192x4080 primer gets smaller buffers than an 8192x4096 stream.
        let primer = Q10cLayout::new((8256, 4080), 11008, 67_751_936, (8192, 4080)).unwrap();
        assert!(primer.size < 67_928_064);
        // A stride or size the rules don't give is not a layout we know.
        assert!(Q10cLayout::new((1920, 1088), 2048, 4_210_688, (1920, 1080)).is_none());
        assert!(Q10cLayout::new((1920, 1088), 2560, 4_210_689, (1920, 1080)).is_none());
        assert!(Q10cLayout::new((1920, 1088), 2560, 4_210_688, (1920, 1100)).is_none());
    }

    #[test]
    fn timestamps_round_trip_and_invalidate() {
        let mut t = Timestamps::default();
        let a = t.insert(100, 1);
        let b = t.insert(200, 1);
        assert!(a >= 1 && b > a);
        let (s, u) = Timestamps::to_timeval(b);
        assert_eq!(Timestamps::from_timeval(s, u), b);
        assert_eq!(t.get(a), Some((100, 1)));
        assert_eq!(t.get(0), None, "primer");
        t.invalidate();
        assert_eq!(t.get(b), None, "stale after a seek");
        let c = t.insert(300, 1);
        assert!(c > b);
        assert_eq!(t.get(c), Some((300, 1)));
        let big = 1_234_567_890;
        let (s, u) = Timestamps::to_timeval(big);
        assert_eq!((s, u), (1234, 567_890));
        assert_eq!(Timestamps::from_timeval(s, u), big);
        for i in 0..1000 {
            t.insert(i, 1);
        }
        assert!(t.map.len() <= TIMESTAMPS_KEPT);
    }

    #[test]
    fn primers_match_their_codecs() {
        let (h, _) = Codec::Hevc.primer().unwrap();
        assert!(h.starts_with(&[0, 0, 0, 1, 0x40, 0x01]), "HEVC VPS first");
        // VPS, SPS, PPS, one IDR slice; no SEI.
        let nal_types: Vec<u8> = h
            .windows(4)
            .filter(|w| w[..3] == [0, 0, 1])
            .map(|w| (w[3] >> 1) & 0x3f)
            .collect();
        assert_eq!(nal_types, [32, 33, 34, 20]);
        assert!(h.len() < 16 * 1024);
        assert_eq!(Codec::Hevc.fourcc(), fourcc(b"HEVC"));
        assert_eq!(macroblocks(8192, 4096), 131_072);
        assert_eq!(macroblocks(1920, 1080), 120 * 68);
    }

    /// Annex B with 4-byte start codes, as `hevc_mp4toannexb` writes it.
    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| [&[0, 0, 0, 1][..], n].concat())
            .collect()
    }

    #[test]
    fn in_band_parameter_sets_go_last() {
        // An hvcC that disagrees with the stream (some Matroska re-encodes):
        // same ids, other contents.
        let (vps, sps, pps) = (
            &[0x40, 1, 0xaa][..],
            &[0x42, 1, 0xaa][..],
            &[0x44, 1, 0xaa][..],
        );
        let hvcc = annex_b(&[vps, sps, pps]);
        let (own_vps, own_sps, own_pps) = (
            &[0x40, 1, 0xbb][..],
            &[0x42, 1, 0xbb][..],
            &[0x44, 1, 0xbb][..],
        );
        let (aud, sei, idr, trail) = (
            &[0x46, 1, 0x50][..],
            &[0x4e, 1, 5][..],
            &[0x26, 1, 0xaf][..],
            &[0x02, 1, 0xd0][..],
        );

        // The keyframe as sent: the hvcC's copy moved ahead, when it moves.
        let sent = |nals: &[&[u8]], extradata: &[u8]| {
            let mut data = annex_b(nals);
            let range = hvcc_copy_after_own(&data, extradata)?;
            data[range].rotate_right(extradata.len());
            Some(data)
        };

        // What the filter makes of a keyframe with its own parameter sets.
        assert_eq!(
            sent(&[own_vps, own_sps, own_pps, vps, sps, pps, idr], &hvcc),
            Some(annex_b(&[vps, sps, pps, own_vps, own_sps, own_pps, idr]))
        );
        // An access unit delimiter and SEI stay ahead of them, and only the
        // first picture's start is looked for.
        assert_eq!(
            sent(
                &[aud, sei, own_vps, own_sps, own_pps, vps, sps, pps, idr, idr],
                &hvcc
            ),
            Some(annex_b(&[
                aud, sei, vps, sps, pps, own_vps, own_sps, own_pps, idr, idr
            ]))
        );

        // Unchanged: keyframes without parameter sets of their own (most
        // MP4s) or with the hvcC's, pictures that aren't IRAP, nothing
        // inserted.
        for nals in [
            &[vps, sps, pps, idr][..],
            &[aud, vps, sps, pps, idr],
            &[vps, sps, pps, vps, sps, pps, idr],
            &[own_vps, own_sps, own_pps, trail],
            &[own_vps, own_sps, own_pps, idr],
            // The search ends at the first picture.
            &[aud, trail, own_vps, own_sps, own_pps, vps, sps, pps, idr],
        ] {
            assert_eq!(sent(nals, &hvcc), None, "{nals:x?}");
        }
        assert_eq!(
            sent(&[own_vps, own_sps, own_pps, vps, sps, pps, idr], &[]),
            None
        );
    }
}
