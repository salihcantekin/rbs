//! GPU resources of the RBS engine (ADR-0005, spec `docs/specs/gpu-compositor.md`).
//!
//! - [`context`]: device creation and adapter selection by capability.
//! - [`pool`]: reusable textures addressed by small, generation-checked ids.
//! - [`compositor`]: draws a flat list of layers into a linear `Rgba16Float` canvas.
//! - [`yuv`]: converts a canvas to NV12 or P010 (two planes, or one multi-planar texture)
//!   for encoders.
//! - [`yuv_decode`]: decodes NV12 or P010 sources (cameras, native YUV surfaces) to the
//!   canvas format.
//! - [`interop`]: imports native producer surfaces zero-copy as pool textures.
//! - `export` (macOS): converts canvases into IOSurfaces that VideoToolbox encodes
//!   without a copy.
//! - [`share`]: cross-process preview; the engine shares scaled canvases with a UI
//!   process as IOSurfaces (macOS), never as pixels.
//! - [`node`]: the render executor's graph runtime and the compositor node.
//! - [`readback`]: explicit, counted CPU readback (fallback and tests only).
//!
//! `Compositor::render`, `YuvConverter::convert` / `convert_to_surface` and the pool
//! are hot paths (`docs/architecture/hot-paths.md`): RBS code in them does not allocate.
//! `unsafe` appears only in [`pool`] (the lease contract of `rbs_core::media`),
//! [`interop`] (native surface import), `export` and `export_d3d12` (native surface
//! export), [`share`]
//! (cross-process preview) and `exit` (Linux: waits for submitted work before the
//! process exits), all marked low-level modules (P11).

#[cfg(windows)]
pub mod browser_pages;
#[cfg(target_os = "macos")]
pub mod browser_surfaces;
pub mod budget;
pub mod compositor;
pub mod context;
pub mod download;
pub mod error;
#[cfg(target_os = "linux")]
mod exit;
#[cfg(target_vendor = "apple")]
pub mod export;
#[cfg(windows)]
pub mod export_d3d12;
pub mod interop;
pub mod math;
pub mod motion;
pub mod native_export;
pub mod node;
pub mod pool;
pub mod readback;
pub mod rgba;
pub mod scroll;
pub mod share;
pub mod slide;
pub mod upload;
pub mod yuv;
pub mod yuv_decode;

pub use compositor::{
    CanvasDesc, Compositor, Layer, LayerShape, LayerStyle, MAX_SCROLL_LAYERS, RenderStats,
    ScrollBinding, Transfer,
};
pub use context::{GpuContext, GpuMetrics, GpuRequest, PowerPreference};
pub use download::{DownloadShare, DownloadStats, YuvDownload};
pub use error::GpuError;
pub use interop::Importer;
pub use math::{Affine2, UvRect};
pub use motion::{MOTION_GRID_H, MOTION_GRID_W, MOTION_RING, MotionProbe, MotionStats};
pub use node::ScrollSender;
pub use pool::{DEFAULT_POOL_SLOTS, PooledTexture, TextureDesc, TexturePool};
pub use rbs_core::media::TextureId;
pub use rgba::{RgbaDownload, RgbaStats};
pub use scroll::{LayerScroll, ScrollDirection, ScrollGpu, ScrollRepeat, ScrollRun, ScrollStart};
pub use slide::{ReservedTargets, SlideKind, SlidePass};
pub use upload::SourceUpload;
pub use yuv::{ScaleKernel, YuvConverter, YuvFormat};
pub use yuv_decode::{YuvDecoder, YuvSource};

/// The canvas working format: linear light, 16-bit float per channel, premultiplied alpha.
pub const CANVAS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Unit tests that open a device take this turn first: software drivers crash when several
/// devices are created at once (see `exit`'s test).
#[cfg(test)]
pub(crate) static TEST_DEVICE_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
