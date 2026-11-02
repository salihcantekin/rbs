//! Native surface import (spec `docs/specs/gpu-compositor.md#native-import-interop`).
//!
//! This is a marked low-level module (principle P11): backend submodules wrap producer
//! surfaces through wgpu's HAL with `unsafe` calls, each with a `// SAFETY:` comment.
//!
//! [`Importer::import`] takes a native [`VideoFrame`] by value, aliases its surface as a
//! pool texture (opened once per surface and cached), makes the queue wait on the
//! producer's fence, and returns a `Texture` frame. [`Importer::retire`] hands the
//! producer's lease back once no frame references the slot and the GPU has finished
//! reading it.

use rbs_core::media::{FramePayload, NativeSurface, VideoFrame};

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::TexturePool;

#[cfg(windows)]
mod d3d12;

/// Most producer surfaces one importer keeps open at a time.
pub const MAX_SURFACES: usize = 16;
/// Ticks after which a cached surface that was not seen again is closed.
pub const SURFACE_IDLE_TICKS: u64 = 120;

/// Imports native frames for one render device. Owned by the render thread.
///
/// Holds no GPU objects until the first import (P13).
#[derive(Default)]
pub struct Importer {
    #[cfg(windows)]
    d3d12: d3d12::D3d12Importer,
}

impl Importer {
    /// An importer with nothing open.
    pub fn new() -> Self {
        Self::default()
    }

    /// Turns a native frame into a `Texture` frame on the same format and time.
    ///
    /// Frames that are not native are returned unchanged. On error the frame (and with it
    /// the producer's lease) is dropped, which hands the surface back to the producer.
    pub fn import(
        &mut self,
        ctx: &GpuContext,
        pool: &mut TexturePool,
        frame: VideoFrame,
    ) -> Result<VideoFrame, GpuError> {
        let FramePayload::Native { surface, .. } = frame.payload() else {
            return Ok(frame);
        };
        match surface {
            #[cfg(windows)]
            NativeSurface::D3dSharedHandle { .. } => self.d3d12.import(ctx, pool, frame),
            #[cfg(not(windows))]
            NativeSurface::D3dSharedHandle { .. } => {
                let _ = (ctx, pool);
                Err(GpuError::Unsupported(
                    "Direct3D shared handles need Windows",
                ))
            }
            NativeSurface::D3d11Texture { .. } => Err(GpuError::Unsupported(
                "D3D11 texture pointers; share the texture with an NT handle",
            )),
            _ => Err(GpuError::Unsupported("surface kind not implemented yet")),
        }
    }

    /// Hands producer leases back when their surfaces are no longer read, and closes
    /// surfaces not seen for [`SURFACE_IDLE_TICKS`]. Call once per render tick.
    pub fn retire(&mut self, ctx: &GpuContext, pool: &mut TexturePool) {
        #[cfg(windows)]
        self.d3d12.retire(ctx, pool);
        #[cfg(not(windows))]
        let _ = (ctx, pool);
    }

    /// Producer surfaces currently open.
    pub fn open_surfaces(&self) -> usize {
        #[cfg(windows)]
        return self.d3d12.open_surfaces();
        #[cfg(not(windows))]
        0
    }

    /// Producer leases currently held (surfaces the producer may not reuse yet).
    pub fn held_leases(&self) -> usize {
        #[cfg(windows)]
        return self.d3d12.held_leases();
        #[cfg(not(windows))]
        0
    }
}

/// The D3D adapter LUID of a device running on D3D12, as `HighPart << 32 | LowPart`.
pub(crate) fn adapter_luid(device: &wgpu::Device) -> Option<u64> {
    #[cfg(windows)]
    return d3d12::adapter_luid(device);
    #[cfg(not(windows))]
    {
        let _ = device;
        None
    }
}
