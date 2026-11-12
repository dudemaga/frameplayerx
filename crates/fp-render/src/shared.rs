//! Reads the hardware decoder's buffers where they lie. Each buffer comes as
//! a DMA-BUF ([`fp_media::SharedPicture`]) and is imported as device memory:
//! an NV12 picture gets one image per plane over it, which the scene shader
//! samples, and a Q10C one is converted by [`crate::q10c`]. Imports are kept
//! by buffer, so later frames the decoder writes into the same buffer cost
//! nothing to set up, until the decoder's set of buffers changes or the
//! buffer goes unused for a while.

use crate::gpu::Gpu;
use crate::mem;
use crate::q10c::{Picture, Sampled};
use crate::{Error, Result, VkContext};
use ash::vk;
use fp_media::SharedPicture;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::Hash;
use std::os::fd::{AsRawFd, BorrowedFd, IntoRawFd};

/// The kind of external memory the decoder's buffers are.
const DMA_BUF: vk::ExternalMemoryHandleTypeFlags = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;

/// `DRM_FORMAT_MOD_LINEAR`: plain rows, one after the other, as the decoder
/// writes NV12.
const LINEAR: u64 = 0;

/// The layout the eye passes sample NV12 planes in (the one the scene
/// descriptor set names).
const SAMPLED: vk::ImageLayout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;

/// How long an import may go unused before a sweep takes it, in frames the
/// renderer shows (calls to [`Imports::sweep`]): five seconds at the
/// Frame's 90 Hz.
///
/// While a video plays, the decoder cycles through a handful of buffers (8
/// on the Frame), so each one comes back after a few video frames: a third
/// of a second at 24 fps, still under two seconds at a quarter of normal
/// speed. Five seconds also outlasts a seek or a short stall. A buffer that
/// stays away longer most likely belongs to a decoder that frames no longer
/// come from (the next video's are copied, or decoded in software), and a
/// new decoder needs the memory it holds. Counting shown frames instead of
/// time keeps an import used in every frame, as a paused video's is, however
/// long rendering stops in between (the headset taken off).
const UNUSED_FRAMES: u64 = 450;

/// An image set of the renderer, as its index and its generation. A Q10C
/// import converts into one generation of one set only.
pub(crate) type Target = (usize, u64);

/// The extension struct that marks an image as backed by DMA-BUF memory, to
/// chain to its `vk::ImageCreateInfo`.
pub(crate) fn external_info() -> vk::ExternalMemoryImageCreateInfo<'static> {
    vk::ExternalMemoryImageCreateInfo::default().handle_types(DMA_BUF)
}

/// The `VK_KHR_external_memory_fd` loader, or why there is none.
fn loader(gpu: &Gpu) -> Result<&ash::khr::external_memory_fd::Device> {
    gpu.memory_fd.as_ref().ok_or_else(|| {
        Error::Unsupported("this GPU can't read the video decoder's buffers in place".into())
    })
}

/// What `image` needs of its memory, and whether the driver prefers or
/// requires that memory to be the image's alone.
fn requirements(gpu: &Gpu, image: vk::Image) -> (vk::MemoryRequirements, bool) {
    let mut dedicated = vk::MemoryDedicatedRequirements::default();
    let mut reqs = vk::MemoryRequirements2::default().push_next(&mut dedicated);
    // SAFETY: valid image; the output chain holds initialised structs.
    unsafe {
        gpu.device.get_image_memory_requirements2(
            &vk::ImageMemoryRequirementsInfo2::default().image(image),
            &mut reqs,
        );
    }
    let reqs = reqs.memory_requirements;
    let alone = dedicated.prefers_dedicated_allocation == vk::TRUE
        || dedicated.requires_dedicated_allocation == vk::TRUE;
    (reqs, alone)
}

/// Lets the GPU reach the decoder's buffer behind `fd` without a copy: the
/// returned memory object spans the buffer's first `len` bytes and is the
/// caller's to free. Its type is the lowest-numbered one that both `bits`
/// (from whatever will be bound to it) and the driver's report on this
/// descriptor permit.
///
/// `fd` stays the caller's, open: the import works on a duplicate, which
/// Vulkan keeps on success and which is closed again on failure.
///
/// Passing `image` lets the driver ask for the memory to serve that image
/// alone. When it prefers or requires that, the allocation is dedicated to
/// the image and sized to what the image needs, which may not exceed `len`.
///
/// Fails with [`Error::Unsupported`] when this GPU has no DMA-BUF import or
/// no memory type fits.
pub(crate) fn import(
    gpu: &Gpu,
    fd: i32,
    len: u64,
    bits: u32,
    image: Option<vk::Image>,
) -> Result<vk::DeviceMemory> {
    let loader = loader(gpu)?;
    if fd < 0 || len == 0 {
        return Err(Error::Unsupported(format!(
            "no video decoder buffer to import (descriptor {fd}, {len} bytes)"
        )));
    }
    let mut fd_props = vk::MemoryFdPropertiesKHR::default();
    // SAFETY: `fd` is the caller's open DMA-BUF; the query doesn't take it.
    unsafe { loader.get_memory_fd_properties(DMA_BUF, fd, &mut fd_props) }
        .ctx("vkGetMemoryFdPropertiesKHR")?;
    let usable = bits & fd_props.memory_type_bits;
    if usable == 0 {
        return Err(Error::Unsupported(format!(
            "the GPU has no memory type that can hold this picture (types {bits:#b}) and \
             import the video decoder's buffer (types {:#b})",
            fd_props.memory_type_bits
        )));
    }
    // Vulkan orders memory types so that the first one that fits is the
    // one to use.
    let index = usable.trailing_zeros();
    let dedicated = image.and_then(|image| {
        let (reqs, alone) = requirements(gpu, image);
        alone.then_some((image, reqs.size))
    });
    let size = match dedicated {
        Some((_, size)) if size > len => {
            return Err(Error::Unsupported(format!(
                "the GPU needs {size} bytes for this picture, the video decoder's buffer \
                 has {len}"
            )));
        }
        Some((_, size)) => size,
        None => len,
    };
    // A successful import takes over the descriptor it is given, while the
    // caller keeps its own: Vulkan gets a copy, closed on drop if the
    // import fails.
    // SAFETY: `fd` is open (the query above accepted it) and stays so for
    // this call.
    let copy = unsafe { BorrowedFd::borrow_raw(fd) }
        .try_clone_to_owned()
        .map_err(|e| {
            Error::Unsupported(format!(
                "copying the video decoder's buffer descriptor: {e}"
            ))
        })?;
    let mut from_fd = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(DMA_BUF)
        .fd(copy.as_raw_fd());
    let mut alone = vk::MemoryDedicatedAllocateInfo::default();
    let mut info = vk::MemoryAllocateInfo::default()
        .allocation_size(size)
        .memory_type_index(index)
        .push_next(&mut from_fd);
    if let Some((image, _)) = dedicated {
        alone = alone.image(image);
        info = info.push_next(&mut alone);
    }
    // SAFETY: valid allocate info: the type is one the buffer allows and the
    // size fits in it; the caller frees the memory.
    let memory = unsafe { gpu.device.allocate_memory(&info, None) }
        .ctx("import the video decoder's buffer")?;
    // The driver owns the copy now and closes it with the memory.
    let _ = copy.into_raw_fd();
    Ok(memory)
}

/// One plane of an NV12 buffer, as an image is made of it.
struct Plane {
    /// For messages.
    name: &'static str,
    format: vk::Format,
    /// In texels (a chroma pair is one).
    extent: (u32, u32),
    /// Where its first row starts in the buffer.
    offset: u64,
}

/// Checks the GPU can sample `plane` as an image over linear DMA-BUF memory
/// of the decoder's, which Vulkan asks to know before such an image is made.
fn check_format(gpu: &Gpu, plane: &Plane) -> Result<()> {
    let format = plane.format;
    let mut modifier = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(LINEAR)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(DMA_BUF);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::SAMPLED)
        .push_next(&mut modifier)
        .push_next(&mut external);
    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    // SAFETY: valid physical device; both chains hold initialised structs.
    unsafe {
        gpu.instance
            .get_physical_device_image_format_properties2(gpu.pdev, &info, &mut props)
    }
    .map_err(|result| match result {
        vk::Result::ERROR_FORMAT_NOT_SUPPORTED => Error::Unsupported(format!(
            "the GPU can't sample {format:?} images in the video decoder's buffers"
        )),
        result => Error::Vk {
            context: "vkGetPhysicalDeviceImageFormatProperties2",
            result,
        },
    })?;
    let max = props.image_format_properties.max_extent;
    let features = external_props
        .external_memory_properties
        .external_memory_features;
    if !features.contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE) {
        return Err(Error::Unsupported(format!(
            "the GPU can't import the video decoder's buffers as {format:?} images"
        )));
    }
    let (width, height) = plane.extent;
    if width > max.width || height > max.height {
        return Err(Error::Unsupported(format!(
            "the GPU samples {format:?} images in the video decoder's buffers up to {}x{}, \
             not {width}x{height}",
            max.width, max.height
        )));
    }
    Ok(())
}

/// An NV12 picture sampled in the decoder's buffer: an image per plane, laid
/// out as the decoder wrote it, each bound to its own import of the buffer
/// (a driver may want an import to belong to a single image).
pub(crate) struct Nv12 {
    /// Luma, then chroma.
    images: [vk::Image; 2],
    views: [vk::ImageView; 2],
    memory: [vk::DeviceMemory; 2],
    /// The layout the last use left the images in: `UNDEFINED` until the
    /// first acquire.
    layout: vk::ImageLayout,
    /// Acquired from the decoder's queue family and not handed back yet.
    held: bool,
}

impl Nv12 {
    /// The planes of the NV12 picture in `p`, whose visible size is `size`.
    /// Whatever was made is destroyed if a step fails.
    fn new(gpu: &Gpu, p: &SharedPicture, (width, height): (u32, u32)) -> Result<Nv12> {
        // Images of DMA-BUF memory are only valid with the import extensions.
        loader(gpu)?;
        if width == 0 || height == 0 {
            return Err(Error::Unsupported(
                "the video decoder's picture is empty".into(),
            ));
        }
        let planes = [
            Plane {
                name: "luma",
                format: vk::Format::R8_UNORM,
                extent: (width, height),
                offset: 0,
            },
            Plane {
                name: "chroma",
                format: vk::Format::R8G8_UNORM,
                extent: (width.div_ceil(2), height.div_ceil(2)),
                offset: p.uv_offset as u64,
            },
        ];
        let mut n = Nv12 {
            images: Default::default(),
            views: Default::default(),
            memory: Default::default(),
            layout: vk::ImageLayout::UNDEFINED,
            held: false,
        };
        for (i, plane) in planes.iter().enumerate() {
            if let Err(e) = n.make_plane(gpu, p, i, plane) {
                n.destroy(gpu);
                return Err(e);
            }
        }
        Ok(n)
    }

    /// Makes plane `i`: its image, its import and its view, each stored as
    /// soon as it exists so that [`Nv12::destroy`] finds it.
    fn make_plane(&mut self, gpu: &Gpu, p: &SharedPicture, i: usize, plane: &Plane) -> Result<()> {
        check_format(gpu, plane)?;
        let d = &gpu.device;
        let (width, height) = plane.extent;
        // The decoder's layout, not one the driver picks: the plane's own
        // offset and row pitch, with the image bound at the buffer's start.
        let layouts = [vk::SubresourceLayout {
            offset: plane.offset,
            row_pitch: p.stride as u64,
            ..Default::default()
        }];
        let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(LINEAR)
            .plane_layouts(&layouts);
        let mut external = external_info();
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(plane.format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            // Vulkan's rule for images over imported memory; the first
            // acquire moves the images on from there.
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut explicit)
            .push_next(&mut external);
        // SAFETY: valid create info, the format checked above; `destroy`
        // destroys the image.
        let image = unsafe { d.create_image(&info, None) }.ctx("NV12 plane image")?;
        self.images[i] = image;
        let (reqs, _) = requirements(gpu, image);
        if reqs.size > p.len as u64 {
            return Err(Error::Unsupported(format!(
                "NV12 {} plane: the GPU needs {} bytes, the video decoder's buffer has {}",
                plane.name, reqs.size, p.len
            )));
        }
        let memory = import(gpu, p.fd, p.len as u64, reqs.memory_type_bits, Some(image))?;
        self.memory[i] = memory;
        // SAFETY: the memory starts at the buffer's start, where the layout
        // above counts the plane's offset from, and holds what the image
        // needs (checked above).
        unsafe { d.bind_image_memory(image, memory, 0) }.ctx("bind NV12 plane")?;
        self.views[i] = mem::view(gpu, image, plane.format, 0, 1)?;
        Ok(())
    }

    /// Records the move of both planes from the decoder's queue family to
    /// the renderer's, ready for the fragment shader.
    fn acquire(&mut self, gpu: &Gpu, cmd: vk::CommandBuffer) {
        // Still held when a frame acquired it but another took its place
        // before the eye passes, so nothing handed it back: hand it back
        // first, so that this acquire is the one that makes what the
        // decoder has written since visible.
        self.release(gpu, cmd);
        for &image in &self.images {
            mem::ownership_barrier(
                gpu,
                cmd,
                image,
                self.layout,
                SAMPLED,
                mem::TOP,
                mem::SHADER_READ,
                (gpu.decoder_family, gpu.queue_family),
            );
        }
        self.layout = SAMPLED;
        self.held = true;
    }

    /// Records the move of both planes back to the decoder's queue family
    /// once the frame has read them, if the renderer holds them.
    fn release(&mut self, gpu: &Gpu, cmd: vk::CommandBuffer) {
        if !self.held {
            return;
        }
        // The layout stays as it is: the decoder ignores it, and a linear
        // image's bytes don't depend on it.
        for &image in &self.images {
            mem::ownership_barrier(
                gpu,
                cmd,
                image,
                self.layout,
                self.layout,
                mem::SHADER_READ,
                mem::BOTTOM,
                (gpu.queue_family, gpu.decoder_family),
            );
        }
        self.held = false;
    }

    /// Destroys the views, images and imports; null handles (a picture
    /// given up half made) are allowed.
    fn destroy(&mut self, gpu: &Gpu) {
        // SAFETY: no submitted frame reads them any more (imports are
        // destroyed with the renderer's garbage, or when the device is
        // idle); null handles are ignored.
        unsafe {
            for v in &mut self.views {
                gpu.device.destroy_image_view(std::mem::take(v), None);
            }
            for i in &mut self.images {
                gpu.device.destroy_image(std::mem::take(i), None);
            }
            for m in &mut self.memory {
                gpu.device.free_memory(std::mem::take(m), None);
            }
        }
    }
}

/// An import taken out of [`Imports`], to destroy once no submitted frame
/// can read it any more.
pub(crate) enum Imported {
    /// An NV12 picture's planes.
    Nv12(Nv12),
    /// A Q10C picture, imported to be converted into one image set.
    Q10c(Picture),
    /// A Q10C picture, imported for the eye passes to sample.
    Tp10(Sampled),
}

impl Imported {
    /// Frees what the import holds, giving the buffer's memory back.
    pub fn destroy(&mut self, gpu: &Gpu) {
        match self {
            Imported::Nv12(p) => p.destroy(gpu),
            Imported::Q10c(p) => p.destroy(gpu),
            Imported::Tp10(p) => p.destroy(gpu),
        }
    }
}

/// An import on a [`Shelf`], with what sweeping it depends on.
struct Stored<T> {
    import: T,
    /// The decoder's set of buffers ([`SharedPicture::set`]).
    set: u64,
    /// The frame it was last used in ([`Imports::frame`]).
    used: u64,
}

/// Imports of one kind by key. Kept apart from [`Imports`] so that the rules
/// for sweeping them can be tested without a GPU.
struct Shelf<K, T>(HashMap<K, Stored<T>>);

impl<K, T> Default for Shelf<K, T> {
    fn default() -> Self {
        Shelf(HashMap::new())
    }
}

impl<K: Eq + Hash, T> Shelf<K, T> {
    /// The import under `key`, made by `make` (for buffer set `set`) when
    /// there is none, and marked as used in `frame`. Nothing is stored when
    /// `make` fails.
    fn get_or_make(
        &mut self,
        key: K,
        set: u64,
        frame: u64,
        make: impl FnOnce() -> Result<T>,
    ) -> Result<&mut T> {
        let stored = match self.0.entry(key) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(Stored {
                import: make()?,
                set,
                used: frame,
            }),
        };
        stored.used = frame;
        Ok(&mut stored.import)
    }

    /// The import under `key`, without counting this as a use.
    fn get(&self, key: &K) -> Option<&T> {
        self.0.get(key).map(|s| &s.import)
    }

    /// As [`Shelf::get`], to change it.
    fn get_mut(&mut self, key: &K) -> Option<&mut T> {
        self.0.get_mut(key).map(|s| &mut s.import)
    }

    /// Takes out the imports `stale` picks. Only as many as the returned
    /// iterator yields are removed, so it has to be run to the end.
    fn take(&mut self, stale: impl FnMut(&K, &mut Stored<T>) -> bool) -> impl Iterator<Item = T> {
        self.0.extract_if(stale).map(|(_, s)| s.import)
    }

    /// Takes out, in `frame`, the imports of a buffer set other than `keep`
    /// (all of them when `None`) and those unused for more than
    /// [`UNUSED_FRAMES`].
    fn sweep(&mut self, keep: Option<u64>, frame: u64) -> impl Iterator<Item = T> {
        self.take(move |_, s| keep != Some(s.set) || frame.saturating_sub(s.used) > UNUSED_FRAMES)
    }

    /// Takes out every import.
    fn drain(&mut self) -> impl Iterator<Item = T> {
        self.0.drain().map(|(_, s)| s.import)
    }
}

impl<T> Shelf<(u64, Target), T> {
    /// Takes out the imports made for image set `index` in a generation
    /// other than `generation`.
    fn sweep_target(&mut self, (index, generation): Target) -> impl Iterator<Item = T> {
        self.take(move |&(_, (i, g)), _| i == index && g != generation)
    }
}

/// Every decoder buffer imported and not swept out yet: NV12 pictures by
/// buffer ([`SharedPicture::buffer`]), Q10C ones by buffer and the image set
/// they convert into. Nothing is destroyed when an import is swept out: it
/// goes to the caller's garbage, which the caller destroys once no
/// submitted frame can read it.
#[derive(Default)]
pub(crate) struct Imports {
    nv12: Shelf<u64, Nv12>,
    q10c: Shelf<(u64, Target), Picture>,
    tp10: Shelf<u64, Sampled>,
    /// Frames shown so far: one per call to [`Imports::sweep`], which the
    /// renderer makes once for each frame. [`UNUSED_FRAMES`] counts on it.
    frame: u64,
}

impl Imports {
    /// Makes the NV12 picture in `p` (visible size `size`) readable by the
    /// frame `cmd` records: imports its buffer the first time it is seen,
    /// then records the planes' move from the decoder to the renderer, ready
    /// for the fragment shader. [`Imports::release_nv12`] hands them back.
    pub fn nv12(
        &mut self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        p: &SharedPicture,
        size: (u32, u32),
    ) -> Result<()> {
        let picture = self
            .nv12
            .get_or_make(p.buffer, p.set, self.frame, || Nv12::new(gpu, p, size))?;
        picture.acquire(gpu, cmd);
        Ok(())
    }

    /// The luma (R8) and chroma (R8G8) views of the NV12 picture in
    /// `buffer`, if it is imported.
    pub fn nv12_views(&self, buffer: u64) -> Option<[vk::ImageView; 2]> {
        self.nv12.get(&buffer).map(|p| p.views)
    }

    /// Records the move of the NV12 picture in `buffer` back to the decoder,
    /// after the frame's eye passes in `cmd`. Nothing if it isn't held.
    pub fn release_nv12(&mut self, gpu: &Gpu, cmd: vk::CommandBuffer, buffer: u64) {
        if let Some(p) = self.nv12.get_mut(&buffer) {
            p.release(gpu, cmd);
        }
    }

    /// The Q10C picture in `p` as the eye passes sample it, made by `import`
    /// the first time this buffer is seen, and moved from the decoder to the
    /// renderer for the frame `cmd` records. [`Imports::release_tp10`] hands
    /// it back.
    pub fn tp10(
        &mut self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        p: &SharedPicture,
        import: impl FnOnce() -> Result<Sampled>,
    ) -> Result<&Sampled> {
        let picture = self.tp10.get_or_make(p.buffer, p.set, self.frame, import)?;
        picture.acquire(gpu, cmd);
        Ok(picture)
    }

    /// The sampled Q10C picture in `buffer`, if it is imported.
    pub fn tp10_picture(&self, buffer: u64) -> Option<&Sampled> {
        self.tp10.get(&buffer)
    }

    /// Records the move of the Q10C picture in `buffer` back to the decoder,
    /// after the frame's eye passes in `cmd`. Nothing if it isn't held.
    pub fn release_tp10(&mut self, gpu: &Gpu, cmd: vk::CommandBuffer, buffer: u64) {
        if let Some(p) = self.tp10.get_mut(&buffer) {
            p.release(gpu, cmd);
        }
    }

    /// The import of the Q10C picture in `p` for image set `target`, made by
    /// `import` the first time this buffer meets this set.
    pub fn q10c(
        &mut self,
        p: &SharedPicture,
        target: Target,
        import: impl FnOnce() -> Result<Picture>,
    ) -> Result<&mut Picture> {
        self.q10c
            .get_or_make((p.buffer, target), p.set, self.frame, import)
    }

    /// Starts a frame and moves to `garbage` the imports of a buffer set
    /// other than `keep` (all of them when `None`), and those unused for
    /// more than [`UNUSED_FRAMES`]. The renderer calls this once per frame.
    pub fn sweep(&mut self, keep: Option<u64>, garbage: &mut Vec<Imported>) {
        self.frame += 1;
        garbage.extend(self.nv12.sweep(keep, self.frame).map(Imported::Nv12));
        garbage.extend(self.q10c.sweep(keep, self.frame).map(Imported::Q10c));
        garbage.extend(self.tp10.sweep(keep, self.frame).map(Imported::Tp10));
    }

    /// Moves to `garbage` the Q10C imports made for the image set of
    /// `target` in an older generation: they convert into images that are
    /// gone.
    pub fn sweep_target(&mut self, target: Target, garbage: &mut Vec<Imported>) {
        garbage.extend(self.q10c.sweep_target(target).map(Imported::Q10c));
    }

    /// Destroys every import at once. Only while the device is idle.
    pub fn destroy(&mut self, gpu: &Gpu) {
        for mut p in self.nv12.drain() {
            p.destroy(gpu);
        }
        for mut p in self.q10c.drain() {
            p.destroy(gpu);
        }
        for mut p in self.tp10.drain() {
            p.destroy(gpu);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What came out of a shelf, in order.
    fn sorted<T: Ord>(taken: impl Iterator<Item = T>) -> Vec<T> {
        let mut v: Vec<T> = taken.collect();
        v.sort();
        v
    }

    /// Uses the import `key` of buffer set `set` in `frame`, making it
    /// `name` if it isn't there.
    fn use_in(
        shelf: &mut Shelf<u64, &'static str>,
        key: u64,
        set: u64,
        frame: u64,
        name: &'static str,
    ) {
        shelf.get_or_make(key, set, frame, || Ok(name)).unwrap();
    }

    #[test]
    fn imports_of_another_buffer_set_go_at_once() {
        let mut shelf = Shelf::default();
        use_in(&mut shelf, 1, 10, 1, "old video");
        use_in(&mut shelf, 2, 11, 1, "new video");
        assert_eq!(sorted(shelf.sweep(Some(11), 2)), ["old video"]);
        assert_eq!(shelf.get(&2), Some(&"new video"));
        assert_eq!(sorted(shelf.sweep(None, 3)), ["new video"]);
        assert!(shelf.0.is_empty());
    }

    #[test]
    fn an_import_used_in_every_frame_never_expires() {
        let mut shelf = Shelf::default();
        for frame in 1..=10 * UNUSED_FRAMES {
            assert_eq!(shelf.sweep(Some(7), frame).count(), 0, "frame {frame}");
            use_in(&mut shelf, 1, 7, frame, "paused");
        }
        assert_eq!(shelf.get(&1), Some(&"paused"));
    }

    #[test]
    fn an_unused_import_expires_after_the_limit() {
        let mut shelf = Shelf::default();
        use_in(&mut shelf, 1, 7, 0, "shown");
        use_in(&mut shelf, 2, 7, 0, "unused");
        for frame in 1..=UNUSED_FRAMES {
            assert_eq!(shelf.sweep(Some(7), frame).count(), 0, "frame {frame}");
            use_in(&mut shelf, 1, 7, frame, "shown");
        }
        assert_eq!(sorted(shelf.sweep(Some(7), UNUSED_FRAMES + 1)), ["unused"]);
        assert_eq!(shelf.get(&1), Some(&"shown"));
    }

    #[test]
    fn a_buffer_is_imported_once_and_a_failed_import_leaves_nothing() {
        let mut shelf: Shelf<u64, &str> = Shelf::default();
        let failed = shelf.get_or_make(1, 7, 1, || Err(Error::Unsupported("no".into())));
        assert!(failed.is_err());
        assert!(shelf.get(&1).is_none());
        use_in(&mut shelf, 1, 7, 2, "first");
        let mut made_again = false;
        let again = shelf
            .get_or_make(1, 7, 3, || {
                made_again = true;
                Ok("second")
            })
            .unwrap();
        assert_eq!(*again, "first");
        assert!(!made_again);
    }

    #[test]
    fn a_new_generation_sweeps_the_older_imports_of_its_set_only() {
        let mut shelf: Shelf<(u64, Target), &str> = Shelf::default();
        for (key, name) in [
            ((1, (0, 1)), "set 0, old"),
            ((1, (1, 1)), "set 1"),
            ((2, (0, 2)), "set 0, current"),
        ] {
            shelf.get_or_make(key, 7, 1, || Ok(name)).unwrap();
        }
        assert_eq!(sorted(shelf.sweep_target((0, 2))), ["set 0, old"]);
        assert_eq!(shelf.0.len(), 2);
    }
}
