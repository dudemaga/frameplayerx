//! Measures each new HDR frame for [`crate::hdr::PeakDetector`]: a compute
//! pass (shaders/peak.wgsl) over about a quarter of a million samples of the
//! frame, whatever its size, counts their brightness into a histogram in
//! the frame slot's own buffer, read once that slot's fence has signalled.

use crate::gpu::Gpu;
use crate::hdr::BINS;
use crate::mem::Buffer;
use crate::params;
use crate::video::VideoPlanes;
use crate::{Error, Result, VkContext};
use ash::vk;
use fp_media::PixelLayout;
use fp_media::info::Transfer;
use gpu_allocator::MemoryLocation;

/// Samples measured per frame, about.
const SAMPLES: u32 = 1 << 18;
/// Workgroup size each way (peak.wgsl).
const GROUP: u32 = 16;

/// `struct Measure` in peak.wgsl.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Measure {
    yuv: [[f32; 4]; 4],
    mode: [u32; 4],
}

struct SlotPeak {
    hist: Buffer,
    set: vk::DescriptorSet,
    /// The slot's submission measures a frame.
    pending: bool,
}

pub(crate) struct PeakPass {
    pool: vk::DescriptorPool,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    slots: Vec<SlotPeak>,
}

/// Grid step, in luma texels, that takes about [`SAMPLES`] from a frame.
fn step(width: u32, height: u32) -> u32 {
    let area = width as f64 * height as f64;
    (area / SAMPLES as f64).sqrt().ceil().max(1.0) as u32
}

impl PeakPass {
    /// The pass, with a histogram for each of `slots` frame slots.
    pub fn new(gpu: &Gpu, slots: usize) -> Result<PeakPass> {
        let mut p = PeakPass {
            pool: vk::DescriptorPool::null(),
            set_layout: vk::DescriptorSetLayout::null(),
            layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            slots: Vec::new(),
        };
        if let Err(e) = p.create(gpu, slots) {
            p.destroy(gpu);
            return Err(e);
        }
        Ok(p)
    }

    fn create(&mut self, gpu: &Gpu, slots: usize) -> Result<()> {
        let d = &gpu.device;
        let bindings = [
            (0, vk::DescriptorType::STORAGE_BUFFER),
            (1, vk::DescriptorType::SAMPLED_IMAGE),
            (2, vk::DescriptorType::SAMPLED_IMAGE),
            (3, vk::DescriptorType::SAMPLED_IMAGE),
        ]
        .map(|(b, t)| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(t)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        });
        let n = slots as u32;
        let sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: n,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLED_IMAGE,
                descriptor_count: 3 * n,
            },
        ];
        // SAFETY: plain object creation on a valid device; everything made
        // here is destroyed by `destroy`.
        unsafe {
            self.set_layout = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
                .ctx("peak descriptor set layout")?;
            let sets = [self.set_layout];
            let ranges = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .size(size_of::<Measure>() as u32)];
            self.layout = d
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&sets)
                        .push_constant_ranges(&ranges),
                    None,
                )
                .ctx("peak pipeline layout")?;
            let module = gpu.shader(crate::PEAK_SPV)?;
            let info = vk::ComputePipelineCreateInfo::default()
                .stage(
                    vk::PipelineShaderStageCreateInfo::default()
                        .stage(vk::ShaderStageFlags::COMPUTE)
                        .module(module)
                        .name(c"main"),
                )
                .layout(self.layout);
            let pipes = d.create_compute_pipelines(vk::PipelineCache::null(), &[info], None);
            d.destroy_shader_module(module, None);
            self.pipeline = pipes.map_err(|(_, result)| Error::Vk {
                context: "peak pipeline",
                result,
            })?[0];
            self.pool = d
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(n)
                        .pool_sizes(&sizes),
                    None,
                )
                .ctx("peak descriptor pool")?;
            for _ in 0..slots {
                let layouts = [self.set_layout];
                let set = d
                    .allocate_descriptor_sets(
                        &vk::DescriptorSetAllocateInfo::default()
                            .descriptor_pool(self.pool)
                            .set_layouts(&layouts),
                    )
                    .ctx("peak descriptor set")?[0];
                let hist = Buffer::new(
                    gpu,
                    (BINS * 4) as u64,
                    vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
                    MemoryLocation::GpuToCpu,
                    "HDR histogram",
                )?;
                self.slots.push(SlotPeak {
                    hist,
                    set,
                    pending: false,
                });
            }
        }
        Ok(())
    }

    /// Records the measurement of `planes`, the image set an HDR frame was
    /// just written to, into slot `slot`'s command buffer `cmd`. Frames of
    /// other transfers are left alone.
    pub fn record(&mut self, gpu: &Gpu, cmd: vk::CommandBuffer, slot: usize, planes: &VideoPlanes) {
        let desc = planes.desc;
        let transfer = match desc.color.transfer {
            Transfer::Pq => 1,
            Transfer::Hlg => 2,
            Transfer::Sdr => return,
        };
        let Some(s) = self.slots.get_mut(slot) else {
            return;
        };
        let views: Vec<vk::ImageView> = planes.images.iter().map(|i| i.views[0]).collect();
        let (three, plane_code) = match (desc.layout, views.as_slice()) {
            (PixelLayout::I420 { .. }, [y, u, v]) => ([*y, *u, *v], 0),
            (PixelLayout::Nv12 | PixelLayout::P010 | PixelLayout::Q10c, [y, uv, ..]) => {
                ([*y, *uv, *uv], 1)
            }
            _ => return,
        };
        let (rows, off) = params::yuv_matrix(&desc.color, desc.layout);
        let step = step(desc.width, desc.height);
        let push = Measure {
            yuv: [
                rows[0].to_array(),
                rows[1].to_array(),
                rows[2].to_array(),
                off.to_array(),
            ],
            mode: [plane_code, transfer, desc.color.wide_gamut as u32, step],
        };
        let buffer = [vk::DescriptorBufferInfo::default()
            .buffer(s.hist.buffer)
            .range(vk::WHOLE_SIZE)];
        let images = three.map(|v| {
            [vk::DescriptorImageInfo::default()
                .image_view(v)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
        });
        let mut writes = vec![
            vk::WriteDescriptorSet::default()
                .dst_set(s.set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&buffer),
        ];
        for (i, info) in images.iter().enumerate() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(s.set)
                    .dst_binding(1 + i as u32)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(info),
            );
        }
        let groups = (
            desc.width.div_ceil(step).div_ceil(GROUP),
            desc.height.div_ceil(step).div_ceil(GROUP),
        );
        let d = &gpu.device;
        let buffer_barrier =
            |src: (vk::PipelineStageFlags2, vk::AccessFlags2),
             dst: (vk::PipelineStageFlags2, vk::AccessFlags2)| {
                vk::BufferMemoryBarrier2::default()
                    .buffer(s.hist.buffer)
                    .size(vk::WHOLE_SIZE)
                    .src_stage_mask(src.0)
                    .src_access_mask(src.1)
                    .dst_stage_mask(dst.0)
                    .dst_access_mask(dst.1)
            };
        let compute_rw = (
            vk::PipelineStageFlags2::COMPUTE_SHADER,
            vk::AccessFlags2::SHADER_STORAGE_READ | vk::AccessFlags2::SHADER_STORAGE_WRITE,
        );
        // SAFETY: the slot's set and buffer aren't in use (its fence was
        // waited for); the images stay alive until this slot comes round.
        unsafe {
            d.update_descriptor_sets(&writes, &[]);
            d.cmd_fill_buffer(cmd, s.hist.buffer, 0, vk::WHOLE_SIZE, 0);
            // The frame's planes were written by an upload or a conversion
            // for the eye passes; this reads them first.
            let written = [vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(
                    vk::AccessFlags2::TRANSFER_WRITE | vk::AccessFlags2::SHADER_STORAGE_WRITE,
                )
                .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)];
            let cleared = [buffer_barrier(crate::mem::TRANSFER_WRITE, compute_rw)];
            d.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default()
                    .memory_barriers(&written)
                    .buffer_memory_barriers(&cleared),
            );
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            d.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.layout,
                0,
                &[s.set],
                &[],
            );
            d.cmd_push_constants(
                cmd,
                self.layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                bytemuck::bytes_of(&push),
            );
            d.cmd_dispatch(cmd, groups.0, groups.1, 1);
            let host = (vk::PipelineStageFlags2::HOST, vk::AccessFlags2::HOST_READ);
            let counted = [buffer_barrier(compute_rw, host)];
            d.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().buffer_memory_barriers(&counted),
            );
        }
        s.pending = true;
    }

    /// The histogram slot `slot`'s last submission counted, if it measured a
    /// frame; call once its fence has signalled.
    pub fn take(&mut self, slot: usize) -> Option<[u32; BINS]> {
        let s = self.slots.get_mut(slot)?;
        if !std::mem::take(&mut s.pending) {
            return None;
        }
        let bytes = s.hist.bytes();
        if bytes.len() < BINS * 4 {
            return None;
        }
        let mut hist = [0u32; BINS];
        for (h, b) in hist.iter_mut().zip(bytes.as_chunks::<4>().0) {
            *h = u32::from_le_bytes(*b);
        }
        Some(hist)
    }

    /// Forgets measurements still on their way (another video).
    pub fn discard(&mut self) {
        for s in &mut self.slots {
            s.pending = false;
        }
    }

    pub fn destroy(&mut self, gpu: &Gpu) {
        for mut s in self.slots.drain(..) {
            s.hist.destroy(gpu);
        }
        // SAFETY: not in use (the renderer waits for its frames first); null
        // handles are ignored, and destroying the pool frees its sets.
        unsafe {
            let d = &gpu.device;
            d.destroy_descriptor_pool(std::mem::take(&mut self.pool), None);
            d.destroy_pipeline(std::mem::take(&mut self.pipeline), None);
            d.destroy_pipeline_layout(std::mem::take(&mut self.layout), None);
            d.destroy_descriptor_set_layout(std::mem::take(&mut self.set_layout), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn about_the_same_number_of_samples_whatever_the_size() {
        for (w, h) in [
            (1920, 1080),
            (3840, 2160),
            (7680, 3840),
            (8192, 4096),
            (640, 360),
        ] {
            let s = step(w, h);
            let n = w.div_ceil(s) * h.div_ceil(s);
            assert!(n <= SAMPLES * 11 / 10, "{w}x{h}: step {s}, {n} samples");
            assert!(n >= SAMPLES / 2 || s == 1, "{w}x{h}: step {s}, {n} samples");
        }
        assert_eq!(step(320, 240), 1);
    }
}
