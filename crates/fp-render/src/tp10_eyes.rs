//! The eye passes for a Q10C picture sampled in the decoder's buffer: the
//! scene shader built with `planes_tp10.wgsl`, so a 10-bit frame needs no
//! conversion pass. Its TP10 texture descriptors exist only as bytes
//! ([`crate::q10c`]), which only a descriptor buffer can hold, and a
//! pipeline reading one descriptor buffer reads all its descriptors from
//! them: so each frame slot has its own, with the scene's parameters in a
//! buffer with a device address beside it.

use crate::gpu::Gpu;
use crate::params::SceneParams;
use crate::pipeline::{self, PipelineDesc};
use crate::q10c::{self, MAX_CHROMA_BANDS, MAX_LUMA_BANDS, Sampled};
use crate::{Error, Result, VkContext};
use ash::vk;

/// Binding of the first luma band; the chroma bands follow the luma ones.
const LUMA: u32 = 2;
const CHROMA: u32 = LUMA + MAX_LUMA_BANDS as u32;
const BINDINGS: u32 = CHROMA + MAX_CHROMA_BANDS as u32;

/// One frame slot's descriptors and parameters, mapped for the CPU.
struct SlotBuffers {
    descriptors: vk::Buffer,
    descriptor_memory: vk::DeviceMemory,
    descriptor_address: vk::DeviceAddress,
    descriptor_bytes: *mut u8,
    params: vk::Buffer,
    params_memory: vk::DeviceMemory,
    params_bytes: *mut u8,
}

pub(crate) struct Tp10Eyes {
    db: ash::ext::descriptor_buffer::Device,
    set_layout: vk::DescriptorSetLayout,
    pub layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    set_size: u64,
    /// Where each binding's descriptor is in a slot's buffer.
    offsets: Vec<usize>,
    sampled_size: usize,
    slots: Vec<SlotBuffers>,
}

// SAFETY: the mapped pointers are written only by the thread driving the
// renderer, into memory these buffers own.
unsafe impl Send for Tp10Eyes {}

impl Tp10Eyes {
    /// The pipeline and `slots` frame slots' buffers, drawing into
    /// `color_format` and sampling through `sampler`.
    pub fn new(
        gpu: &Gpu,
        color_format: vk::Format,
        sampler: vk::Sampler,
        slots: usize,
    ) -> Result<Tp10Eyes> {
        let mut t = Tp10Eyes {
            db: ash::ext::descriptor_buffer::Device::new(&gpu.instance, &gpu.device),
            set_layout: vk::DescriptorSetLayout::null(),
            layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            set_size: 0,
            offsets: Vec::new(),
            sampled_size: 0,
            slots: Vec::new(),
        };
        if let Err(e) = t.create(gpu, color_format, sampler, slots) {
            t.destroy(gpu);
            return Err(e);
        }
        Ok(t)
    }

    fn create(
        &mut self,
        gpu: &Gpu,
        color_format: vk::Format,
        sampler: vk::Sampler,
        slots: usize,
    ) -> Result<()> {
        let mut dbp = vk::PhysicalDeviceDescriptorBufferPropertiesEXT::default();
        let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut dbp);
        // SAFETY: valid physical device.
        unsafe {
            gpu.instance
                .get_physical_device_properties2(gpu.pdev, &mut p2);
        }
        self.sampled_size = dbp.sampled_image_descriptor_size;
        if self.sampled_size != 64 {
            return Err(Error::Unsupported(format!(
                "{}-byte texture descriptors",
                self.sampled_size
            )));
        }
        let bindings: Vec<_> = (0..BINDINGS)
            .map(|b| {
                let ty = match b {
                    0 => vk::DescriptorType::UNIFORM_BUFFER,
                    1 => vk::DescriptorType::SAMPLER,
                    _ => vk::DescriptorType::SAMPLED_IMAGE,
                };
                vk::DescriptorSetLayoutBinding::default()
                    .binding(b)
                    .descriptor_type(ty)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            })
            .collect();
        let d = &gpu.device;
        // SAFETY: plain object creation on a valid device; everything made
        // here is destroyed by `destroy`.
        unsafe {
            self.set_layout = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default()
                        .flags(vk::DescriptorSetLayoutCreateFlags::DESCRIPTOR_BUFFER_EXT)
                        .bindings(&bindings),
                    None,
                )
                .ctx("TP10 eye descriptor set layout")?;
            self.set_size = self
                .db
                .get_descriptor_set_layout_size(self.set_layout)
                .next_multiple_of(dbp.descriptor_buffer_offset_alignment.max(1));
            self.offsets = (0..BINDINGS)
                .map(|b| {
                    self.db
                        .get_descriptor_set_layout_binding_offset(self.set_layout, b)
                        as usize
                })
                .collect();
        }
        self.layout = pipeline::layout(gpu, self.set_layout, 4)?;
        self.pipeline = pipeline::create(
            gpu,
            &PipelineDesc {
                spv: crate::SCENE_TP10_SPV,
                layout: self.layout,
                color_format,
                vertex_stride: 0,
                attributes: &[],
                blend: false,
                descriptor_buffer: true,
            },
        )?;
        for _ in 0..slots {
            let s = self.slot_buffers(gpu, &dbp, sampler)?;
            self.slots.push(s);
        }
        Ok(())
    }

    /// A slot's descriptor buffer and parameter buffer, with the parameters'
    /// and the sampler's descriptors written once for good.
    fn slot_buffers(
        &self,
        gpu: &Gpu,
        dbp: &vk::PhysicalDeviceDescriptorBufferPropertiesEXT,
        sampler: vk::Sampler,
    ) -> Result<SlotBuffers> {
        let d = &gpu.device;
        let mut s = SlotBuffers {
            descriptors: vk::Buffer::null(),
            descriptor_memory: vk::DeviceMemory::null(),
            descriptor_address: 0,
            descriptor_bytes: std::ptr::null_mut(),
            params: vk::Buffer::null(),
            params_memory: vk::DeviceMemory::null(),
            params_bytes: std::ptr::null_mut(),
        };
        let made = (|| -> Result<()> {
            let params_size = size_of::<SceneParams>() as u64;
            // SAFETY: buffers in host-visible memory with device addresses,
            // owned by the slot and mapped for its life.
            unsafe {
                for (buffer, memory, bytes, size, usage) in [
                    (
                        &mut s.descriptors,
                        &mut s.descriptor_memory,
                        &mut s.descriptor_bytes,
                        self.set_size,
                        vk::BufferUsageFlags::RESOURCE_DESCRIPTOR_BUFFER_EXT
                            | vk::BufferUsageFlags::SAMPLER_DESCRIPTOR_BUFFER_EXT,
                    ),
                    (
                        &mut s.params,
                        &mut s.params_memory,
                        &mut s.params_bytes,
                        params_size,
                        vk::BufferUsageFlags::UNIFORM_BUFFER,
                    ),
                ] {
                    *buffer = d
                        .create_buffer(
                            &vk::BufferCreateInfo::default()
                                .size(size)
                                .usage(usage | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS),
                            None,
                        )
                        .ctx("TP10 eye buffer")?;
                    let req = d.get_buffer_memory_requirements(*buffer);
                    *memory = q10c::allocate(gpu, req.size, req.memory_type_bits, true)?;
                    d.bind_buffer_memory(*buffer, *memory, 0)
                        .ctx("bind TP10 eye buffer")?;
                    *bytes = d
                        .map_memory(*memory, 0, size, vk::MemoryMapFlags::empty())
                        .ctx("map TP10 eye buffer")? as *mut u8;
                }
                let address = |buffer| {
                    d.get_buffer_device_address(
                        &vk::BufferDeviceAddressInfo::default().buffer(buffer),
                    )
                };
                s.descriptor_address = address(s.descriptors);
                let set =
                    std::slice::from_raw_parts_mut(s.descriptor_bytes, self.set_size as usize);
                let ubo = vk::DescriptorAddressInfoEXT::default()
                    .address(address(s.params))
                    .range(params_size);
                let at = self.offsets[0];
                self.db.get_descriptor(
                    &vk::DescriptorGetInfoEXT::default()
                        .ty(vk::DescriptorType::UNIFORM_BUFFER)
                        .data(vk::DescriptorDataEXT {
                            p_uniform_buffer: &ubo,
                        }),
                    &mut set[at..at + dbp.uniform_buffer_descriptor_size],
                );
                let at = self.offsets[1];
                self.db.get_descriptor(
                    &vk::DescriptorGetInfoEXT::default()
                        .ty(vk::DescriptorType::SAMPLER)
                        .data(vk::DescriptorDataEXT {
                            p_sampler: &sampler,
                        }),
                    &mut set[at..at + dbp.sampler_descriptor_size],
                );
            }
            Ok(())
        })();
        if let Err(e) = made {
            Self::free_slot(gpu, &mut s);
            return Err(e);
        }
        Ok(s)
    }

    /// Writes slot `slot`'s parameters and `picture`'s bands, for the frame
    /// being recorded in it (the slot's last one has finished).
    pub fn write(&mut self, slot: usize, params: &SceneParams, picture: &Sampled) {
        let Some(s) = self.slots.get(slot) else {
            return;
        };
        let bytes = bytemuck::bytes_of(params);
        // SAFETY: both mappings cover their buffers, which no submitted
        // frame reads any more.
        let (set, out) = unsafe {
            (
                std::slice::from_raw_parts_mut(s.descriptor_bytes, self.set_size as usize),
                std::slice::from_raw_parts_mut(s.params_bytes, bytes.len()),
            )
        };
        out.copy_from_slice(bytes);
        for (first, count, bands) in [
            (LUMA, MAX_LUMA_BANDS, &picture.luma),
            (CHROMA, MAX_CHROMA_BANDS, &picture.chroma),
        ] {
            for i in 0..count {
                // Bands past the picture's are never read; any valid one fills them.
                let Some(band) = bands.get(i).or(bands.first()) else {
                    continue;
                };
                let at = self.offsets[first as usize + i];
                set[at..at + self.sampled_size].copy_from_slice(bytemuck::cast_slice(band));
            }
        }
    }

    /// Binds the pipeline and slot `slot`'s descriptors for the eye passes.
    pub fn bind(&self, gpu: &Gpu, cmd: vk::CommandBuffer, slot: usize) {
        let Some(s) = self.slots.get(slot) else {
            return;
        };
        let binding = [vk::DescriptorBufferBindingInfoEXT::default()
            .address(s.descriptor_address)
            .usage(
                vk::BufferUsageFlags::RESOURCE_DESCRIPTOR_BUFFER_EXT
                    | vk::BufferUsageFlags::SAMPLER_DESCRIPTOR_BUFFER_EXT,
            )];
        // SAFETY: recording; the pipeline, layout and buffer are valid, and
        // the slot's descriptors were written for this frame.
        unsafe {
            gpu.device
                .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            self.db.cmd_bind_descriptor_buffers(cmd, &binding);
            self.db.cmd_set_descriptor_buffer_offsets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                self.layout,
                0,
                &[0],
                &[0],
            );
        }
    }

    fn free_slot(gpu: &Gpu, s: &mut SlotBuffers) {
        // SAFETY: not in use; freeing memory unmaps it; nulls are ignored.
        unsafe {
            let d = &gpu.device;
            d.destroy_buffer(std::mem::take(&mut s.descriptors), None);
            d.destroy_buffer(std::mem::take(&mut s.params), None);
            d.free_memory(std::mem::take(&mut s.descriptor_memory), None);
            d.free_memory(std::mem::take(&mut s.params_memory), None);
        }
    }

    pub fn destroy(&mut self, gpu: &Gpu) {
        for mut s in self.slots.drain(..) {
            Self::free_slot(gpu, &mut s);
        }
        // SAFETY: not in use (the device is idle); nulls are ignored.
        unsafe {
            let d = &gpu.device;
            d.destroy_pipeline(std::mem::take(&mut self.pipeline), None);
            d.destroy_pipeline_layout(std::mem::take(&mut self.layout), None);
            d.destroy_descriptor_set_layout(std::mem::take(&mut self.set_layout), None);
        }
    }
}
