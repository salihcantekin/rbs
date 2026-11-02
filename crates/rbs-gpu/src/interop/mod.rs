//! Native surface import (spec `docs/specs/gpu-compositor.md#native-import-interop`).
//!
//! This is a marked low-level module (principle P11): backend submodules wrap producer
//! surfaces through wgpu's HAL with `unsafe` calls, each with a `// SAFETY:` comment.
//!
//! [`Importer::import`] takes a native [`VideoFrame`] by value, aliases its surface as a
//! pool texture (opened once per surface and cached), makes the queue wait on the
//! producer's fence where there is one, and returns a `Texture` frame.
//! [`Importer::retire`] hands the producer's lease back once no frame references the
//! slot and the GPU has finished reading it.

use std::time::Duration;

use rbs_core::media::{FramePayload, Lease, NativeSurface, TextureId, VideoFrame};
use rbs_core::time::Timestamp;

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::{TextureDesc, TexturePool};

#[cfg(windows)]
mod d3d12;
#[cfg(target_vendor = "apple")]
mod metal;

/// Most producer surfaces one importer keeps open at a time.
///
/// Sized for several capture sessions at once: a WGC session keeps a ring of 5 surfaces
/// plus replaced ones during a resize (capture-port spec, "Native surface rules").
pub const MAX_SURFACES: usize = 64;
/// Time after which a cached surface that was not seen again is closed, whatever the
/// tick rate. Producers keep a handed-out handle open for at least 10 s after its last
/// reference (capture-port spec, "Native surface rules"), so a cached handle value is
/// forgotten before the producer may close it and the OS may reuse it.
pub const SURFACE_IDLE: Duration = Duration::from_secs(8);

/// Usage of aliased surfaces: sampled by the compositor, copyable for readback and tests.
#[cfg_attr(not(any(windows, target_vendor = "apple")), allow(dead_code))]
const USAGE: wgpu::TextureUsages =
    wgpu::TextureUsages::TEXTURE_BINDING.union(wgpu::TextureUsages::COPY_SRC);

/// A producer surface opened by a backend: the wrapper texture and what it looks like.
struct Opened {
    texture: wgpu::Texture,
    desc: TextureDesc,
}

struct Entry {
    /// Producer identity of the surface (handle value or object pointer).
    key: u64,
    id: TextureId,
    /// Frame size the surface was first delivered with. A frame of another size on the same
    /// key means the producer's handle value now names another object.
    frame_size: (u32, u32),
    /// Tick of the last import (least recently used eviction).
    last_seen: u64,
    /// Engine time of the `retire` that followed the last import (idle closing).
    seen_at: Timestamp,
    /// The producer's lease while the surface may still be read.
    native: Option<Lease>,
    /// Submission after which the lease goes back (also returns D3D12 surfaces to COMMON).
    handback: Option<wgpu::SubmissionIndex>,
}

/// Imports native frames for one render device. Owned by the render thread.
///
/// Holds no GPU objects until the first import (P13).
#[derive(Default)]
pub struct Importer {
    entries: Vec<Entry>,
    tick: u64,
    #[cfg(windows)]
    fences: d3d12::Fences,
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
        let FramePayload::Native { surface, .. } = *frame.payload() else {
            return Ok(frame);
        };
        let format = *frame.format();
        let timestamp = frame.timestamp();
        let sequence = frame.sequence();
        let source_time = frame.source_time();
        let Some((_, sync, lease)) = frame.into_native() else {
            return Err(GpuError::Unsupported("native frame without lease"));
        };

        // Validate and wait first: nothing is opened for a frame that is rejected.
        let key = self.prepare(ctx, &surface, sync)?;
        let size = (format.size.width, format.size.height);
        let cached = self.entries.iter().position(|e| e.key == key);
        let index = match cached {
            Some(i) if self.entries[i].frame_size == size => i,
            stale => {
                // A cached entry of another size is a reused handle value: reopen it.
                if let Some(i) = stale {
                    self.close(i, pool);
                }
                self.open(ctx, pool, key, &surface, size)?
            }
        };

        let entry = &mut self.entries[index];
        entry.last_seen = self.tick;
        entry.handback = None;
        // A producer resends a surface only after its lease was released, so this is
        // normally `None`; replacing drops a stale lease.
        entry.native = Some(lease);
        let mut out = pool
            .frame(entry.id, format, timestamp)?
            .with_sequence(sequence);
        if let Some(t) = source_time {
            out = out.with_source_time(t);
        }
        Ok(out)
    }

    /// Checks the surface and sync kind for this backend, stages any GPU wait, and returns
    /// the cache key of the surface.
    fn prepare(
        &mut self,
        ctx: &GpuContext,
        surface: &NativeSurface,
        sync: rbs_core::media::SurfaceSync,
    ) -> Result<u64, GpuError> {
        match *surface {
            #[cfg(windows)]
            NativeSurface::D3dSharedHandle {
                handle,
                nt,
                adapter_luid,
                ..
            } => {
                d3d12::validate(ctx, nt, adapter_luid)?;
                self.fences.wait(ctx, sync)?;
                Ok(handle)
            }
            #[cfg(target_vendor = "apple")]
            NativeSurface::IoSurface { surface, .. } => {
                metal::validate(sync)?;
                Ok(surface as u64)
            }
            NativeSurface::D3d11Texture { .. } => Err(GpuError::Unsupported(
                "D3D11 texture pointers; share the texture with an NT handle",
            )),
            _ => {
                let _ = (ctx, sync);
                Err(GpuError::Unsupported(
                    "surface kind not supported on this platform or backend",
                ))
            }
        }
    }

    fn open(
        &mut self,
        ctx: &GpuContext,
        pool: &mut TexturePool,
        key: u64,
        surface: &NativeSurface,
        frame_size: (u32, u32),
    ) -> Result<usize, GpuError> {
        if self.entries.len() >= MAX_SURFACES {
            self.evict_one(pool)?;
        }
        let opened = open_backend(ctx, surface)?;
        let id = pool.insert_external(ctx, opened.texture, opened.desc)?;
        ctx.metrics().count_surface_opened();
        self.entries.push(Entry {
            key,
            id,
            frame_size,
            last_seen: self.tick,
            // Set by the next `retire`, which sees `last_seen == tick`.
            seen_at: Timestamp::ZERO,
            native: None,
            handback: None,
        });
        Ok(self.entries.len() - 1)
    }

    fn evict_one(&mut self, pool: &mut TexturePool) -> Result<(), GpuError> {
        let i = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.native.is_none() && pool.lease_count(e.id) == 0)
            .min_by_key(|(_, e)| e.last_seen)
            .map(|(i, _)| i)
            .ok_or(GpuError::Unsupported(
                "more producer surfaces in flight than MAX_SURFACES",
            ))?;
        self.close(i, pool);
        Ok(())
    }

    /// Forgets entry `i`. Frames that still reference its slot keep it alive through their
    /// pool leases; the producer's lease, if any, is dropped.
    fn close(&mut self, i: usize, pool: &mut TexturePool) {
        let e = self.entries.swap_remove(i);
        let _ = pool.release(e.id);
    }

    /// Closes every cached surface that is not in use (no producer lease, no frame).
    ///
    /// Call when a source stops: its handle values may be reused by the OS afterwards,
    /// so they must not stay in the cache (capture-port spec, "Native surface rules").
    pub fn close_unused(&mut self, pool: &mut TexturePool) {
        let mut i = 0;
        while i < self.entries.len() {
            let e = &self.entries[i];
            if e.native.is_none() && pool.lease_count(e.id) == 0 {
                self.close(i, pool);
            } else {
                i += 1;
            }
        }
    }

    /// Hands producer leases back when their surfaces are no longer read, and closes
    /// surfaces not seen for [`SURFACE_IDLE`]. Call once per render tick with the engine's
    /// monotonic time (the tick's master-clock time).
    pub fn retire(&mut self, ctx: &GpuContext, pool: &mut TexturePool, now: Timestamp) {
        let tick = self.tick;
        self.tick += 1;
        if self.entries.is_empty() {
            return;
        }
        for e in &mut self.entries {
            if e.last_seen == tick {
                e.seen_at = now;
            }
        }

        // 1. Leases whose hand-back submission has completed go back to the producer.
        for e in &mut self.entries {
            let done = match &e.handback {
                Some(index) => ctx
                    .device()
                    .poll(wgpu::PollType::Wait {
                        submission_index: Some(index.clone()),
                        timeout: Some(Duration::ZERO),
                    })
                    .is_ok(),
                None => false,
            };
            if done {
                e.handback = None;
                e.native = None;
            }
        }

        // 2. Surfaces no frame references any more: after all earlier queue work, return
        //    them to COMMON (D3D12; a no-op elsewhere) and release the lease once that ran.
        let pending = |e: &Entry, pool: &TexturePool| {
            e.native.is_some() && e.handback.is_none() && pool.lease_count(e.id) == 0
        };
        let mut encoder: Option<wgpu::CommandEncoder> = None;
        for e in self.entries.iter().filter(|e| pending(e, pool)) {
            let Some(texture) = pool.get(e.id) else {
                continue;
            };
            let enc = encoder.get_or_insert_with(|| {
                ctx.device()
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("rbs-gpu import hand-back"),
                    })
            });
            enc.transition_resources(
                std::iter::empty(),
                std::iter::once(wgpu::TextureTransition {
                    texture: texture.texture(),
                    selector: None,
                    state: wgpu::TextureUses::PRESENT,
                }),
            );
        }
        if let Some(enc) = encoder {
            let index = ctx.queue().submit(Some(enc.finish()));
            for e in &mut self.entries {
                if pending(e, pool) {
                    e.handback = Some(index.clone());
                }
            }
        }

        // 3. Close surfaces the producer stopped sending.
        let mut i = 0;
        while i < self.entries.len() {
            let e = &self.entries[i];
            if e.native.is_none()
                && now.saturating_since(e.seen_at) > SURFACE_IDLE
                && pool.lease_count(e.id) == 0
            {
                self.close(i, pool);
            } else {
                i += 1;
            }
        }
    }

    /// Producer surfaces currently open.
    pub fn open_surfaces(&self) -> usize {
        self.entries.len()
    }

    /// Producer leases currently held (surfaces the producer may not reuse yet).
    pub fn held_leases(&self) -> usize {
        self.entries.iter().filter(|e| e.native.is_some()).count()
    }
}

fn open_backend(ctx: &GpuContext, surface: &NativeSurface) -> Result<Opened, GpuError> {
    match *surface {
        #[cfg(windows)]
        NativeSurface::D3dSharedHandle { handle, .. } => d3d12::open(ctx, handle),
        #[cfg(target_vendor = "apple")]
        NativeSurface::IoSurface { surface, .. } => metal::open(ctx, surface),
        _ => {
            let _ = ctx;
            Err(GpuError::Unsupported("surface kind"))
        }
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
