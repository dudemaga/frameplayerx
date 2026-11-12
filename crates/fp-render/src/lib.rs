//! FramePlayerX's Vulkan renderer.
//!
//! - [`gpu`]: instance/device creation, directly (tests, desktop preview) or
//!   through OpenXR's `XR_KHR_vulkan_enable2`.
//! - [`renderer`]: per-frame recording: video upload, UI panels painted with
//!   egui, and the two eye passes (projection shader, then world-space quads).
//! - [`params`]: turns a video format, view settings and colour info into the
//!   projection shader's uniform block.
//! - [`math`]: OpenXR-style projection and view matrices.
//! - [`capture`]: offscreen eye targets with read-back, for tests and
//!   screenshots.
//! - `q10c`: shows the Steam Frame decoder's 10-bit (Q10C) pictures.
//! - [`hdr`]: HDR on the SDR screen: tone curve, gamut compression and the
//!   scene peak, which `peak` measures from the frames.
//! - [`backdrop`]: finds the green or blue screen behind a video in a
//!   picture of one of its frames, the colour its chroma key starts with.

pub mod backdrop;
pub mod capture;
mod foveation;
pub mod gpu;
pub mod hdr;
pub mod math;
pub mod mem;
pub mod params;
mod peak;
mod pipeline;
mod q10c;
pub mod renderer;
mod shared;
mod timing;
mod tp10_eyes;
mod video;

pub use foveation::Foveation;
pub use gpu::{Creator, DirectCreator, Gpu};
pub use params::VideoParams;
pub use renderer::{EyeTarget, EyeView, PanelId, QuadDraw, QuadTexture, Renderer};
pub use timing::GpuTimes;

use ash::vk;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{context}: {result:?}")]
    Vk {
        context: &'static str,
        result: vk::Result,
    },
    #[error("loading Vulkan: {0}")]
    Load(String),
    #[error("memory allocation: {0}")]
    Alloc(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) trait VkContext<T> {
    fn ctx(self, context: &'static str) -> Result<T>;
}

impl<T> VkContext<T> for std::result::Result<T, vk::Result> {
    fn ctx(self, context: &'static str) -> Result<T> {
        self.map_err(|result| Error::Vk { context, result })
    }
}

pub(crate) const SCENE_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/scene.spv"));
pub(crate) const QUAD_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/quad.spv"));
pub(crate) const EGUI_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/egui.spv"));
pub(crate) const Q10C_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/q10c.spv"));
pub(crate) const PEAK_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/peak.spv"));
pub(crate) const SCENE_TP10_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/scene_tp10.spv"));
