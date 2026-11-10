//! Vulkan instance and device.

use crate::{Error, Result, VkContext};
use ash::vk;
use fp_core::sync::lock;
use gpu_allocator::vulkan::{Allocator, AllocatorCreateDesc};
use std::ffi::CString;
use std::mem::ManuallyDrop;
use std::sync::Mutex;

/// FramePlayerX needs Vulkan 1.3 (dynamic rendering, synchronization2).
pub const API_VERSION: u32 = vk::API_VERSION_1_3;

/// Creates the instance and device. OpenXR runtimes need to do this
/// themselves (XR_KHR_vulkan_enable2), so creation is pluggable.
pub trait Creator {
    fn create_instance(
        &self,
        entry: &ash::Entry,
        info: &vk::InstanceCreateInfo,
    ) -> Result<vk::Instance>;
    fn physical_device(&self, instance: &ash::Instance) -> Result<vk::PhysicalDevice>;
    fn create_device(
        &self,
        instance: &ash::Instance,
        pdev: vk::PhysicalDevice,
        info: &vk::DeviceCreateInfo,
    ) -> Result<vk::Device>;
}

/// Plain Vulkan: first device that supports Vulkan 1.3 with a graphics queue.
pub struct DirectCreator;

impl Creator for DirectCreator {
    fn create_instance(
        &self,
        entry: &ash::Entry,
        info: &vk::InstanceCreateInfo,
    ) -> Result<vk::Instance> {
        // SAFETY: valid create info.
        unsafe { entry.create_instance(info, None) }
            .map(|i| i.handle())
            .ctx("vkCreateInstance")
    }
    fn physical_device(&self, instance: &ash::Instance) -> Result<vk::PhysicalDevice> {
        // SAFETY: valid instance.
        let devices = unsafe { instance.enumerate_physical_devices() }.ctx("enumerate devices")?;
        devices
            .into_iter()
            .find(|&d| {
                // SAFETY: valid physical device.
                let p = unsafe { instance.get_physical_device_properties(d) };
                p.api_version >= API_VERSION && graphics_queue(instance, d).is_some()
            })
            .ok_or_else(|| Error::Unsupported("no Vulkan 1.3 device with a graphics queue".into()))
    }
    fn create_device(
        &self,
        instance: &ash::Instance,
        pdev: vk::PhysicalDevice,
        info: &vk::DeviceCreateInfo,
    ) -> Result<vk::Device> {
        // SAFETY: valid create info for this physical device.
        unsafe { instance.create_device(pdev, info, None) }
            .map(|d| d.handle())
            .ctx("vkCreateDevice")
    }
}

pub(crate) fn graphics_queue(instance: &ash::Instance, pdev: vk::PhysicalDevice) -> Option<u32> {
    // SAFETY: valid physical device.
    unsafe { instance.get_physical_device_queue_family_properties(pdev) }
        .iter()
        .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .map(|i| i as u32)
}

pub struct Gpu {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub pdev: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub queue_family: u32,
    pub allocator: ManuallyDrop<Mutex<Allocator>>,
    pub device_name: String,
    pub driver: String,
    /// Turnip with what [`crate::q10c`] needs enabled: descriptor buffers,
    /// buffer device addresses, DRM format modifiers and 16-bit storage
    /// images.
    pub(crate) q10c_features: bool,
    /// Imports DMA-BUFs ([`crate::shared`]): the video decoder's buffers
    /// are read in place. With [`Gpu::q10c_features`] only.
    pub(crate) memory_fd: Option<ash::khr::external_memory_fd::Device>,
    /// The queue family owning memory the video decoder writes: foreign
    /// when the driver has `VK_EXT_queue_family_foreign`, else external.
    pub(crate) decoder_family: u32,
    /// Fragment shading rate attachments, for foveated eye passes
    /// ([`crate::foveation`]).
    pub(crate) shading_rates: Option<ShadingRateCaps>,
}

/// What [`crate::foveation`] needs to know of the GPU's shading rates.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ShadingRateCaps {
    /// The pixels one texel of a shading rate attachment covers.
    pub texel: vk::Extent2D,
}

/// Whether `pdev` takes a fragment shading rate attachment, and how.
fn shading_rate_caps(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
) -> Option<ShadingRateCaps> {
    if !has_extension(instance, pdev, ash::khr::fragment_shading_rate::NAME) {
        return None;
    }
    let mut features = vk::PhysicalDeviceFragmentShadingRateFeaturesKHR::default();
    let mut f2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut features);
    let mut props = vk::PhysicalDeviceFragmentShadingRatePropertiesKHR::default();
    let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut props);
    // SAFETY: valid physical device; the chains hold initialised structs.
    unsafe {
        instance.get_physical_device_features2(pdev, &mut f2);
        instance.get_physical_device_properties2(pdev, &mut p2);
    }
    (features.attachment_fragment_shading_rate == vk::TRUE).then_some(ShadingRateCaps {
        texel: props.min_fragment_shading_rate_attachment_texel_size,
    })
}

/// Device extensions [`crate::q10c`] needs.
const Q10C_EXTENSIONS: [&std::ffi::CStr; 2] = [
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::descriptor_buffer::NAME,
];

/// Device extensions importing the decoder's DMA-BUFs needs, besides
/// [`Q10C_EXTENSIONS`].
const SHARED_EXTENSIONS: [&std::ffi::CStr; 2] = [
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
];

/// Whether `pdev` has device extension `name`.
fn has_extension(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    name: &std::ffi::CStr,
) -> bool {
    // SAFETY: valid physical device.
    unsafe { instance.enumerate_device_extension_properties(pdev) }
        .unwrap_or_default()
        .iter()
        .any(|e| e.extension_name_as_c_str() == Ok(name))
}

/// Whether `pdev` is Turnip with everything [`crate::q10c`] needs.
fn q10c_supported(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    driver: vk::DriverId,
) -> bool {
    if driver != vk::DriverId::MESA_TURNIP {
        return false;
    }
    // SAFETY: valid physical device.
    let exts = unsafe { instance.enumerate_device_extension_properties(pdev) }.unwrap_or_default();
    let has = |n: &std::ffi::CStr| exts.iter().any(|e| e.extension_name_as_c_str() == Ok(n));
    if !Q10C_EXTENSIONS.iter().all(|n| has(n)) {
        return false;
    }
    let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut fdb = vk::PhysicalDeviceDescriptorBufferFeaturesEXT::default();
    let mut f2 = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut f12)
        .push_next(&mut fdb);
    // SAFETY: valid physical device; the chain holds initialised structs.
    unsafe { instance.get_physical_device_features2(pdev, &mut f2) };
    let storage_formats = f2.features.shader_storage_image_extended_formats;
    f12.buffer_device_address == vk::TRUE
        && fdb.descriptor_buffer == vk::TRUE
        && storage_formats == vk::TRUE
}

impl Gpu {
    pub fn new(creator: &dyn Creator, app_name: &str) -> Result<Gpu> {
        // SAFETY: loads the system Vulkan loader.
        let entry = unsafe { ash::Entry::load() }.map_err(|e| Error::Load(e.to_string()))?;
        let name = CString::new(app_name).unwrap_or_default();
        let app = vk::ApplicationInfo::default()
            .application_name(&name)
            .engine_name(&name)
            .api_version(API_VERSION);
        let ici = vk::InstanceCreateInfo::default().application_info(&app);
        let raw = creator.create_instance(&entry, &ici)?;
        // SAFETY: raw is a live instance created with this entry.
        let instance = unsafe { ash::Instance::load(entry.static_fn(), raw) };
        let pdev = creator.physical_device(&instance)?;
        let mut driver_props = vk::PhysicalDeviceDriverProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver_props);
        // SAFETY: valid physical device.
        unsafe { instance.get_physical_device_properties2(pdev, &mut props2) };
        let props = props2.properties;
        if props.api_version < API_VERSION {
            return Err(Error::Unsupported("GPU does not support Vulkan 1.3".into()));
        }
        let device_name = props
            .device_name_as_c_str()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        let driver = format!(
            "{} {}",
            driver_props
                .driver_name_as_c_str()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default(),
            driver_props
                .driver_info_as_c_str()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        let queue_family = graphics_queue(&instance, pdev)
            .ok_or_else(|| Error::Unsupported("no graphics queue".into()))?;
        let prio = [1.0f32];
        let qci = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&prio)];
        let mut f13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true);
        let q10c_features = q10c_supported(&instance, pdev, driver_props.driver_id);
        let shared = q10c_features
            && SHARED_EXTENSIONS
                .iter()
                .all(|n| has_extension(&instance, pdev, n));
        let foreign =
            shared && has_extension(&instance, pdev, ash::ext::queue_family_foreign::NAME);
        let shading_rates = shading_rate_caps(&instance, pdev);
        let mut exts: Vec<*const std::ffi::c_char> = Vec::new();
        if q10c_features {
            exts.extend(Q10C_EXTENSIONS.map(std::ffi::CStr::as_ptr));
        }
        if shared {
            exts.extend(SHARED_EXTENSIONS.map(std::ffi::CStr::as_ptr));
        }
        if foreign {
            exts.push(ash::ext::queue_family_foreign::NAME.as_ptr());
        }
        if shading_rates.is_some() {
            exts.push(ash::khr::fragment_shading_rate::NAME.as_ptr());
        }
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default().buffer_device_address(true);
        let mut fdb =
            vk::PhysicalDeviceDescriptorBufferFeaturesEXT::default().descriptor_buffer(true);
        let mut fsr = vk::PhysicalDeviceFragmentShadingRateFeaturesKHR::default()
            .attachment_fragment_shading_rate(true);
        let features =
            vk::PhysicalDeviceFeatures::default().shader_storage_image_extended_formats(true);
        let mut dci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&qci)
            .enabled_extension_names(&exts)
            .push_next(&mut f13);
        if q10c_features {
            dci = dci
                .enabled_features(&features)
                .push_next(&mut f12)
                .push_next(&mut fdb);
        }
        if shading_rates.is_some() {
            dci = dci.push_next(&mut fsr);
        }
        let raw_dev = creator.create_device(&instance, pdev, &dci)?;
        // SAFETY: raw_dev was created from this instance.
        let device = unsafe { ash::Device::load(instance.fp_v1_0(), raw_dev) };
        // SAFETY: queue 0 of the family was requested.
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let allocator = Allocator::new(&AllocatorCreateDesc {
            instance: instance.clone(),
            device: device.clone(),
            physical_device: pdev,
            debug_settings: Default::default(),
            buffer_device_address: false,
            allocation_sizes: Default::default(),
        })
        .map_err(|e| Error::Alloc(e.to_string()))?;
        log::info!("GPU: {device_name} ({driver})");
        let memory_fd =
            shared.then(|| ash::khr::external_memory_fd::Device::new(&instance, &device));
        if shared {
            log::info!("GPU: imports the video decoder's buffers (zero copy)");
        }
        Ok(Gpu {
            entry,
            instance,
            pdev,
            device,
            queue,
            queue_family,
            allocator: ManuallyDrop::new(Mutex::new(allocator)),
            device_name,
            driver,
            q10c_features,
            memory_fd,
            decoder_family: if foreign {
                vk::QUEUE_FAMILY_FOREIGN_EXT
            } else {
                vk::QUEUE_FAMILY_EXTERNAL
            },
            shading_rates,
        })
    }

    /// Headless device for tests and tools.
    pub fn headless() -> Result<Gpu> {
        Gpu::new(&DirectCreator, "frameplayerx")
    }

    pub(crate) fn alloc(&self) -> std::sync::MutexGuard<'_, Allocator> {
        lock(&self.allocator)
    }

    pub fn wait_idle(&self) {
        // SAFETY: valid device.
        let _ = unsafe { self.device.device_wait_idle() };
    }

    pub(crate) fn shader(&self, spv: &[u8]) -> Result<vk::ShaderModule> {
        let words: Vec<u32> = spv
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        // SAFETY: valid SPIR-V produced by naga at build time.
        unsafe {
            self.device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
        }
        .ctx("create shader")
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        self.wait_idle();
        // SAFETY: all resources were destroyed by their owners; the
        // allocator must go before the device, the device before the instance.
        unsafe {
            ManuallyDrop::drop(&mut self.allocator);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
