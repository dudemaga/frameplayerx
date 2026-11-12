//! Buffers, images and barriers.

use crate::gpu::Gpu;
use crate::{Error, Result, VkContext};
use ash::vk;
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::{Allocation, AllocationCreateDesc, AllocationScheme};

pub struct Buffer {
    pub buffer: vk::Buffer,
    pub size: u64,
    alloc: Option<Allocation>,
}

impl Buffer {
    pub fn new(
        gpu: &Gpu,
        size: u64,
        usage: vk::BufferUsageFlags,
        location: MemoryLocation,
        name: &str,
    ) -> Result<Buffer> {
        let size = size.max(16);
        // SAFETY: valid create info.
        let buffer = unsafe {
            gpu.device.create_buffer(
                &vk::BufferCreateInfo::default().size(size).usage(usage),
                None,
            )
        }
        .ctx("create buffer")?;
        let mut b = Buffer {
            buffer,
            size,
            alloc: None,
        };
        // Out of memory, what was made so far is given back.
        if let Err(e) = b.back(gpu, location, name) {
            b.destroy(gpu);
            return Err(e);
        }
        Ok(b)
    }

    /// Gives the buffer its memory.
    fn back(&mut self, gpu: &Gpu, location: MemoryLocation, name: &str) -> Result<()> {
        // SAFETY: valid buffer.
        let requirements = unsafe { gpu.device.get_buffer_memory_requirements(self.buffer) };
        let alloc = self.alloc.insert(
            gpu.alloc()
                .allocate(&AllocationCreateDesc {
                    name,
                    requirements,
                    location,
                    linear: true,
                    allocation_scheme: AllocationScheme::GpuAllocatorManaged,
                })
                .map_err(|e| Error::Alloc(e.to_string()))?,
        );
        // SAFETY: memory from the allocator matches the requirements.
        unsafe {
            gpu.device
                .bind_buffer_memory(self.buffer, alloc.memory(), alloc.offset())
        }
        .ctx("bind buffer")
    }

    /// The mapped bytes of a host-visible buffer.
    pub fn bytes(&mut self) -> &mut [u8] {
        match self.alloc.as_mut().and_then(|a| a.mapped_slice_mut()) {
            Some(s) => s,
            None => &mut [],
        }
    }

    pub fn destroy(&mut self, gpu: &Gpu) {
        if let Some(a) = self.alloc.take() {
            let _ = gpu.alloc().free(a);
        }
        // SAFETY: the buffer is no longer in use (callers wait for fences).
        unsafe { gpu.device.destroy_buffer(self.buffer, None) };
        self.buffer = vk::Buffer::null();
    }
}

pub struct Image {
    pub image: vk::Image,
    pub format: vk::Format,
    pub extent: vk::Extent2D,
    pub mips: u32,
    /// One view per requested format, all mips.
    pub views: Vec<vk::ImageView>,
    /// Single-mip view of level 0 in the first format (render target).
    pub level0: vk::ImageView,
    alloc: Option<Allocation>,
}

impl Image {
    /// Creates a 2D image. `view_formats[0]` is the image format; more formats
    /// make the image mutable-format (e.g. UNORM rendering, sRGB sampling).
    pub fn new(
        gpu: &Gpu,
        width: u32,
        height: u32,
        view_formats: &[vk::Format],
        usage: vk::ImageUsageFlags,
        mips: u32,
        name: &str,
    ) -> Result<Image> {
        let format = view_formats[0];
        let mut format_list = vk::ImageFormatListCreateInfo::default().view_formats(view_formats);
        let mut info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: width.max(1),
                height: height.max(1),
                depth: 1,
            })
            .mip_levels(mips.max(1))
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        if view_formats.len() > 1 {
            info = info
                .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
                .push_next(&mut format_list);
        }
        // SAFETY: valid create info.
        let image = unsafe { gpu.device.create_image(&info, None) }.ctx("create image")?;
        let mut img = Image {
            image,
            format,
            extent: vk::Extent2D {
                width: width.max(1),
                height: height.max(1),
            },
            mips: mips.max(1),
            views: Vec::new(),
            level0: vk::ImageView::null(),
            alloc: None,
        };
        // Out of memory, what was made so far is given back.
        if let Err(e) = img.back(gpu, view_formats, name) {
            img.destroy(gpu);
            return Err(e);
        }
        Ok(img)
    }

    /// Gives the image its memory and views.
    fn back(&mut self, gpu: &Gpu, view_formats: &[vk::Format], name: &str) -> Result<()> {
        // SAFETY: valid image.
        let requirements = unsafe { gpu.device.get_image_memory_requirements(self.image) };
        let alloc = self.alloc.insert(
            gpu.alloc()
                .allocate(&AllocationCreateDesc {
                    name,
                    requirements,
                    location: MemoryLocation::GpuOnly,
                    linear: false,
                    allocation_scheme: AllocationScheme::GpuAllocatorManaged,
                })
                .map_err(|e| Error::Alloc(e.to_string()))?,
        );
        // SAFETY: memory matches the requirements.
        unsafe {
            gpu.device
                .bind_image_memory(self.image, alloc.memory(), alloc.offset())
        }
        .ctx("bind image")?;
        for &f in view_formats {
            self.views.push(view(gpu, self.image, f, 0, self.mips)?);
        }
        self.level0 = view(gpu, self.image, self.format, 0, 1)?;
        Ok(())
    }

    /// Destroys what the image holds; null handles (an image given up half
    /// made) are allowed.
    pub fn destroy(&mut self, gpu: &Gpu) {
        // SAFETY: not in use (callers defer destruction past in-flight frames).
        unsafe {
            for v in self.views.drain(..) {
                gpu.device.destroy_image_view(v, None);
            }
            gpu.device.destroy_image_view(self.level0, None);
            gpu.device.destroy_image(self.image, None);
        }
        if let Some(a) = self.alloc.take() {
            let _ = gpu.alloc().free(a);
        }
    }
}

pub fn view(
    gpu: &Gpu,
    image: vk::Image,
    format: vk::Format,
    base_mip: u32,
    mips: u32,
) -> Result<vk::ImageView> {
    // SAFETY: valid image and format compatible with it.
    unsafe {
        gpu.device.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(range(base_mip, mips)),
            None,
        )
    }
    .ctx("create image view")
}

pub fn range(base_mip: u32, mips: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: base_mip,
        level_count: mips,
        base_array_layer: 0,
        layer_count: 1,
    }
}

/// One image layout transition (synchronization2).
#[allow(clippy::too_many_arguments)]
pub fn barrier(
    gpu: &Gpu,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    sub: vk::ImageSubresourceRange,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
    src: (vk::PipelineStageFlags2, vk::AccessFlags2),
    dst: (vk::PipelineStageFlags2, vk::AccessFlags2),
) {
    let b = [vk::ImageMemoryBarrier2::default()
        .image(image)
        .subresource_range(sub)
        .old_layout(old)
        .new_layout(new)
        .src_stage_mask(src.0)
        .src_access_mask(src.1)
        .dst_stage_mask(dst.0)
        .dst_access_mask(dst.1)];
    // SAFETY: recording into a command buffer in the recording state.
    unsafe {
        gpu.device.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&b),
        );
    };
}

/// [`barrier`] over a whole single-mip image that also moves it from queue
/// family `families.0` to `families.1`: for memory the video decoder
/// writes, from the foreign family before the GPU reads it and back after
/// ([`Gpu::decoder_family`](crate::gpu::Gpu)).
#[allow(clippy::too_many_arguments)]
pub fn ownership_barrier(
    gpu: &Gpu,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
    src: (vk::PipelineStageFlags2, vk::AccessFlags2),
    dst: (vk::PipelineStageFlags2, vk::AccessFlags2),
    families: (u32, u32),
) {
    let b = [vk::ImageMemoryBarrier2::default()
        .image(image)
        .subresource_range(range(0, 1))
        .old_layout(old)
        .new_layout(new)
        .src_stage_mask(src.0)
        .src_access_mask(src.1)
        .dst_stage_mask(dst.0)
        .dst_access_mask(dst.1)
        .src_queue_family_index(families.0)
        .dst_queue_family_index(families.1)];
    // SAFETY: recording into a command buffer in the recording state.
    unsafe {
        gpu.device.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&b),
        );
    };
}

/// After everything, for a release to another queue family.
pub const BOTTOM: (vk::PipelineStageFlags2, vk::AccessFlags2) = (
    vk::PipelineStageFlags2::BOTTOM_OF_PIPE,
    vk::AccessFlags2::NONE,
);

pub const TOP: (vk::PipelineStageFlags2, vk::AccessFlags2) =
    (vk::PipelineStageFlags2::TOP_OF_PIPE, vk::AccessFlags2::NONE);
pub const TRANSFER_WRITE: (vk::PipelineStageFlags2, vk::AccessFlags2) = (
    vk::PipelineStageFlags2::TRANSFER,
    vk::AccessFlags2::TRANSFER_WRITE,
);
pub const TRANSFER_READ: (vk::PipelineStageFlags2, vk::AccessFlags2) = (
    vk::PipelineStageFlags2::TRANSFER,
    vk::AccessFlags2::TRANSFER_READ,
);
pub const SHADER_READ: (vk::PipelineStageFlags2, vk::AccessFlags2) = (
    vk::PipelineStageFlags2::FRAGMENT_SHADER,
    vk::AccessFlags2::SHADER_SAMPLED_READ,
);
pub const COLOR_WRITE: (vk::PipelineStageFlags2, vk::AccessFlags2) = (
    vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
    vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
);
