//! Shows the Steam Frame decoder's 10-bit pictures, Q10C (Qualcomm's
//! compressed TP10, three 10-bit samples in four bytes), with the stock
//! Turnip driver.
//!
//! Turnip has no TP10 images, but the GPU reads TP10 itself (FMT6_TP10_Y and
//! FMT6_TP10_UV), and Turnip lays out a compressed (UBWC) 32-bit image byte
//! for byte as the decoder lays out a TP10 plane: a TP10 block of 48x4
//! samples is 64x4 bytes, as is a 32-bit block of 16x4 texels. So each plane
//! is bound as a "carrier" image of 32-bit texels, the carrier's texture
//! descriptor is fetched through `VK_EXT_descriptor_buffer`, and its format
//! and width are changed to the TP10 ones (fields from Mesa's a6xx register
//! descriptions). A compute pass copies the samples into P010 planes, which
//! the scene shader samples like any P010 frame, or, for an SDR picture
//! read in the decoder's buffer, the eye passes sample the TP10 descriptors
//! themselves ([`Sampled`], [`crate::tp10_eyes`]). The other carrier formats
//! decompress the same bytes wrongly: the decompression depends on the
//! format.
//!
//! The texture unit reads a TP10 texture wrongly past 2^24 samples
//! (measured on the Frame: a 4096x4096 Y plane and an 8192x4096 picture's UV
//! plane are exact, 5760x3840 and 7680x2880 Y planes are not), so planes are
//! read in bands of whole metadata tiles, each with its own descriptor.
//!
//! [`Converter::new`] checks everything about Turnip this relies on, and
//! each picture's carriers must match the decoder's layout exactly; anything
//! else leaves 10-bit HEVC to software decoding.

use crate::gpu::Gpu;
use crate::mem::{self, Image};
use crate::{Error, Result, VkContext};
use ash::vk;

/// `DRM_FORMAT_MOD_QCOM_COMPRESSED`: Qualcomm's UBWC.
const QCOM_COMPRESSED: u64 = 0x0500_0000_0000_0001;
/// Carrier texels: four bytes, as three TP10 samples.
const CARRIER: vk::Format = vk::Format::A2B10G10R10_UNORM_PACK32;
/// Most samples one TP10 texture reads correctly.
const MAX_BAND_SAMPLES: u64 = 1 << 24;
/// Bands start on metadata tiles: 16 rows of 4-row blocks.
const BAND_ALIGN: u32 = 64;

/// The a6xx/a7xx texture descriptor (Mesa's `A6XX_TEX_MEMOBJ`): (word,
/// shift, bits) of the fields this module reads or changes.
mod tex {
    pub type Field = (usize, u32, u32);
    pub const WORDS: usize = 16;
    pub const TILE_MODE: Field = (0, 0, 2);
    pub const FMT: Field = (0, 22, 8);
    pub const WIDTH: Field = (1, 0, 15);
    pub const HEIGHT: Field = (1, 15, 15);
    pub const FLAG: Field = (3, 28, 1);
    /// In 64-byte units.
    pub const FLAG_BUFFER_PITCH: Field = (10, 0, 7);
    pub const FLAG_BUFFER_LOGH: Field = (10, 12, 4);
    /// Words holding the pixel address (low word, then 17 bits of the next).
    pub const BASE: usize = 4;
    /// Words holding the compression metadata (flag buffer) address.
    pub const FLAGS: usize = 7;

    pub const TILE6_3: u32 = 3;
    pub const FMT6_10_10_10_2_UNORM: u32 = 0x36;
    pub const FMT6_TP10_Y: u32 = 0x9e;
    pub const FMT6_TP10_UV: u32 = 0x9f;

    pub fn get(d: &[u32; WORDS], (word, shift, bits): Field) -> u32 {
        (d[word] >> shift) & ((1 << bits) - 1)
    }

    pub fn set(d: &mut [u32; WORDS], (word, shift, bits): Field, v: u32) {
        let mask = ((1 << bits) - 1) << shift;
        d[word] = (d[word] & !mask) | ((v << shift) & mask);
    }

    pub fn address(d: &[u32; WORDS], word: usize) -> u64 {
        d[word] as u64 | ((d[word + 1] & 0x1_ffff) as u64) << 32
    }

    pub fn set_address(d: &mut [u32; WORDS], word: usize, a: u64) {
        d[word] = a as u32;
        d[word + 1] = (d[word + 1] & !0x1_ffff) | ((a >> 32) as u32 & 0x1_ffff);
    }
}

/// The descriptor of rows `row0..row0 + rows` of a TP10 plane `width`
/// samples (UV pairs for the UV plane) wide, made from the descriptor of
/// its carrier (`texels` x `carrier_rows`). `None` when the carrier's
/// descriptor is not laid out as this expects.
#[allow(clippy::too_many_arguments)]
fn tp10_band(
    carrier: &[u32; tex::WORDS],
    texels: u32,
    carrier_rows: u32,
    fmt: u32,
    width: u32,
    row0: u32,
    rows: u32,
    stride: u64,
) -> Option<[u32; tex::WORDS]> {
    use tex::*;
    let expected = get(carrier, FMT) == FMT6_10_10_10_2_UNORM
        && get(carrier, TILE_MODE) == TILE6_3
        && get(carrier, WIDTH) == texels
        && get(carrier, HEIGHT) == carrier_rows
        && get(carrier, FLAG) == 1
        && get(carrier, FLAG_BUFFER_PITCH) != 0;
    if !expected || !row0.is_multiple_of(BAND_ALIGN) || rows == 0 {
        return None;
    }
    let mut d = *carrier;
    set(&mut d, FMT, fmt);
    set(&mut d, WIDTH, width);
    set(&mut d, HEIGHT, rows);
    // Blocks are 4 rows; the block columns are the same as the carrier's.
    set(
        &mut d,
        FLAG_BUFFER_LOGH,
        rows.div_ceil(4).next_power_of_two().trailing_zeros(),
    );
    let meta_pitch = get(&d, FLAG_BUFFER_PITCH) as u64 * 64;
    let (base, flags) = (address(&d, BASE), address(&d, FLAGS));
    set_address(&mut d, BASE, base + row0 as u64 * stride);
    set_address(&mut d, FLAGS, flags + (row0 / 4) as u64 * meta_pitch);
    Some(d)
}

/// The bands, (first row, rows), that read a plane of `height` rows, each
/// of `per_row` samples.
fn bands(per_row: u32, height: u32) -> Vec<(u32, u32)> {
    let fit = (MAX_BAND_SAMPLES / per_row.max(1) as u64) as u32;
    let step = (fit / BAND_ALIGN * BAND_ALIGN).max(BAND_ALIGN);
    (0..height)
        .step_by(step as usize)
        .map(|r0| (r0, step.min(height - r0)))
        .collect()
}

/// Overlapping bands that let a shader sample a whole plane with bilinear
/// filtering: each band `rows` tall, one starting every `step` rows (half a
/// band), `count` of them, the last ending at the plane's bottom. A lookup
/// reads the band whose middle is nearest, which reaches at least half a
/// step beyond it, so a sample and its neighbours are always in one band.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SampleBands {
    pub step: u32,
    pub rows: u32,
    pub count: u32,
}

/// Most luma and chroma bands the eye passes bind (`planes_tp10.wgsl`).
pub(crate) const MAX_LUMA_BANDS: usize = 8;
pub(crate) const MAX_CHROMA_BANDS: usize = 4;

/// The overlapping bands for a plane of `height` rows of `per_row` samples.
pub(crate) fn sample_bands(per_row: u32, height: u32) -> SampleBands {
    let fit = (MAX_BAND_SAMPLES / per_row.max(1) as u64) as u32;
    let rows = (fit / BAND_ALIGN * BAND_ALIGN).max(2 * BAND_ALIGN);
    if rows >= height {
        return SampleBands {
            step: height.max(1),
            rows: height.max(1),
            count: 1,
        };
    }
    let step = (rows / 2 / BAND_ALIGN * BAND_ALIGN).max(BAND_ALIGN);
    SampleBands {
        step,
        rows,
        count: (height - rows).div_ceil(step) + 1,
    }
}

/// Where a Q10C picture's planes are ([`fp_media::Q10cPicture`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Geometry {
    /// Visible size.
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// Luma rows of the buffer.
    pub rows: u32,
    pub uv_offset: usize,
    pub len: usize,
}

impl Geometry {
    /// The 1920x1080 picture the decoder writes, as measured on the Frame.
    const REFERENCE: Geometry = Geometry {
        width: 1920,
        height: 1080,
        stride: 2560,
        rows: 1088,
        uv_offset: 2_805_760,
        len: 4_210_688,
    };

    /// Carrier sizes in texels: Y, then UV.
    fn carriers(&self) -> [(u32, u32); 2] {
        let texels = self.stride / 4;
        [(texels, self.rows), (texels, self.rows.div_ceil(2))]
    }
}

/// A memory type among `bits` with all of `flags`.
fn memory_type(gpu: &Gpu, bits: u32, flags: vk::MemoryPropertyFlags) -> Option<u32> {
    // SAFETY: valid physical device.
    let props = unsafe { gpu.instance.get_physical_device_memory_properties(gpu.pdev) };
    (0..props.memory_type_count).find(|&i| {
        bits & (1 << i) != 0
            && props.memory_types[i as usize]
                .property_flags
                .contains(flags)
    })
}

const HOST: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::from_raw(
    vk::MemoryPropertyFlags::HOST_VISIBLE.as_raw()
        | vk::MemoryPropertyFlags::HOST_COHERENT.as_raw(),
);

/// A picture's two carrier images, bound to the memory holding it.
struct Carriers {
    images: [vk::Image; 2],
    views: [vk::ImageView; 2],
}

impl Carriers {
    /// Creates the carriers of `g` and checks they need the memory the
    /// decoder's layout has; `bind` then gets their requirements and binds
    /// them (at 0 and the UV offset).
    /// `external`: bound to an imported DMA-BUF (the decoder's buffer).
    fn new(
        gpu: &Gpu,
        g: &Geometry,
        external: bool,
        bind: impl FnOnce(&[vk::MemoryRequirements; 2]) -> Result<vk::DeviceMemory>,
    ) -> Result<Carriers> {
        let mut c = Carriers {
            images: [vk::Image::null(); 2],
            views: [vk::ImageView::null(); 2],
        };
        let r = c.create(gpu, g, external, bind);
        if r.is_err() {
            c.destroy(gpu);
        }
        r.map(|()| c)
    }

    fn create(
        &mut self,
        gpu: &Gpu,
        g: &Geometry,
        external: bool,
        bind: impl FnOnce(&[vk::MemoryRequirements; 2]) -> Result<vk::DeviceMemory>,
    ) -> Result<()> {
        let d = &gpu.device;
        let mut reqs = [vk::MemoryRequirements::default(); 2];
        for (i, (w, h)) in g.carriers().into_iter().enumerate() {
            let layouts = [vk::SubresourceLayout {
                row_pitch: g.stride as u64,
                ..Default::default()
            }];
            let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
                .drm_format_modifier(QCOM_COMPRESSED)
                .plane_layouts(&layouts);
            let mut ext = crate::shared::external_info();
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(CARRIER)
                .extent(vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED)
                // The memory is written by the host, then the layout
                // changes to GENERAL keeping it (see `Converter::record`).
                // Images over imported memory start UNDEFINED, as Vulkan
                // asks, and are read in GENERAL as the decoder left it.
                .initial_layout(if external {
                    vk::ImageLayout::UNDEFINED
                } else {
                    vk::ImageLayout::PREINITIALIZED
                })
                .push_next(&mut explicit);
            let info = if external {
                info.push_next(&mut ext)
            } else {
                info
            };
            // SAFETY: valid create info; images are destroyed by `destroy`.
            unsafe {
                self.images[i] = d.create_image(&info, None).ctx("Q10C carrier")?;
                reqs[i] = d.get_image_memory_requirements(self.images[i]);
            }
        }
        let y_size = g.uv_offset as u64;
        if reqs[0].size != y_size || g.uv_offset as u64 + reqs[1].size > g.len as u64 {
            return Err(Error::Unsupported(format!(
                "Q10C {}x{}: the GPU lays the planes out in {} + {} bytes, the decoder in \
                 {y_size} + {}",
                g.width,
                g.height,
                reqs[0].size,
                reqs[1].size,
                g.len - g.uv_offset
            )));
        }
        if !y_size.is_multiple_of(reqs[1].alignment.max(1)) {
            return Err(Error::Unsupported(
                "Q10C UV plane misaligned for the GPU".into(),
            ));
        }
        let memory = bind(&reqs)?;
        for (i, offset) in [0, y_size].into_iter().enumerate() {
            // SAFETY: memory holds the picture; requirements checked above.
            unsafe {
                d.bind_image_memory(self.images[i], memory, offset)
                    .ctx("bind Q10C carrier")?;
            }
            self.views[i] = mem::view(gpu, self.images[i], CARRIER, 0, 1)?;
        }
        Ok(())
    }

    fn destroy(&mut self, gpu: &Gpu) {
        // SAFETY: not in use (callers wait for the frames that read them).
        unsafe {
            for v in &mut self.views {
                gpu.device.destroy_image_view(std::mem::take(v), None);
            }
            for i in &mut self.images {
                gpu.device.destroy_image(std::mem::take(i), None);
            }
        }
    }
}

/// Plane a band reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plane {
    Y,
    Uv,
}

/// One dispatch: rows `row0..` of a plane, through descriptors at `offset`
/// in the picture's descriptor buffer.
struct Band {
    plane: Plane,
    row0: u32,
    rows: u32,
    offset: u64,
}

/// One video image set's Q10C resources: the memory the picture is copied
/// into, its carriers, and a descriptor buffer with each band's descriptors.
pub(crate) struct Picture {
    pub geometry: Geometry,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
    carriers: Carriers,
    descriptors: vk::Buffer,
    descriptor_memory: vk::DeviceMemory,
    descriptor_address: vk::DeviceAddress,
    bands: Vec<Band>,
    /// The carriers' layout is GENERAL (after the first conversion).
    ready: bool,
    /// The memory is the decoder's buffer, imported ([`Converter::import`]):
    /// nothing is copied in, and each conversion acquires the carriers from
    /// the decoder's queue family and releases them to it after.
    imported: bool,
}

// SAFETY: `mapped` points into `memory`, which only this picture uses; the
// renderer drives it from one thread at a time.
unsafe impl Send for Picture {}

impl Picture {
    /// The memory the decoder's buffer goes into (nothing for an imported
    /// picture, which is the decoder's buffer).
    pub fn bytes(&mut self) -> &mut [u8] {
        if self.mapped.is_null() {
            return &mut [];
        }
        // SAFETY: `mapped` is the persistent mapping of `geometry.len` bytes.
        unsafe { std::slice::from_raw_parts_mut(self.mapped, self.geometry.len) }
    }

    pub fn destroy(&mut self, gpu: &Gpu) {
        self.carriers.destroy(gpu);
        // SAFETY: not in use; null handles are ignored.
        unsafe {
            gpu.device
                .destroy_buffer(std::mem::take(&mut self.descriptors), None);
            gpu.device
                .free_memory(std::mem::take(&mut self.descriptor_memory), None);
            gpu.device
                .free_memory(std::mem::take(&mut self.memory), None);
        }
    }
}

/// A Q10C picture in the decoder's buffer, sampled there by the eye passes
/// (`planes_tp10.wgsl`) instead of being converted: its carriers and the
/// TP10 descriptors of its overlapping bands.
pub(crate) struct Sampled {
    memory: vk::DeviceMemory,
    carriers: Carriers,
    pub luma: Vec<[u32; tex::WORDS]>,
    pub chroma: Vec<[u32; tex::WORDS]>,
    pub luma_bands: SampleBands,
    pub chroma_bands: SampleBands,
    /// Acquired from the decoder's queue family and not handed back yet.
    held: bool,
}

impl Sampled {
    /// Records the move of the carriers from the decoder to the renderer,
    /// for the eye passes to sample.
    pub fn acquire(&mut self, gpu: &Gpu, cmd: vk::CommandBuffer) {
        // A frame that took it over without drawing hands it back first.
        self.release(gpu, cmd);
        for &image in &self.carriers.images {
            mem::ownership_barrier(
                gpu,
                cmd,
                image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::GENERAL,
                mem::TOP,
                mem::SHADER_READ,
                (gpu.decoder_family, gpu.queue_family),
            );
        }
        self.held = true;
    }

    /// Records the move back to the decoder once the eye passes have read
    /// the picture, if the renderer holds it.
    pub fn release(&mut self, gpu: &Gpu, cmd: vk::CommandBuffer) {
        if !std::mem::take(&mut self.held) {
            return;
        }
        for &image in &self.carriers.images {
            mem::ownership_barrier(
                gpu,
                cmd,
                image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::GENERAL,
                mem::SHADER_READ,
                mem::BOTTOM,
                (gpu.queue_family, gpu.decoder_family),
            );
        }
    }

    pub fn destroy(&mut self, gpu: &Gpu) {
        self.carriers.destroy(gpu);
        // SAFETY: not in use; null is ignored.
        unsafe {
            gpu.device
                .free_memory(std::mem::take(&mut self.memory), None);
        }
    }
}

/// The conversion pipelines, made once per renderer.
pub(crate) struct Converter {
    db: ash::ext::descriptor_buffer::Device,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    luma: vk::Pipeline,
    chroma: vk::Pipeline,
    /// Bytes of one band's descriptors, aligned for binding.
    set_size: u64,
    /// Where bindings 0 (TP10), 1 (Y out) and 2 (UV out) are in a band's.
    binding_offsets: [usize; 3],
    sampled_size: usize,
    storage_size: usize,
}

impl Converter {
    /// The converter, when this GPU and driver can show Q10C pictures.
    pub fn new(gpu: &Gpu) -> Option<Converter> {
        if !gpu.q10c_features {
            return None;
        }
        let mut c = match Converter::create(gpu) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("10-bit HEVC stays in software: {e}");
                return None;
            }
        };
        // Lay out the measured 1080p picture and read its descriptors: the
        // driver must agree with the decoder and with `tp10_band`.
        if let Err(e) = c.check(gpu) {
            log::warn!("10-bit HEVC stays in software: {e}");
            c.destroy(gpu);
            return None;
        }
        log::info!("10-bit HEVC: hardware decoding, Q10C shown through TP10 descriptors");
        Some(c)
    }

    fn create(gpu: &Gpu) -> Result<Converter> {
        let i = &gpu.instance;
        // SAFETY: format queries on a valid physical device.
        unsafe {
            let mut count = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut fp = vk::FormatProperties2::default().push_next(&mut count);
            i.get_physical_device_format_properties2(gpu.pdev, CARRIER, &mut fp);
            let mut mods = vec![
                vk::DrmFormatModifierPropertiesEXT::default();
                count.drm_format_modifier_count as usize
            ];
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
                .drm_format_modifier_properties(&mut mods);
            let mut fp = vk::FormatProperties2::default().push_next(&mut list);
            i.get_physical_device_format_properties2(gpu.pdev, CARRIER, &mut fp);
            let compressed = mods.iter().any(|m| {
                m.drm_format_modifier == QCOM_COMPRESSED
                    && m.drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
            });
            if !compressed {
                return Err(Error::Unsupported(
                    "no compressed (UBWC) 32-bit images to carry TP10".into(),
                ));
            }
            let out = vk::FormatFeatureFlags::STORAGE_IMAGE | vk::FormatFeatureFlags::SAMPLED_IMAGE;
            for f in [vk::Format::R16_UNORM, vk::Format::R16G16_UNORM] {
                if !i
                    .get_physical_device_format_properties(gpu.pdev, f)
                    .optimal_tiling_features
                    .contains(out)
                {
                    return Err(Error::Unsupported(format!("{f:?} storage images")));
                }
            }
        }
        let mut dbp = vk::PhysicalDeviceDescriptorBufferPropertiesEXT::default();
        let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut dbp);
        // SAFETY: valid physical device.
        unsafe { i.get_physical_device_properties2(gpu.pdev, &mut p2) };
        if dbp.sampled_image_descriptor_size != tex::WORDS * 4 {
            return Err(Error::Unsupported(format!(
                "{}-byte texture descriptors",
                dbp.sampled_image_descriptor_size
            )));
        }
        let db = ash::ext::descriptor_buffer::Device::new(i, &gpu.device);
        let d = &gpu.device;
        let bindings = [
            (0, vk::DescriptorType::SAMPLED_IMAGE),
            (1, vk::DescriptorType::STORAGE_IMAGE),
            (2, vk::DescriptorType::STORAGE_IMAGE),
        ]
        .map(|(b, t)| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(t)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        });
        // SAFETY: plain object creation on a valid device; everything made
        // here is destroyed by `destroy`.
        unsafe {
            let set_layout = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default()
                        .flags(vk::DescriptorSetLayoutCreateFlags::DESCRIPTOR_BUFFER_EXT)
                        .bindings(&bindings),
                    None,
                )
                .ctx("Q10C descriptor set layout")?;
            let sets = [set_layout];
            let ranges = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .size(4)];
            let layout = d
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&sets)
                        .push_constant_ranges(&ranges),
                    None,
                )
                .ctx("Q10C pipeline layout")?;
            let mut c = Converter {
                set_size: db
                    .get_descriptor_set_layout_size(set_layout)
                    .next_multiple_of(dbp.descriptor_buffer_offset_alignment.max(1)),
                binding_offsets: [0, 1, 2]
                    .map(|b| db.get_descriptor_set_layout_binding_offset(set_layout, b) as usize),
                db,
                set_layout,
                layout,
                luma: vk::Pipeline::null(),
                chroma: vk::Pipeline::null(),
                sampled_size: dbp.sampled_image_descriptor_size,
                storage_size: dbp.storage_image_descriptor_size,
            };
            let module = match gpu.shader(crate::Q10C_SPV) {
                Ok(m) => m,
                Err(e) => {
                    c.destroy(gpu);
                    return Err(e);
                }
            };
            let infos = [c"luma", c"chroma"].map(|name| {
                vk::ComputePipelineCreateInfo::default()
                    .flags(vk::PipelineCreateFlags::DESCRIPTOR_BUFFER_EXT)
                    .stage(
                        vk::PipelineShaderStageCreateInfo::default()
                            .stage(vk::ShaderStageFlags::COMPUTE)
                            .module(module)
                            .name(name),
                    )
                    .layout(layout)
            });
            let pipes = d.create_compute_pipelines(vk::PipelineCache::null(), &infos, None);
            d.destroy_shader_module(module, None);
            match pipes {
                Ok(p) => (c.luma, c.chroma) = (p[0], p[1]),
                Err((p, e)) => {
                    for p in p {
                        d.destroy_pipeline(p, None);
                    }
                    c.destroy(gpu);
                    return Err(Error::Vk {
                        context: "Q10C pipelines",
                        result: e,
                    });
                }
            }
            Ok(c)
        }
    }

    /// Lays out the reference picture and checks its descriptors.
    fn check(&self, gpu: &Gpu) -> Result<()> {
        let g = Geometry::REFERENCE;
        let mut memory = vk::DeviceMemory::null();
        let carriers = Carriers::new(gpu, &g, false, |reqs| {
            memory = allocate(
                gpu,
                g.len as u64,
                reqs[0].memory_type_bits & reqs[1].memory_type_bits,
                false,
            )?;
            Ok(memory)
        });
        let result = carriers.and_then(|mut carriers| {
            let r = [Plane::Y, Plane::Uv]
                .into_iter()
                .try_for_each(|plane| self.band(&g, &carriers, plane, 0, 64).map(drop));
            carriers.destroy(gpu);
            r
        });
        // SAFETY: nothing uses the memory any more (null is ignored).
        unsafe { gpu.device.free_memory(memory, None) };
        result
    }

    /// The descriptor of one band of a plane.
    fn band(
        &self,
        g: &Geometry,
        carriers: &Carriers,
        plane: Plane,
        row0: u32,
        rows: u32,
    ) -> Result<[u32; tex::WORDS]> {
        let i = plane as usize;
        let mut carrier = [0u32; tex::WORDS];
        let info = vk::DescriptorImageInfo::default()
            .image_view(carriers.views[i])
            .image_layout(vk::ImageLayout::GENERAL);
        let get = vk::DescriptorGetInfoEXT::default()
            .ty(vk::DescriptorType::SAMPLED_IMAGE)
            .data(vk::DescriptorDataEXT {
                p_sampled_image: &info,
            });
        // SAFETY: the view is valid and the destination holds a sampled
        // image descriptor (its size checked in `create`).
        unsafe {
            self.db
                .get_descriptor(&get, bytemuck::cast_slice_mut(&mut carrier));
        }
        let (texels, carrier_rows) = g.carriers()[i];
        let (fmt, width) = match plane {
            Plane::Y => (tex::FMT6_TP10_Y, g.width),
            Plane::Uv => (tex::FMT6_TP10_UV, g.width.div_ceil(2)),
        };
        tp10_band(
            &carrier,
            texels,
            carrier_rows,
            fmt,
            width,
            row0,
            rows,
            g.stride as u64,
        )
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "unexpected texture descriptor for a compressed 32-bit image: {carrier:08x?}"
            ))
        })
    }

    /// The resources to convert pictures of geometry `g` into `outputs`
    /// (the Y and UV planes, R16 and R16G16).
    pub fn picture(&self, gpu: &Gpu, g: Geometry, outputs: &[Image]) -> Result<Picture> {
        self.make(gpu, g, outputs, None)
    }

    /// A picture whose memory is the decoder's buffer `fd` (a DMA-BUF of
    /// `g.len` bytes), converted into `outputs` where it lies.
    pub fn import(&self, gpu: &Gpu, g: Geometry, fd: i32, outputs: &[Image]) -> Result<Picture> {
        self.make(gpu, g, outputs, Some(fd))
    }

    /// The picture in the decoder's buffer `fd` (a DMA-BUF of `g.len`
    /// bytes) as the eye passes sample it, without a conversion.
    pub fn sample(&self, gpu: &Gpu, g: Geometry, fd: i32) -> Result<Sampled> {
        let (cw, ch) = (g.width.div_ceil(2), g.height.div_ceil(2));
        let luma_bands = sample_bands(g.width, g.height);
        let chroma_bands = sample_bands(cw * 2, ch);
        if luma_bands.count as usize > MAX_LUMA_BANDS
            || chroma_bands.count as usize > MAX_CHROMA_BANDS
        {
            return Err(Error::Unsupported(format!(
                "Q10C {}x{} needs more bands than the eye passes read",
                g.width, g.height
            )));
        }
        let mut memory = vk::DeviceMemory::null();
        let carriers = Carriers::new(gpu, &g, true, |reqs| {
            let bits = reqs[0].memory_type_bits & reqs[1].memory_type_bits;
            memory = crate::shared::import(gpu, fd, g.len as u64, bits, None)?;
            Ok(memory)
        });
        let mut s = Sampled {
            memory,
            carriers: match carriers {
                Ok(c) => c,
                Err(e) => {
                    // SAFETY: unused (null is ignored).
                    unsafe { gpu.device.free_memory(memory, None) };
                    return Err(e);
                }
            },
            luma: Vec::new(),
            chroma: Vec::new(),
            luma_bands,
            chroma_bands,
            held: false,
        };
        let made = [
            (Plane::Y, luma_bands, g.height),
            (Plane::Uv, chroma_bands, ch),
        ]
        .into_iter()
        .try_for_each(|(plane, b, height)| {
            for k in 0..b.count {
                let row0 = k * b.step;
                let d = self.band(&g, &s.carriers, plane, row0, b.rows.min(height - row0))?;
                match plane {
                    Plane::Y => s.luma.push(d),
                    Plane::Uv => s.chroma.push(d),
                }
            }
            Ok(())
        });
        if let Err(e) = made {
            s.destroy(gpu);
            return Err(e);
        }
        Ok(s)
    }

    fn make(&self, gpu: &Gpu, g: Geometry, outputs: &[Image], fd: Option<i32>) -> Result<Picture> {
        let mut memory = vk::DeviceMemory::null();
        let carriers = Carriers::new(gpu, &g, fd.is_some(), |reqs| {
            let bits = reqs[0].memory_type_bits & reqs[1].memory_type_bits;
            memory = match fd {
                Some(fd) => crate::shared::import(gpu, fd, g.len as u64, bits, None)?,
                None => allocate(gpu, g.len as u64, bits, false)?,
            };
            Ok(memory)
        });
        let mut p = Picture {
            geometry: g,
            memory,
            mapped: std::ptr::null_mut(),
            carriers: match carriers {
                Ok(c) => c,
                Err(e) => {
                    // SAFETY: unused (null is ignored).
                    unsafe { gpu.device.free_memory(memory, None) };
                    return Err(e);
                }
            },
            descriptors: vk::Buffer::null(),
            descriptor_memory: vk::DeviceMemory::null(),
            descriptor_address: 0,
            bands: Vec::new(),
            ready: false,
            imported: fd.is_some(),
        };
        if let Err(e) = self.fill(gpu, &mut p, outputs) {
            p.destroy(gpu);
            return Err(e);
        }
        Ok(p)
    }

    fn fill(&self, gpu: &Gpu, p: &mut Picture, outputs: &[Image]) -> Result<()> {
        let d = &gpu.device;
        let g = p.geometry;
        if !p.imported {
            // SAFETY: host-visible memory of g.len bytes, mapped for the
            // picture's lifetime (freeing it unmaps).
            p.mapped = unsafe {
                d.map_memory(p.memory, 0, g.len as u64, vk::MemoryMapFlags::empty())
                    .ctx("map Q10C memory")? as *mut u8
            };
        }
        let (cw, ch) = (g.width.div_ceil(2), g.height.div_ceil(2));
        let mut offset = 0;
        for (plane, per_row, height) in [(Plane::Y, g.width, g.height), (Plane::Uv, cw * 2, ch)] {
            for (row0, rows) in bands(per_row, height) {
                p.bands.push(Band {
                    plane,
                    row0,
                    rows,
                    offset,
                });
                offset += self.set_size;
            }
        }
        // SAFETY: a buffer for the bands' descriptors in host-visible
        // memory with a device address, owned by the picture.
        unsafe {
            p.descriptors = d
                .create_buffer(
                    &vk::BufferCreateInfo::default().size(offset).usage(
                        vk::BufferUsageFlags::RESOURCE_DESCRIPTOR_BUFFER_EXT
                            | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS,
                    ),
                    None,
                )
                .ctx("Q10C descriptor buffer")?;
            let req = d.get_buffer_memory_requirements(p.descriptors);
            p.descriptor_memory = allocate(gpu, req.size, req.memory_type_bits, true)?;
            d.bind_buffer_memory(p.descriptors, p.descriptor_memory, 0)
                .ctx("bind Q10C descriptor buffer")?;
            p.descriptor_address = d.get_buffer_device_address(
                &vk::BufferDeviceAddressInfo::default().buffer(p.descriptors),
            );
            let base = d
                .map_memory(p.descriptor_memory, 0, offset, vk::MemoryMapFlags::empty())
                .ctx("map Q10C descriptors")? as *mut u8;
            let all = std::slice::from_raw_parts_mut(base, offset as usize);
            let written = p.bands.iter().try_for_each(|b| {
                let set = &mut all[b.offset as usize..(b.offset + self.set_size) as usize];
                let tp10 = self.band(&g, &p.carriers, b.plane, b.row0, b.rows)?;
                let at = self.binding_offsets[0];
                set[at..at + self.sampled_size].copy_from_slice(bytemuck::cast_slice(&tp10));
                for (i, out) in outputs.iter().enumerate().take(2) {
                    let info = vk::DescriptorImageInfo::default()
                        .image_view(out.views[0])
                        .image_layout(vk::ImageLayout::GENERAL);
                    let get = vk::DescriptorGetInfoEXT::default()
                        .ty(vk::DescriptorType::STORAGE_IMAGE)
                        .data(vk::DescriptorDataEXT {
                            p_storage_image: &info,
                        });
                    let at = self.binding_offsets[1 + i];
                    self.db
                        .get_descriptor(&get, &mut set[at..at + self.storage_size]);
                }
                Ok(())
            });
            d.unmap_memory(p.descriptor_memory);
            written
        }
    }

    /// Records the conversion of the picture just copied into `p` into
    /// `outputs`, leaving them ready for the scene shader.
    pub fn record(&self, gpu: &Gpu, cmd: vk::CommandBuffer, p: &mut Picture, outputs: &[Image]) {
        let d = &gpu.device;
        let compute_write = (
            vk::PipelineStageFlags2::COMPUTE_SHADER,
            vk::AccessFlags2::SHADER_STORAGE_WRITE,
        );
        let compute_read = (
            vk::PipelineStageFlags2::COMPUTE_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );
        for o in outputs {
            mem::barrier(
                gpu,
                cmd,
                o.image,
                mem::range(0, 1),
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::GENERAL,
                mem::TOP,
                compute_write,
            );
        }
        if p.imported {
            // From the decoder, as it wrote the buffer: compressed data
            // read in GENERAL, as on the Frame since the first conversion.
            for &image in &p.carriers.images {
                mem::ownership_barrier(
                    gpu,
                    cmd,
                    image,
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::GENERAL,
                    mem::TOP,
                    compute_read,
                    (gpu.decoder_family, gpu.queue_family),
                );
            }
        } else if !p.ready {
            for &image in &p.carriers.images {
                mem::barrier(
                    gpu,
                    cmd,
                    image,
                    mem::range(0, 1),
                    vk::ImageLayout::PREINITIALIZED,
                    vk::ImageLayout::GENERAL,
                    mem::TOP,
                    compute_read,
                );
            }
            p.ready = true;
        }
        let binding = [vk::DescriptorBufferBindingInfoEXT::default()
            .address(p.descriptor_address)
            .usage(vk::BufferUsageFlags::RESOURCE_DESCRIPTOR_BUFFER_EXT)];
        // SAFETY: recording; the pipelines, layout and descriptor buffer are
        // valid, and each band's descriptors cover its dispatch.
        unsafe {
            self.db.cmd_bind_descriptor_buffers(cmd, &binding);
            for b in &p.bands {
                let (pipe, width) = match b.plane {
                    Plane::Y => (self.luma, p.geometry.width),
                    Plane::Uv => (self.chroma, p.geometry.width.div_ceil(2)),
                };
                d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipe);
                self.db.cmd_set_descriptor_buffer_offsets(
                    cmd,
                    vk::PipelineBindPoint::COMPUTE,
                    self.layout,
                    0,
                    &[0],
                    &[b.offset],
                );
                d.cmd_push_constants(
                    cmd,
                    self.layout,
                    vk::ShaderStageFlags::COMPUTE,
                    0,
                    &b.row0.to_ne_bytes(),
                );
                d.cmd_dispatch(cmd, width.div_ceil(8), b.rows.div_ceil(8), 1);
            }
        }
        if p.imported {
            // Back to the decoder once read.
            for &image in &p.carriers.images {
                mem::ownership_barrier(
                    gpu,
                    cmd,
                    image,
                    vk::ImageLayout::GENERAL,
                    vk::ImageLayout::GENERAL,
                    compute_read,
                    mem::BOTTOM,
                    (gpu.queue_family, gpu.decoder_family),
                );
            }
        }
        for o in outputs {
            mem::barrier(
                gpu,
                cmd,
                o.image,
                mem::range(0, 1),
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                compute_write,
                mem::SHADER_READ,
            );
        }
    }

    pub fn destroy(&mut self, gpu: &Gpu) {
        // SAFETY: not in use (the renderer waits for its frames first);
        // null handles are ignored.
        unsafe {
            let d = &gpu.device;
            d.destroy_pipeline(std::mem::take(&mut self.luma), None);
            d.destroy_pipeline(std::mem::take(&mut self.chroma), None);
            d.destroy_pipeline_layout(std::mem::take(&mut self.layout), None);
            d.destroy_descriptor_set_layout(std::mem::take(&mut self.set_layout), None);
        }
    }
}

/// `size` bytes of host-visible, coherent memory of a type in `bits`, with
/// a device address when `address`.
pub(crate) fn allocate(gpu: &Gpu, size: u64, bits: u32, address: bool) -> Result<vk::DeviceMemory> {
    let index = memory_type(gpu, bits, HOST)
        .ok_or_else(|| Error::Unsupported("no host-visible memory for Q10C".into()))?;
    let mut flags =
        vk::MemoryAllocateFlagsInfo::default().flags(vk::MemoryAllocateFlags::DEVICE_ADDRESS);
    let mut info = vk::MemoryAllocateInfo::default()
        .allocation_size(size)
        .memory_type_index(index);
    if address {
        info = info.push_next(&mut flags);
    }
    // SAFETY: valid allocate info; the caller frees the memory.
    unsafe { gpu.device.allocate_memory(&info, None) }.ctx("Q10C memory")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Turnip's descriptor of a 640x1080 carrier at 0x1_0002_6000, as read
    /// on the Frame (Mesa 26.3).
    const CARRIER_1080: [u32; 16] = [
        0x0d80_6883,
        0x021c_0280,
        0x2005_0002,
        0x1000_02a8,
        0x0002_b000,
        0x0002_0001,
        0,
        0x0002_6000,
        0x0000_0001,
        0x0000_0140,
        0x0000_9601,
        0,
        0,
        0,
        0,
        0,
    ];

    #[test]
    fn carrier_descriptors_become_tp10_bands() {
        let whole = tp10_band(
            &CARRIER_1080,
            640,
            1080,
            tex::FMT6_TP10_Y,
            1920,
            0,
            1080,
            2560,
        )
        .unwrap();
        // What the exact 1080p read used: only format and width change.
        assert_eq!(whole[0], 0x2780_6883);
        assert_eq!(whole[1], 0x021c_0780);
        assert_eq!(whole[2..], CARRIER_1080[2..]);

        let band = tp10_band(
            &CARRIER_1080,
            640,
            1080,
            tex::FMT6_TP10_Y,
            1920,
            512,
            256,
            2560,
        )
        .unwrap();
        assert_eq!(tex::get(&band, tex::HEIGHT), 256);
        assert_eq!(tex::get(&band, tex::FLAG_BUFFER_LOGH), 6);
        // 512 rows of 2560 bytes on, and 128 metadata rows of 64 bytes.
        assert_eq!(tex::address(&band, tex::BASE), 0x1_0002_b000 + 512 * 2560);
        assert_eq!(tex::address(&band, tex::FLAGS), 0x1_0002_6000 + 128 * 64);
        assert_eq!(band[5] >> 17, CARRIER_1080[5] >> 17, "DEPTH kept");

        // Anything but the carrier this was measured with is refused.
        assert!(
            tp10_band(
                &CARRIER_1080,
                641,
                1080,
                tex::FMT6_TP10_Y,
                1920,
                0,
                1080,
                2560
            )
            .is_none()
        );
        let mut other = CARRIER_1080;
        tex::set(&mut other, tex::FMT, 0x30);
        assert!(tp10_band(&other, 640, 1080, tex::FMT6_TP10_Y, 1920, 0, 1080, 2560).is_none());
        assert!(
            tp10_band(
                &CARRIER_1080,
                640,
                1080,
                tex::FMT6_TP10_Y,
                1920,
                32,
                64,
                2560
            )
            .is_none()
        );
    }

    #[test]
    fn bands_stay_under_the_sample_limit() {
        // Measured exact: one band up to 2^24 samples.
        assert_eq!(bands(1920, 1080), [(0, 1080)]);
        assert_eq!(bands(4096, 4096), [(0, 4096)]);
        assert_eq!(bands(8192, 2048), [(0, 2048)], "8K UV, 4096 pairs a row");
        // Measured exact in these bands.
        assert_eq!(bands(8192, 4096), [(0, 2048), (2048, 2048)]);
        assert_eq!(bands(7680, 3840), [(0, 2176), (2176, 1664)]);
        assert_eq!(bands(5760, 3840), [(0, 2880), (2880, 960)]);
        for (w, h) in [(8192, 8192), (7680, 3840), (5000, 3001)] {
            let b = bands(w, h);
            assert_eq!(b.iter().map(|&(_, r)| r).sum::<u32>(), h);
            for &(r0, rows) in &b {
                assert!(r0.is_multiple_of(BAND_ALIGN));
                assert!(w as u64 * rows as u64 <= MAX_BAND_SAMPLES);
            }
        }
    }

    #[test]
    fn a_sample_and_its_neighbours_are_always_in_the_band_the_shader_picks() {
        // Luma and chroma (samples a row = UV pairs x 2) of the largest and
        // some odd pictures the decoder gives.
        for (per_row, height, most) in [
            (8192, 8192, MAX_LUMA_BANDS),
            (8192, 4320, MAX_LUMA_BANDS),
            (7680, 3840, MAX_LUMA_BANDS),
            (5760, 5760, MAX_LUMA_BANDS),
            (1920, 1080, MAX_LUMA_BANDS),
            (8192, 4096, MAX_CHROMA_BANDS),
            (8192, 2160, MAX_CHROMA_BANDS),
            (7680, 1920, MAX_CHROMA_BANDS),
        ] {
            let b = sample_bands(per_row, height);
            assert!(b.count as usize <= most, "{per_row}x{height}: {b:?}");
            assert!(b.step.is_multiple_of(BAND_ALIGN) || b.count == 1, "{b:?}");
            assert!(per_row as u64 * b.rows as u64 <= MAX_BAND_SAMPLES || b.count == 1);
            for row in 0..height {
                // As planes_tp10.wgsl picks: the band whose middle is nearest.
                let y = row as f32 + 0.5;
                let k = ((y - 0.5 * b.rows as f32) / b.step as f32)
                    .round()
                    .clamp(0.0, (b.count - 1) as f32) as u32;
                let row0 = k * b.step;
                let end = (row0 + b.rows).min(height);
                // Bilinear reads the next row; sharpening one more each way.
                let need = (row.saturating_sub(2), (row + 2).min(height - 1));
                assert!(
                    row0 <= need.0 && need.1 < end,
                    "{per_row}x{height} row {row}: band {k} is {row0}..{end}"
                );
            }
        }
        assert_eq!(sample_bands(1920, 1080).count, 1);
        assert_eq!(sample_bands(7680, 3840).count, 3);
    }
}
