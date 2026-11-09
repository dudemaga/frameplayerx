//! Decoded video frames in layouts the renderer can upload directly.

use crate::info::Transfer;
use crate::{Error, Result};
use fp_ffmpeg_sys as ff;

/// Memory layout of a decoded frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelLayout {
    /// Three planes Y, U, V at 4:2:0. `bits` is 8 (one byte per sample) or 10
    /// (two bytes, value in the low bits).
    I420 { bits: u32 },
    /// Y plane plus interleaved UV plane, 8 bits.
    Nv12,
    /// Y plane plus interleaved UV plane, 16 bits with 10 significant high bits.
    P010,
    /// GPU buffer shared as DMA-BUF file descriptors (zero copy).
    DrmPrime,
    /// The Steam Frame decoder's 10-bit output, Q10C: Qualcomm's compressed
    /// (UBWC) TP10, three 10-bit samples in four bytes, as one buffer of Y
    /// metadata, Y, UV metadata and UV ([`VideoFrame::q10c`]). Only the GPU
    /// can read it.
    Q10c,
}

impl PixelLayout {
    pub fn bytes_per_sample(&self) -> u32 {
        match self {
            PixelLayout::I420 { bits: 8 } | PixelLayout::Nv12 => 1,
            _ => 2,
        }
    }
}

/// YUV to RGB matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorInfo {
    pub matrix: Matrix,
    pub full_range: bool,
    pub transfer: Transfer,
    /// BT.2020 primaries (needs gamut mapping to the display's BT.709).
    pub wide_gamut: bool,
}

/// The buffer of a [`PixelLayout::Q10c`] frame.
pub struct Q10cPicture<'a> {
    /// The whole buffer: Y metadata, Y, UV metadata, UV.
    pub data: &'a [u8],
    /// Bytes per row of both planes.
    pub stride: u32,
    /// Luma rows the buffer is laid out for (the frame height or more).
    pub rows: u32,
    /// Where the UV plane (its metadata first) starts.
    pub uv_offset: usize,
}

/// `AVFrame::format` of [`PixelLayout::Q10c`] frames. FFmpeg has no such
/// format, so this is its fourcc, far outside FFmpeg's own values.
pub(crate) const PIX_FMT_Q10C: ff::AVPixelFormat = 0x4330_3151;

/// `AVFrame::format` of frames left in the decoder's buffer
/// ([`VideoFrame::shared`]), "SHRD", also outside FFmpeg's values.
pub(crate) const PIX_FMT_SHARED: ff::AVPixelFormat = 0x4452_4853;

/// A picture left in the hardware decoder's buffer, for the GPU to read
/// there instead of from a copy. Laid out as [`Q10cPicture`] for
/// [`PixelLayout::Q10c`], as NV12 (Y rows, then interleaved UV rows, at
/// `stride`) for [`PixelLayout::Nv12`]. The buffer goes back to the
/// decoder when the last frame holding it is dropped, so the renderer keeps
/// a frame until the GPU has finished with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedPicture {
    /// The buffer as a DMA-BUF file descriptor, open while the frame lives.
    pub fd: i32,
    /// Bytes in the buffer.
    pub len: usize,
    /// Bytes per row of both planes.
    pub stride: u32,
    /// Luma rows the buffer is laid out for (the frame height or more).
    pub rows: u32,
    /// Where the UV plane starts (for Q10C, its metadata first).
    pub uv_offset: usize,
    /// Names the buffer: every frame from it has the same, so the renderer
    /// imports it once. Unique in the process.
    pub buffer: u64,
    /// The decoder's set of buffers it belongs to. A new set (another
    /// video, or a new size) means the old buffers are done with.
    pub set: u64,
}

/// What a shared frame's `buf[0]` points at: the picture, and what gives
/// the buffer back to the decoder when FFmpeg frees the last reference.
pub(crate) struct Lent {
    pub(crate) picture: SharedPicture,
    pub(crate) layout: PixelLayout,
    pub(crate) release: Option<Box<dyn FnOnce() + Send>>,
}

impl Drop for Lent {
    fn drop(&mut self) {
        if let Some(r) = self.release.take() {
            r();
        }
    }
}

unsafe extern "C" fn free_lent(_opaque: *mut std::ffi::c_void, data: *mut u8) {
    // SAFETY: `data` is the Box `set_shared` leaked into the buffer, freed
    // once, when its last reference goes.
    drop(unsafe { Box::from_raw(data as *mut Lent) });
}

/// Makes `f` a shared frame of the picture in `lent`, `size` its visible
/// size. On failure `lent` is dropped, which gives the buffer back.
///
/// # Safety
/// `f` is an unreferenced frame.
pub(crate) unsafe fn set_shared(
    f: &mut ff::AVFrame,
    lent: Box<Lent>,
    size: (u32, u32),
) -> Result<()> {
    let data = Box::into_raw(lent) as *mut u8;
    // SAFETY: the buffer owns the Box from here; free_lent drops it.
    let buf = unsafe {
        ff::av_buffer_create(
            data,
            size_of::<Lent>(),
            Some(free_lent),
            std::ptr::null_mut(),
            ff::AV_BUFFER_FLAG_READONLY as i32,
        )
    };
    if buf.is_null() {
        // SAFETY: not taken over by a buffer.
        drop(unsafe { Box::from_raw(data as *mut Lent) });
        return Err(Error::Unsupported("out of memory for a frame".into()));
    }
    // No pixels in `data`: only the GPU reads the picture.
    f.buf[0] = buf;
    f.format = PIX_FMT_SHARED;
    f.width = size.0 as i32;
    f.height = size.1 as i32;
    Ok(())
}

/// The [`Lent`] of a shared frame.
fn lent(f: &ff::AVFrame) -> Option<&Lent> {
    if f.format != PIX_FMT_SHARED || f.buf[0].is_null() {
        return None;
    }
    // SAFETY: shared frames are built by set_shared: buf[0] holds a Lent,
    // alive while the frame references it.
    unsafe { ((*f.buf[0]).data as *const Lent).as_ref() }
}

/// One plane of a frame.
pub struct Plane<'a> {
    pub data: &'a [u8],
    /// Bytes per row.
    pub stride: usize,
    /// Size in samples (a UV pair counts once).
    pub width: u32,
    pub height: u32,
    pub bytes_per_sample: u32,
    /// 1 for Y/U/V planes, 2 for interleaved UV.
    pub components: u32,
}

/// A decoded frame. Holds a reference to FFmpeg's buffer, released on drop.
pub struct VideoFrame {
    /// Presentation time in seconds.
    pub pts: f64,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub layout: PixelLayout,
    pub color: ColorInfo,
    /// Seek generation this frame belongs to.
    pub serial: u64,
    /// Unique per decoded frame in this process, for upload caching.
    pub id: u64,
    frame: *mut ff::AVFrame,
}

static NEXT_FRAME_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_FRAME_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

// SAFETY: the AVFrame is immutable after construction and only freed on drop;
// FFmpeg frame buffers are reference counted and thread safe.
unsafe impl Send for VideoFrame {}
// SAFETY: as for Send; shared access only reads the frame.
unsafe impl Sync for VideoFrame {}

impl Drop for VideoFrame {
    fn drop(&mut self) {
        // SAFETY: we own this reference.
        unsafe { ff::av_frame_free(&mut self.frame) };
    }
}

impl std::fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "VideoFrame({:.3}s {}x{} {:?})",
            self.pts, self.width, self.height, self.layout
        )
    }
}

impl VideoFrame {
    pub fn planes(&self) -> Vec<Plane<'_>> {
        let (n, chroma_components) = match self.layout {
            PixelLayout::I420 { .. } => (3, 1),
            PixelLayout::Nv12 | PixelLayout::P010 => (2, 2),
            PixelLayout::DrmPrime | PixelLayout::Q10c => return Vec::new(),
        };
        let bps = self.layout.bytes_per_sample();
        // SAFETY: frame is a valid decoded frame of this layout; plane sizes
        // follow from the 4:2:0 subsampling.
        unsafe {
            if self.frame.is_null() || self.width == 0 || self.height == 0 {
                return Vec::new();
            }
            let f = &*self.frame;
            for i in 0..n {
                if f.data[i].is_null() {
                    return Vec::new();
                }
            }
            (0..n)
                .map(|i| {
                    let (w, h, comps) = if i == 0 {
                        (self.width, self.height, 1)
                    } else {
                        (
                            self.width.div_ceil(2),
                            self.height.div_ceil(2),
                            chroma_components,
                        )
                    };
                    let stride = f.linesize[i].unsigned_abs() as usize;
                    let row_bytes = (w * comps * bps) as usize;
                    let len = if h == 0 {
                        0
                    } else {
                        stride * (h as usize - 1) + row_bytes
                    };
                    Plane {
                        data: if f.data[i].is_null() || len == 0 {
                            &[]
                        } else {
                            std::slice::from_raw_parts(f.data[i], len)
                        },
                        stride,
                        width: w,
                        height: h,
                        bytes_per_sample: bps,
                        components: comps,
                    }
                })
                .collect()
        }
    }

    /// Builds a frame from tightly packed RGBA pixels (photos decoded in Rust,
    /// test images). Converted to full-range BT.709 I420.
    pub fn from_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<VideoFrame> {
        Self::from_rgba_as(width, height, rgba, PixelLayout::I420 { bits: 8 }, true)
    }

    /// Like [`VideoFrame::from_rgba`], converted to BT.709 in `layout` (any
    /// but `DrmPrime`), full or limited range: frames shaped like the hardware
    /// decoders' (NV12, P010) for tests.
    pub fn from_rgba_as(
        width: u32,
        height: u32,
        rgba: &[u8],
        layout: PixelLayout,
        full_range: bool,
    ) -> Result<VideoFrame> {
        if width == 0 || height == 0 || rgba.len() < (width * height * 4) as usize {
            return Err(Error::Unsupported(
                "RGBA buffer does not match its size".into(),
            ));
        }
        let format = match layout {
            PixelLayout::I420 { bits: 8 } => ff::AV_PIX_FMT_YUV420P,
            PixelLayout::I420 { bits: 10 } => ff::AV_PIX_FMT_YUV420P10LE,
            PixelLayout::Nv12 => ff::AV_PIX_FMT_NV12,
            PixelLayout::P010 => ff::AV_PIX_FMT_P010LE,
            _ => {
                return Err(Error::Unsupported(format!(
                    "no RGBA conversion to {layout:?}"
                )));
            }
        };
        let (w, h) = (width as i32, height as i32);
        // SAFETY: a fresh frame is allocated with av_frame_get_buffer and
        // filled by swscale from the caller's buffer, which holds w*h*4 bytes.
        unsafe {
            let ctx = ff::sws_getContext(
                w,
                h,
                ff::AV_PIX_FMT_RGBA,
                w,
                h,
                format,
                (ff::SWS_BICUBIC | ff::SWS_ACCURATE_RND | ff::SWS_FULL_CHR_H_INP) as i32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if ctx.is_null() {
                return Err(Error::Unsupported("swscale RGBA -> YUV".into()));
            }
            let coeffs = ff::sws_getCoefficients(ff::SWS_CS_ITU709 as i32);
            ff::sws_setColorspaceDetails(
                ctx,
                coeffs,
                1,
                coeffs,
                full_range as i32,
                0,
                1 << 16,
                1 << 16,
            );
            let mut dst = ff::av_frame_alloc();
            (*dst).format = format;
            (*dst).width = w;
            (*dst).height = h;
            (*dst).colorspace = ff::AVCOL_SPC_BT709;
            (*dst).color_range = if full_range {
                ff::AVCOL_RANGE_JPEG
            } else {
                ff::AVCOL_RANGE_MPEG
            };
            (*dst).color_trc = ff::AVCOL_TRC_BT709;
            (*dst).color_primaries = ff::AVCOL_PRI_BT709;
            if ff::av_frame_get_buffer(dst, 0) < 0 {
                ff::av_frame_free(&mut dst);
                ff::sws_freeContext(ctx);
                return Err(Error::Unsupported("frame allocation failed".into()));
            }
            let src = [
                rgba.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            ];
            let stride = [w * 4, 0, 0, 0];
            ff::sws_scale(
                ctx,
                src.as_ptr(),
                stride.as_ptr(),
                0,
                h,
                (*dst).data.as_ptr(),
                (*dst).linesize.as_ptr(),
            );
            ff::sws_freeContext(ctx);
            let color = color_info(&*dst);
            Ok(VideoFrame {
                pts: 0.0,
                duration: 0.0,
                width,
                height,
                layout,
                color,
                serial: 0,
                id: next_id(),
                frame: dst,
            })
        }
    }

    /// DMA-BUF description for zero-copy import, when the layout is DrmPrime.
    pub fn drm_descriptor(&self) -> Option<&ff::AVDRMFrameDescriptor> {
        if self.layout != PixelLayout::DrmPrime {
            return None;
        }
        // SAFETY: for DRM_PRIME frames data[0] points at the descriptor.
        unsafe { ((*self.frame).data[0] as *const ff::AVDRMFrameDescriptor).as_ref() }
    }

    /// The picture in the decoder's own buffer, for frames the GPU reads
    /// there ([`SharedPicture`]). Such frames have no [`VideoFrame::planes`]
    /// or [`VideoFrame::q10c`]: the CPU can't read them.
    pub fn shared(&self) -> Option<&SharedPicture> {
        // SAFETY: the frame is valid while self lives.
        let f = unsafe { self.frame.as_ref() }?;
        lent(f).map(|l| &l.picture)
    }

    /// The decoder's buffer, when the layout is Q10c. Such frames keep the
    /// whole buffer in `data[0]`, the UV plane's start in `data[1]`, the
    /// stride in `linesize[0]` and the buffer's luma rows in `linesize[2]`.
    pub fn q10c(&self) -> Option<Q10cPicture<'_>> {
        if self.layout != PixelLayout::Q10c {
            return None;
        }
        // SAFETY: Q10c frames are built by `set_q10c`: buf[0] holds the
        // whole picture starting at data[0], and data[1] lies inside it.
        unsafe {
            let f = &*self.frame;
            if f.buf[0].is_null() || f.data[0].is_null() || f.data[1].is_null() {
                return None;
            }
            let len = (*f.buf[0]).size;
            let uv_offset = f.data[1].offset_from(f.data[0]);
            let data = std::slice::from_raw_parts(f.data[0], len);
            (uv_offset > 0 && (uv_offset as usize) < len).then_some(Q10cPicture {
                data,
                stride: f.linesize[0] as u32,
                rows: f.linesize[2] as u32,
                uv_offset: uv_offset as usize,
            })
        }
    }

    /// Copies the frame into tightly packed planes (for tests and thumbnails).
    pub fn packed_planes(&self) -> Vec<Vec<u8>> {
        self.planes()
            .iter()
            .map(|p| {
                let row = (p.width * p.components * p.bytes_per_sample) as usize;
                let mut out = Vec::with_capacity(row * p.height as usize);
                for y in 0..p.height as usize {
                    out.extend_from_slice(&p.data[y * p.stride..y * p.stride + row]);
                }
                out
            })
            .collect()
    }
}

/// Makes `f` a Q10c frame holding the picture in `buf`, laid out as
/// [`VideoFrame::q10c`] reads it back.
///
/// # Safety
/// `f` is an unreferenced frame and takes over the reference `buf`, which
/// holds the whole picture; `uv_offset` lies inside it.
pub(crate) unsafe fn set_q10c(
    f: &mut ff::AVFrame,
    buf: *mut ff::AVBufferRef,
    stride: u32,
    rows: u32,
    uv_offset: usize,
    size: (u32, u32),
) {
    // SAFETY: buf is a valid reference with uv_offset inside it (caller).
    unsafe {
        f.buf[0] = buf;
        f.data[0] = (*buf).data;
        f.data[1] = (*buf).data.add(uv_offset);
    }
    f.linesize[0] = stride as i32;
    f.linesize[1] = stride as i32;
    f.linesize[2] = rows as i32;
    f.format = PIX_FMT_Q10C;
    f.width = size.0 as i32;
    f.height = size.1 as i32;
}

fn color_info(f: &ff::AVFrame) -> ColorInfo {
    let matrix = match f.colorspace {
        ff::AVCOL_SPC_BT709 => Matrix::Bt709,
        ff::AVCOL_SPC_BT2020_NCL | ff::AVCOL_SPC_BT2020_CL => Matrix::Bt2020,
        ff::AVCOL_SPC_BT470BG | ff::AVCOL_SPC_SMPTE170M => Matrix::Bt601,
        // Unspecified: HD and larger is BT.709 by convention.
        _ => {
            if f.height >= 720 {
                Matrix::Bt709
            } else {
                Matrix::Bt601
            }
        }
    };
    let transfer = match f.color_trc {
        ff::AVCOL_TRC_SMPTE2084 => Transfer::Pq,
        ff::AVCOL_TRC_ARIB_STD_B67 => Transfer::Hlg,
        _ => Transfer::Sdr,
    };
    ColorInfo {
        matrix,
        full_range: f.color_range == ff::AVCOL_RANGE_JPEG || f.format == ff::AV_PIX_FMT_YUVJ420P,
        transfer,
        wide_gamut: f.color_primaries == ff::AVCOL_PRI_BT2020,
    }
}

/// Converts decoder output into a [`VideoFrame`], rescaling unusual pixel
/// formats with swscale.
pub(crate) struct FrameConverter {
    sws: *mut ff::SwsContext,
    sws_key: (i32, i32, i32, i32),
}

// SAFETY: used by one decode thread at a time.
unsafe impl Send for FrameConverter {}

impl Default for FrameConverter {
    fn default() -> Self {
        FrameConverter {
            sws: std::ptr::null_mut(),
            sws_key: (0, 0, 0, 0),
        }
    }
}

impl Drop for FrameConverter {
    fn drop(&mut self) {
        // SAFETY: frees our own context (null-safe).
        unsafe { ff::sws_freeContext(self.sws) };
    }
}

impl FrameConverter {
    /// Takes ownership of `src` (a decoded frame reference).
    pub(crate) fn convert(
        &mut self,
        mut src: *mut ff::AVFrame,
        pts: f64,
        duration: f64,
        serial: u64,
    ) -> Result<VideoFrame> {
        if src.is_null() {
            return Err(Error::Unsupported("null frame".into()));
        }
        // SAFETY: src is a valid decoded frame we own.
        let f = unsafe { &*src };
        if f.width <= 0 || f.height <= 0 {
            let mut s = src;
            // SAFETY: src is ours and not used after this.
            unsafe { ff::av_frame_free(&mut s) };
            return Err(Error::Unsupported("invalid frame dimensions".into()));
        }

        // A hardware or DRM_PRIME frame is transferred to system memory.
        // SAFETY: src is valid; sw is checked for null before use, and
        // whichever frame is dropped is freed exactly once.
        unsafe {
            let desc = ff::av_pix_fmt_desc_get(f.format);
            let is_hw = f.format == ff::AV_PIX_FMT_DRM_PRIME
                || (!desc.is_null() && ((*desc).flags & ff::AV_PIX_FMT_FLAG_HWACCEL as u64 != 0))
                || !f.hw_frames_ctx.is_null();
            if is_hw {
                let mut sw = ff::av_frame_alloc();
                if !sw.is_null() && ff::av_hwframe_transfer_data(sw, src, 0) >= 0 {
                    (*sw).pts = (*src).pts;
                    (*sw).duration = (*src).duration;
                    ff::av_frame_free(&mut src);
                    src = sw;
                } else if !sw.is_null() {
                    ff::av_frame_free(&mut sw);
                }
            }
        }
        // SAFETY: src is still a valid frame we own (possibly the copy).
        let f = unsafe { &*src };
        let layout = match f.format {
            ff::AV_PIX_FMT_YUV420P | ff::AV_PIX_FMT_YUVJ420P => Some(PixelLayout::I420 { bits: 8 }),
            ff::AV_PIX_FMT_YUV420P10LE => Some(PixelLayout::I420 { bits: 10 }),
            ff::AV_PIX_FMT_NV12 => Some(PixelLayout::Nv12),
            ff::AV_PIX_FMT_P010LE => Some(PixelLayout::P010),
            PIX_FMT_Q10C => Some(PixelLayout::Q10c),
            PIX_FMT_SHARED => lent(f).map(|l| l.layout),
            _ => None,
        }
        // Rows stored bottom up (negative strides) go through swscale, which
        // turns them the right way up.
        .filter(|_| f.linesize[..3].iter().all(|&l| l >= 0));
        let color = color_info(f);
        let (width, height) = (f.width.max(0) as u32, f.height.max(0) as u32);
        if let Some(layout) = layout {
            return Ok(VideoFrame {
                pts,
                duration,
                width,
                height,
                layout,
                color,
                serial,
                id: next_id(),
                frame: src,
            });
        }
        // Anything else: convert to 8- or 10-bit I420.
        // SAFETY: pixdesc lookups and swscale on valid frames; the new frame is
        // allocated with av_frame_get_buffer before scaling into it.
        unsafe {
            // Read before any error path frees the frame `f` points into.
            let format = f.format;
            let desc = ff::av_pix_fmt_desc_get(format);
            if desc.is_null() || ((*desc).flags & ff::AV_PIX_FMT_FLAG_HWACCEL as u64 != 0) {
                let mut s = src;
                ff::av_frame_free(&mut s);
                return Err(Error::Unsupported(format!(
                    "unsupported pixel format {format}"
                )));
            }
            let deep = (*desc).comp[0].depth > 8;
            let (dst_fmt, bits) = if deep {
                (ff::AV_PIX_FMT_YUV420P10LE, 10)
            } else {
                (ff::AV_PIX_FMT_YUV420P, 8)
            };
            let key = (f.width, f.height, f.format, dst_fmt);
            if self.sws.is_null() || self.sws_key != key {
                ff::sws_freeContext(self.sws);
                self.sws = ff::sws_getContext(
                    f.width,
                    f.height,
                    f.format,
                    f.width,
                    f.height,
                    dst_fmt,
                    // The fast paths produce wrong chroma in some builds;
                    // accurate rounding with full chroma input is exact.
                    (ff::SWS_BILINEAR | ff::SWS_ACCURATE_RND | ff::SWS_FULL_CHR_H_INP) as i32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
                self.sws_key = key;
                if self.sws.is_null() {
                    let mut s = src;
                    ff::av_frame_free(&mut s);
                    return Err(Error::Unsupported(format!("pixel format {format}")));
                }
            }
            let mut dst = ff::av_frame_alloc();
            if dst.is_null() {
                let mut s = src;
                ff::av_frame_free(&mut s);
                return Err(Error::Unsupported("frame allocation failed".into()));
            }
            (*dst).format = dst_fmt;
            (*dst).width = f.width;
            (*dst).height = f.height;
            (*dst).colorspace = f.colorspace;
            (*dst).color_range = f.color_range;
            (*dst).color_trc = f.color_trc;
            (*dst).color_primaries = f.color_primaries;
            let r = ff::av_frame_get_buffer(dst, 0);
            if r < 0 {
                ff::av_frame_free(&mut dst);
                let mut s = src;
                ff::av_frame_free(&mut s);
                return Err(Error::Unsupported("frame allocation failed".into()));
            }
            ff::sws_scale(
                self.sws,
                f.data.as_ptr() as *const *const u8,
                f.linesize.as_ptr(),
                0,
                f.height,
                (*dst).data.as_ptr(),
                (*dst).linesize.as_ptr(),
            );
            let mut s = src;
            ff::av_frame_free(&mut s);
            Ok(VideoFrame {
                pts,
                duration,
                width,
                height,
                layout: PixelLayout::I420 { bits },
                color,
                serial,
                id: next_id(),
                frame: dst,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A shared frame of `layout` whose release counts into `released`.
    fn shared_frame(layout: PixelLayout, released: &Arc<AtomicUsize>) -> *mut ff::AVFrame {
        let picture = SharedPicture {
            fd: 7,
            len: 4096,
            stride: 64,
            rows: 32,
            uv_offset: 2048,
            buffer: 1,
            set: 1,
        };
        let r = released.clone();
        let lent = Box::new(Lent {
            picture,
            layout,
            release: Some(Box::new(move || {
                r.fetch_add(1, Ordering::SeqCst);
            })),
        });
        // SAFETY: a fresh frame, then made shared.
        unsafe {
            let f = ff::av_frame_alloc();
            set_shared(&mut *f, lent, (60, 30)).unwrap();
            f
        }
    }

    #[test]
    fn a_shared_frame_gives_its_buffer_back_once_its_last_copy_goes() {
        let released = Arc::new(AtomicUsize::new(0));
        let raw = shared_frame(PixelLayout::Nv12, &released);
        // SAFETY: a valid frame; the clone adds a reference to its buffer.
        let mut copy = unsafe { ff::av_frame_clone(raw) };
        let frame = FrameConverter::default()
            .convert(raw, 1.0, 0.04, 3)
            .unwrap();
        // SAFETY: our own reference.
        unsafe { ff::av_frame_free(&mut copy) };
        assert_eq!(
            released.load(Ordering::SeqCst),
            0,
            "the frame still holds it"
        );
        assert_eq!(frame.layout, PixelLayout::Nv12);
        assert_eq!((frame.width, frame.height, frame.serial), (60, 30, 3));
        assert_eq!(frame.shared().map(|p| (p.fd, p.uv_offset)), Some((7, 2048)));
        // Only the GPU reads it.
        assert!(frame.planes().is_empty());
        drop(frame);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_shared_q10c_frame_has_no_cpu_picture() {
        let released = Arc::new(AtomicUsize::new(0));
        let frame = FrameConverter::default()
            .convert(shared_frame(PixelLayout::Q10c, &released), 0.0, 0.0, 0)
            .unwrap();
        assert_eq!(frame.layout, PixelLayout::Q10c);
        assert!(frame.q10c().is_none());
        assert_eq!(frame.shared().map(|p| p.rows), Some(32));
        drop(frame);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn copied_frames_are_not_shared() {
        let rgba = vec![255u8; 16 * 16 * 4];
        let f = VideoFrame::from_rgba_as(16, 16, &rgba, PixelLayout::Nv12, false).unwrap();
        assert!(f.shared().is_none());
        assert!(!f.planes().is_empty());
    }

    #[test]
    fn from_rgba_grey_has_neutral_chroma() {
        let rgba: Vec<u8> = (0..64 * 32).flat_map(|_| [128u8, 128, 128, 255]).collect();
        let f = VideoFrame::from_rgba(64, 32, &rgba).unwrap();
        let p = f.packed_planes();
        assert_eq!(p.len(), 3);
        assert_eq!(
            (p[0].len(), p[1].len(), p[2].len()),
            (64 * 32, 32 * 16, 32 * 16)
        );
        let avg = |v: &[u8]| v.iter().map(|&x| x as u32).sum::<u32>() / v.len() as u32;
        assert!((avg(&p[0]) as i32 - 128).abs() <= 2, "Y {}", avg(&p[0]));
        assert!((avg(&p[1]) as i32 - 128).abs() <= 2, "U {}", avg(&p[1]));
        assert!((avg(&p[2]) as i32 - 128).abs() <= 2, "V {}", avg(&p[2]));
        assert!(f.color.full_range);
        // Saturated colours survive the round trip's chroma.
        let red: Vec<u8> = (0..16 * 16).flat_map(|_| [255u8, 0, 0, 255]).collect();
        let r = VideoFrame::from_rgba(16, 16, &red).unwrap().packed_planes();
        // BT.709 full range red: Y ~54, Cb ~99, Cr 255.
        assert!(
            (r[0][0] as i32 - 54).abs() <= 3 && (r[1][0] as i32 - 99).abs() <= 3 && r[2][0] >= 250,
            "{} {} {}",
            r[0][0],
            r[1][0],
            r[2][0]
        );
    }

    /// A planar frame `w`×`h` whose luma rows hold their row number.
    fn numbered_rows(w: i32, h: i32, format: ff::AVPixelFormat) -> *mut ff::AVFrame {
        // SAFETY: a fresh frame, checked, given its own buffers.
        unsafe {
            let f = ff::av_frame_alloc();
            assert!(!f.is_null());
            (*f).format = format;
            (*f).width = w;
            (*f).height = h;
            assert!(ff::av_frame_get_buffer(f, 0) >= 0);
            for y in 0..h {
                let row = (*f).data[0].offset((y * (*f).linesize[0]) as isize);
                std::ptr::write_bytes(row, y as u8, w as usize);
            }
            for i in 1..3 {
                std::ptr::write_bytes((*f).data[i], 128, ((*f).linesize[i] * h / 2) as usize);
            }
            f
        }
    }

    #[test]
    fn bottom_up_rows_come_out_the_right_way_up() {
        let (w, h) = (16, 16);
        let f = numbered_rows(w, h, ff::AV_PIX_FMT_YUV420P);
        // SAFETY: each plane's pointer moves to its last row, inside its own
        // buffer; the buffers (not these pointers) are what gets freed.
        unsafe {
            for i in 0..3 {
                let rows = if i == 0 { h } else { h / 2 };
                (*f).data[i] = (*f).data[i].offset(((rows - 1) * (*f).linesize[i]) as isize);
                (*f).linesize[i] = -(*f).linesize[i];
            }
        }
        let frame = FrameConverter::default().convert(f, 0.0, 0.0, 0).unwrap();
        let p = frame.packed_planes();
        let firsts: Vec<u8> = (0..h as usize).map(|y| p[0][y * w as usize]).collect();
        let want: Vec<u8> = (0..h as u8).rev().collect();
        assert_eq!(firsts, want);
    }

    #[test]
    fn an_unsupported_format_reports_itself() {
        let f = numbered_rows(16, 16, ff::AV_PIX_FMT_YUV420P);
        // A format FFmpeg has no description for.
        // SAFETY: only the format number changes.
        unsafe { (*f).format = 12345 };
        let Err(e) = FrameConverter::default().convert(f, 0.0, 0.0, 0) else {
            panic!("converted an unknown format");
        };
        assert!(e.to_string().contains("12345"), "{e}");
    }
}
