//! GPU resources of the RBS engine (ADR-0005, spec `docs/specs/gpu-compositor.md`).
//!
//! - [`context`]: device creation and adapter selection by capability.
//! - [`pool`]: reusable textures addressed by small, generation-checked ids.
//! - [`compositor`]: draws a flat list of layers into a linear `Rgba16Float` canvas.
//! - [`nv12`]: converts a canvas to NV12 planes for encoders.
//! - [`interop`]: imports native producer surfaces zero-copy as pool textures.
//! - [`readback`]: explicit, counted CPU readback (fallback and tests only).
//!
//! `Compositor::render`, `Nv12Converter::convert` and the pool are hot paths
//! (`docs/architecture/hot-paths.md`): RBS code in them does not allocate.
//! `unsafe` appears only in [`pool`] (the lease contract of `rbs_core::media`) and
//! [`interop`] (native surface import), both marked low-level modules (P11).

pub mod compositor;
pub mod context;
pub mod error;
pub mod interop;
pub mod math;
pub mod nv12;
pub mod pool;
pub mod readback;

pub use compositor::{CanvasDesc, Compositor, Layer, RenderStats, Transfer};
pub use context::{GpuContext, GpuMetrics, GpuRequest, PowerPreference};
pub use error::GpuError;
pub use interop::Importer;
pub use math::{Affine2, UvRect};
pub use nv12::Nv12Converter;
pub use pool::{DEFAULT_POOL_SLOTS, PooledTexture, TextureDesc, TexturePool};
pub use rbs_core::media::TextureId;

/// The canvas working format: linear light, 16-bit float per channel, premultiplied alpha.
pub const CANVAS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
