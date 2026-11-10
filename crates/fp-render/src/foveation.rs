//! Foveated rendering: away from where the eye looks, the projection shader
//! runs once for 2x2 or 4x4 pixels, through a fragment shading rate
//! attachment for each eye. Panels and the pointer keep every pixel.
//!
//! Not a fragment density map: Turnip applies one density per rendering
//! bin, and its bins are hundreds of pixels a side, so the sharp middle
//! can't be placed. A shading rate texel covers a few pixels a side.
//!
//! The zones are angles from the gaze, so they mean the same at any render
//! size, and moving the gaze moves them.

use crate::Result;
use crate::gpu::{Gpu, ShadingRateCaps};
use crate::mem::{self, Buffer, Image};
use ash::vk;
pub use fp_core::display::Foveation;
use glam::{Mat4, Vec3, Vec4};
use gpu_allocator::MemoryLocation;

/// Angles from the gaze, in degrees, past which a fragment shades 2x2
/// pixels, then 4x4.
fn zones(level: Foveation) -> Option<(f32, f32)> {
    match level {
        Foveation::Off => None,
        Foveation::Low => Some((30.0, 45.0)),
        Foveation::High => Some((18.0, 32.0)),
    }
}

/// The tangents (eye space, x right and y up over -z) of the directions
/// through the middle of each texel column and row of a `texels` map of
/// `texel`-pixel texels over an eye target of `target` pixels whose
/// projection's inverse is `inv_proj`, rows from the top. A projection has
/// no skew, so a column's x and a row's y don't depend on each other.
pub(crate) fn tangents(
    texels: (u32, u32),
    texel: (u32, u32),
    target: (u32, u32),
    inv_proj: Mat4,
) -> (Vec<f32>, Vec<f32>) {
    let tangent = |nx: f32, ny: f32| {
        let at = |z: f32| {
            let p = inv_proj * Vec4::new(nx, ny, z, 1.0);
            p.truncate() / p.w
        };
        let d = at(1.0) - at(0.0);
        (d.x / -d.z, d.y / -d.z)
    };
    // A texel's middle in pixels, then in normalised device coordinates
    // (y down, as Vulkan's).
    let ndc = |i: u32, texel: u32, size: u32| {
        let px = ((i as f32 + 0.5) * texel as f32).min(size as f32);
        2.0 * px / size as f32 - 1.0
    };
    let cols = (0..texels.0)
        .map(|i| tangent(ndc(i, texel.0, target.0), 0.0).0)
        .collect();
    let rows = (0..texels.1)
        .map(|i| tangent(0.0, ndc(i, texel.1, target.1)).1)
        .collect();
    (cols, rows)
}

/// Texels decided together, this many a side: 32 pixels at 8x8 texels,
/// under a degree at the eye sizes rendered. The rates are rewritten every
/// frame the gaze moves, and this keeps that cheap.
const BLOCK: usize = 4;

/// Writes into `out` the shading rate of each texel of a map whose columns
/// and rows have tangents `cols` and `rows`, row by row, for an eye looking
/// along `gaze`.
pub(crate) fn fill_rates(out: &mut [u8], cols: &[f32], rows: &[f32], gaze: Vec3, level: Foveation) {
    let (w, h) = (cols.len(), rows.len());
    let Some(out) = out.get_mut(..w * h) else {
        return;
    };
    let Some((half, quarter)) = zones(level) else {
        out.fill(rate(1));
        return;
    };
    let g = gaze.normalize_or(Vec3::NEG_Z);
    // Smaller angles have larger cosines.
    let (half, quarter) = (half.to_radians().cos(), quarter.to_radians().cos());
    // A tangent changes linearly across the image, so a block's middle is
    // halfway between its first and last texel's.
    let middle = |t: &[f32], start: usize| (t[start] + t[(start + BLOCK).min(t.len()) - 1]) / 2.0;
    for by in (0..h).step_by(BLOCK) {
        let ty = middle(rows, by);
        for bx in (0..w).step_by(BLOCK) {
            let tx = middle(cols, bx);
            let c = (tx * g.x + ty * g.y - g.z) / (tx * tx + ty * ty + 1.0).sqrt();
            let r = rate(if c > half {
                1
            } else if c > quarter {
                2
            } else {
                4
            });
            for y in by..(by + BLOCK).min(h) {
                out[y * w + bx..y * w + (bx + BLOCK).min(w)].fill(r);
            }
        }
    }
}

/// A fragment `side` pixels square as a shading rate texel:
/// log2(width) << 2 | log2(height).
fn rate(side: u8) -> u8 {
    let l = side.max(1).ilog2() as u8;
    (l << 2) | l
}

/// One eye's shading rates in one frame slot.
struct Map {
    image: Image,
    /// What the CPU writes, copied into `image` when it changes.
    staging: Buffer,
    texels: (u32, u32),
    /// The eye target it was made for.
    target: (u32, u32),
    /// Each column's and row's tangent, for `inv_proj`.
    cols: Vec<f32>,
    rows: Vec<f32>,
    inv_proj: Mat4,
    /// What it was last written for.
    written: Option<(Vec3, Foveation)>,
}

/// The shading rates of every frame slot's two eyes.
pub(crate) struct RateMaps {
    caps: ShadingRateCaps,
    maps: Vec<[Option<Map>; 2]>,
}

impl RateMaps {
    pub fn new(gpu: &Gpu, slots: usize) -> Option<RateMaps> {
        let caps = gpu.shading_rates?;
        Some(RateMaps {
            caps,
            maps: (0..slots).map(|_| [None, None]).collect(),
        })
    }

    /// Slot `slot`'s rates for eye `eye`, written for a `target` eye target
    /// with inverse projection `inv_proj`, looking along `gaze`, and ready
    /// for the eye pass `cmd` records next. The slot's last frame has
    /// finished, and `cmd` is outside a render.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &mut self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        slot: usize,
        eye: usize,
        target: vk::Extent2D,
        inv_proj: Mat4,
        gaze: Vec3,
        level: Foveation,
    ) -> Result<vk::RenderingFragmentShadingRateAttachmentInfoKHR<'static>> {
        let target = (target.width, target.height);
        let texel = self.caps.texel;
        let entry = &mut self.maps[slot][eye];
        if entry.as_ref().is_some_and(|m| m.target != target)
            && let Some(mut old) = entry.take()
        {
            old.destroy(gpu);
        }
        let map = match entry {
            Some(m) => m,
            None => entry.insert(Map::new(gpu, texel, target)?),
        };
        if map.cols.is_empty() || map.inv_proj != inv_proj {
            (map.cols, map.rows) =
                tangents(map.texels, (texel.width, texel.height), target, inv_proj);
            map.inv_proj = inv_proj;
            map.written = None;
        }
        if map.written != Some((gaze, level)) {
            map.upload(gpu, cmd, gaze, level);
            map.written = Some((gaze, level));
        }
        Ok(vk::RenderingFragmentShadingRateAttachmentInfoKHR::default()
            .image_view(map.image.level0)
            .image_layout(vk::ImageLayout::FRAGMENT_SHADING_RATE_ATTACHMENT_OPTIMAL_KHR)
            .shading_rate_attachment_texel_size(texel))
    }

    pub fn destroy(&mut self, gpu: &Gpu) {
        for eyes in &mut self.maps {
            for m in eyes.iter_mut() {
                if let Some(mut m) = m.take() {
                    m.destroy(gpu);
                }
            }
        }
    }
}

impl Map {
    fn new(gpu: &Gpu, texel: vk::Extent2D, target: (u32, u32)) -> Result<Map> {
        // Padded: 270 texels wide, Turnip shaded a 16x32-texel block near
        // the right edge at a rate the image didn't hold.
        let texels = (
            target.0.div_ceil(texel.width.max(1)).next_multiple_of(64),
            target.1.div_ceil(texel.height.max(1)).next_multiple_of(64),
        );
        let mut staging = Buffer::new(
            gpu,
            u64::from(texels.0) * u64::from(texels.1),
            vk::BufferUsageFlags::TRANSFER_SRC,
            MemoryLocation::CpuToGpu,
            "shading rate staging",
        )?;
        let image = Image::new(
            gpu,
            texels.0,
            texels.1,
            &[vk::Format::R8_UINT],
            vk::ImageUsageFlags::FRAGMENT_SHADING_RATE_ATTACHMENT_KHR
                | vk::ImageUsageFlags::TRANSFER_DST,
            1,
            "shading rate",
        )
        .inspect_err(|_| staging.destroy(gpu))?;
        Ok(Map {
            image,
            staging,
            texels,
            target,
            cols: Vec::new(),
            rows: Vec::new(),
            inv_proj: Mat4::IDENTITY,
            written: None,
        })
    }

    /// Writes the rates for `gaze` and `level` and records copying them
    /// into the image.
    fn upload(&mut self, gpu: &Gpu, cmd: vk::CommandBuffer, gaze: Vec3, level: Foveation) {
        fill_rates(self.staging.bytes(), &self.cols, &self.rows, gaze, level);
        let attachment = (
            vk::PipelineStageFlags2::FRAGMENT_SHADING_RATE_ATTACHMENT_KHR,
            vk::AccessFlags2::FRAGMENT_SHADING_RATE_ATTACHMENT_READ_KHR,
        );
        let transfer = (
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
        );
        // The whole image is rewritten, so what it held can go.
        mem::barrier(
            gpu,
            cmd,
            self.image.image,
            mem::range(0, 1),
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            attachment,
            transfer,
        );
        let region = vk::BufferImageCopy::default()
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D {
                width: self.texels.0,
                height: self.texels.1,
                depth: 1,
            });
        // SAFETY: recording outside a render; the buffer holds the whole
        // image, tightly packed.
        unsafe {
            gpu.device.cmd_copy_buffer_to_image(
                cmd,
                self.staging.buffer,
                self.image.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
        }
        mem::barrier(
            gpu,
            cmd,
            self.image.image,
            mem::range(0, 1),
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::FRAGMENT_SHADING_RATE_ATTACHMENT_OPTIMAL_KHR,
            transfer,
            attachment,
        );
    }

    fn destroy(&mut self, gpu: &Gpu) {
        self.image.destroy(gpu);
        self.staging.destroy(gpu);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::{Fov, projection};

    /// Each texel's fragment side (1, 2 or 4).
    fn map(level: Foveation, gaze: Vec3) -> Vec<u8> {
        let inv = projection(Fov::symmetric(100.0), 0.05, 100.0).inverse();
        let (cols, rows) = tangents((96, 96), (32, 32), (3072, 3072), inv);
        let mut out = vec![0; 96 * 96];
        fill_rates(&mut out, &cols, &rows, gaze, level);
        out.iter().map(|r| 1 << (r & 3)).collect()
    }

    #[test]
    fn off_shades_every_pixel() {
        assert!(map(Foveation::Off, Vec3::NEG_Z).iter().all(|&s| s == 1));
    }

    #[test]
    fn fragments_grow_away_from_the_gaze() {
        let m = map(Foveation::High, Vec3::NEG_Z);
        let at = |x: usize, y: usize| m[y * 96 + x];
        assert_eq!(at(47, 47), 1, "the middle is sharp");
        assert_eq!(at(0, 0), 4, "a corner shades 4x4");
        // Symmetric about the middle.
        assert_eq!(at(10, 48), at(85, 48));
        assert_eq!(at(48, 10), at(48, 85));
        let sharp = m.iter().filter(|&&s| s == 1).count();
        let low_sharp = map(Foveation::Low, Vec3::NEG_Z)
            .iter()
            .filter(|&&s| s == 1)
            .count();
        assert!(
            sharp < low_sharp,
            "high keeps less sharp: {sharp} vs {low_sharp}"
        );
    }

    #[test]
    fn the_sharp_zone_follows_the_gaze() {
        // Looking 25 degrees right: the right side is now the sharp one.
        let right = Vec3::new(25f32.to_radians().sin(), 0.0, -25f32.to_radians().cos());
        let m = map(Foveation::High, right);
        let at = |x: usize, y: usize| m[y * 96 + x];
        assert_eq!(at(47, 47), 2);
        let sharp: Vec<usize> = (0..96).filter(|&x| at(x, 48) == 1).collect();
        let middle = sharp.iter().sum::<usize>() / sharp.len().max(1);
        assert!(middle > 60, "sharp columns {sharp:?}");
    }

    #[test]
    fn up_is_the_top_row() {
        let up = Vec3::new(0.0, 25f32.to_radians().sin(), -25f32.to_radians().cos());
        let m = map(Foveation::High, up);
        let sharp_row = (0..96).find(|&y| m[y * 96 + 48] == 1).unwrap();
        assert!(sharp_row < 48, "first sharp row {sharp_row}");
    }

    #[test]
    fn rates_encode_both_axes() {
        assert_eq!(rate(1), 0);
        assert_eq!(rate(2), 0b0101);
        assert_eq!(rate(4), 0b1010);
    }
}
