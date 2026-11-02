//! GPU resources of the RBS engine (ADR-0005, spec `docs/specs/gpu-compositor.md`).
//!
//! - [`context`]: device creation and adapter selection by capability.
//! - [`pool`]: reusable textures addressed by small, generation-checked ids.
//! - [`compositor`]: draws a flat list of layers into a linear `Rgba16Float` canvas.
//! - [`nv12`]: converts a canvas to NV12 planes for encoders.
//! - [`readback`]: explicit, counted CPU readback (fallback and tests only).
//!
//! `Compositor::render`, `Nv12Converter::convert` and the pool are hot paths
//! (`docs/architecture/hot-paths.md`): RBS code in them does not allocate.
//! This crate contains no `unsafe` code; native texture interop will live in a
//! separate, marked module.

pub mod compositor;
pub mod context;
pub mod error;
pub mod math;
pub mod nv12;
pub mod pool;
pub mod readback;

pub use compositor::{CanvasDesc, Compositor, Layer, RenderStats, Transfer};
pub use context::{GpuContext, GpuMetrics, GpuRequest, PowerPreference};
pub use error::GpuError;
pub use math::{Affine2, UvRect};
pub use nv12::Nv12Converter;
pub use pool::{PooledTexture, TextureDesc, TextureId, TexturePool};

/// The canvas working format: linear light, 16-bit float per channel, premultiplied alpha.
pub const CANVAS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
