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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_core::media::{FramePayload, Lease, NativeSurface, PixelFormat, TextureId, VideoFrame};
use rbs_core::time::Timestamp;

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::{TextureDesc, TexturePool};
use crate::yuv_decode::check_color;

#[cfg(windows)]
mod d3d12;
#[cfg(target_vendor = "apple")]
pub(crate) mod metal;
#[cfg(target_os = "linux")]
mod vulkan;

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

/// Opens an NT shared texture handle of another process on `ctx`'s D3D12 device without
/// a copy (the cross-process preview's UI side). The handle stays the caller's.
#[cfg(windows)]
pub(crate) fn open_d3d12_shared(
    ctx: &GpuContext,
    handle: u64,
    usage: wgpu::TextureUsages,
) -> Result<wgpu::Texture, GpuError> {
    d3d12::open_with_usage(ctx, handle, usage).map(|o| o.texture)
}

/// Usage of aliased surfaces: sampled by the compositor, copyable for readback and tests.
const USAGE: wgpu::TextureUsages =
    wgpu::TextureUsages::TEXTURE_BINDING.union(wgpu::TextureUsages::COPY_SRC);

/// A producer surface opened by a backend: the wrapper texture and what it looks like.
struct Opened {
    /// The surface, or its luma plane for 4:2:0 surfaces.
    texture: wgpu::Texture,
    desc: TextureDesc,
    /// The interleaved CbCr plane of a 4:2:0 surface (NV12, P010). The two planes share
    /// one pool slot (a plane pair) that the compositor samples as it is.
    chroma: Option<(wgpu::Texture, TextureDesc)>,
}

struct Entry {
    /// Producer identity of the surface (handle value or object pointer).
    key: u64,
    /// The surface's pool slot (a plane pair for 4:2:0 surfaces). Frames reference it,
    /// so the native lease goes back once no frame does and the GPU is done with it.
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
    /// Queue work after which the lease goes back (also returns D3D12 surfaces to COMMON).
    handback: Option<Handback>,
}

/// What a lease waits for once no frame references its surface.
enum Handback {
    /// The hand-back submission that carries the state transition.
    Submitted(wgpu::SubmissionIndex),
    /// A work-done notification with this generation: everything submitted before it,
    /// including every read of the surface, has completed once [`Importer`]'s completed
    /// generation reaches it.
    Notified(u64),
}

/// How surfaces whose frames are gone are handed back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandbackKind {
    /// A command buffer with a transition to `PRESENT`, then a submit (one per tick for
    /// all surfaces). D3D12 needs it to return shared resources to `COMMON`; on Vulkan it
    /// moves the image to `PRESENT_SRC_KHR` (kept until it is checked on hardware).
    Transition,
    /// No GPU work: a work-done notification on the work already submitted. Metal has no
    /// resource states, so the transition would be an empty command buffer (about 21 heap
    /// allocations in wgpu per tick, ADR-0167 follow-up).
    Notify,
}

impl Default for HandbackKind {
    fn default() -> Self {
        if cfg!(target_vendor = "apple") {
            Self::Notify
        } else {
            Self::Transition
        }
    }
}

/// Imports native frames for one render device. Owned by the render thread.
///
/// Holds no GPU objects until the first import (P13).
#[derive(Default)]
pub struct Importer {
    entries: Vec<Entry>,
    tick: u64,
    handback_kind: HandbackKind,
    /// Last work-done generation handed out (`HandbackKind::Notify`).
    notified: u64,
    /// Highest generation whose work completed, written by wgpu's work-done callback on
    /// the thread that polls. Created with the first surface (P13).
    completed: Option<Arc<AtomicU64>>,
    #[cfg(windows)]
    fences: d3d12::Fences,
    #[cfg(target_os = "linux")]
    sync_files: vulkan::SyncFiles,
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
        // 4:2:0 frames: reject colour spaces the decode cannot handle before anything opens.
        match format.pixel_format {
            PixelFormat::Nv12 => check_color(format.color, 8)?,
            PixelFormat::P010 => check_color(format.color, 10)?,
            _ => {}
        }
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
        let id = entry.id;
        // 4:2:0 (perf audit R-11 step b): the frame is the plane pair itself, in the
        // source's own format and colour; the compositor samples it with no decoded copy.
        // The surface's bit depth must match the frame's pixel format.
        let planar = match pool.get(id).map(|t| t.desc().format) {
            Some(wgpu::TextureFormat::NV12) => Some(PixelFormat::Nv12),
            Some(wgpu::TextureFormat::P010) => Some(PixelFormat::P010),
            _ => None,
        };
        if planar.is_some_and(|expected| format.pixel_format != expected) {
            return Err(GpuError::Unsupported(
                "4:2:0 surface whose bit depth differs from the frame's pixel format",
            ));
        }
        let mut out = pool.frame(id, format, timestamp)?;
        out = out.with_sequence(sequence);
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
            #[cfg(target_os = "linux")]
            NativeSurface::DmaBuf(ref dmabuf) => {
                let key = vulkan::validate(ctx, dmabuf, sync)?;
                self.sync_files.wait(ctx, sync)?;
                Ok(key)
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
        let opened = open_backend(ctx, surface, frame_size)?;
        let id = match opened.chroma {
            None => pool.insert_external(ctx, opened.texture, opened.desc)?,
            Some((chroma, _)) => {
                let format = match opened.desc.format {
                    wgpu::TextureFormat::R16Unorm => wgpu::TextureFormat::P010,
                    _ => wgpu::TextureFormat::NV12,
                };
                let desc = TextureDesc {
                    format,
                    ..opened.desc
                };
                pool.insert_external_planes(ctx, opened.texture, chroma, desc)?
            }
        };
        ctx.metrics().count_surface_opened();
        if self.handback_kind == HandbackKind::Notify && self.completed.is_none() {
            self.completed = Some(Arc::new(AtomicU64::new(0)));
        }
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
        #[cfg(target_os = "linux")]
        self.sync_files.retire(ctx);
        if self.entries.is_empty() {
            return;
        }
        for e in &mut self.entries {
            if e.last_seen == tick {
                e.seen_at = now;
            }
        }

        // 1. Leases whose hand-back has completed go back to the producer. Notified
        //    hand-backs share one non-blocking poll, which runs wgpu's callbacks.
        if self
            .entries
            .iter()
            .any(|e| matches!(e.handback, Some(Handback::Notified(_))))
        {
            ctx.poll();
        }
        let completed = self
            .completed
            .as_ref()
            .map_or(0, |c| c.load(Ordering::Acquire));
        for e in &mut self.entries {
            let done = match &e.handback {
                Some(Handback::Submitted(index)) => ctx
                    .device()
                    .poll(wgpu::PollType::Wait {
                        submission_index: Some(index.clone()),
                        timeout: Some(Duration::ZERO),
                    })
                    .is_ok(),
                Some(Handback::Notified(generation)) => *generation <= completed,
                None => false,
            };
            if done {
                e.handback = None;
                e.native = None;
            }
        }

        // 2. Surfaces no frame references any more: after all earlier queue work, return
        //    them to COMMON (D3D12) and release the lease once that ran.
        let pending = |e: &Entry, pool: &TexturePool| {
            e.native.is_some() && e.handback.is_none() && pool.lease_count(e.id) == 0
        };
        let handback = match self.handback_kind {
            HandbackKind::Transition => self.submit_transitions(ctx, pool, pending),
            HandbackKind::Notify => self.notify(ctx, pool, pending),
        };
        if let Some(handback) = handback {
            for e in &mut self.entries {
                if pending(e, pool) {
                    e.handback = Some(match &handback {
                        Handback::Submitted(index) => Handback::Submitted(index.clone()),
                        Handback::Notified(generation) => Handback::Notified(*generation),
                    });
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

    /// Records a transition to `PRESENT` for every pending surface in one command buffer
    /// and submits it, or `None` when nothing is pending.
    fn submit_transitions(
        &self,
        ctx: &GpuContext,
        pool: &TexturePool,
        pending: impl Fn(&Entry, &TexturePool) -> bool,
    ) -> Option<Handback> {
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
            // Only the Y' texture of a plane pair: on D3D12 its whole-resource barrier
            // also returns the CbCr plane, whose wrapper never transitions (see
            // `d3d12::open_planar`).
            let transition = wgpu::TextureTransition {
                texture: texture.texture(),
                selector: None,
                state: wgpu::TextureUses::PRESENT,
            };
            enc.transition_resources(std::iter::empty(), std::iter::once(transition));
        }
        let enc = encoder?;
        Some(Handback::Submitted(ctx.queue().submit(Some(enc.finish()))))
    }

    /// Asks wgpu to report when the work submitted so far has completed, or `None` when
    /// nothing is pending. One boxed callback per tick, no command buffer.
    fn notify(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        pending: impl Fn(&Entry, &TexturePool) -> bool,
    ) -> Option<Handback> {
        if !self.entries.iter().any(|e| pending(e, pool)) {
            return None;
        }
        let completed = Arc::clone(self.completed.as_ref()?);
        self.notified += 1;
        let generation = self.notified;
        // Callbacks run in submission order; `fetch_max` keeps the value monotonic anyway.
        ctx.queue().on_submitted_work_done(move || {
            completed.fetch_max(generation, Ordering::AcqRel);
        });
        Some(Handback::Notified(generation))
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

fn open_backend(
    ctx: &GpuContext,
    surface: &NativeSurface,
    frame_size: (u32, u32),
) -> Result<Opened, GpuError> {
    let _ = frame_size; // Only DMA-BUFs take their size from the frame.
    match *surface {
        #[cfg(windows)]
        NativeSurface::D3dSharedHandle { handle, .. } => d3d12::open(ctx, handle),
        #[cfg(target_vendor = "apple")]
        NativeSurface::IoSurface { surface, .. } => metal::open(ctx, surface),
        #[cfg(target_os = "linux")]
        NativeSurface::DmaBuf(ref dmabuf) => vulkan::open(ctx, dmabuf, frame_size),
        _ => {
            let _ = ctx;
            Err(GpuError::Unsupported("surface kind"))
        }
    }
}

/// Opens a Vulkan device through the HAL with the importer's optional extensions, or `None`
/// when the adapter is not Vulkan. The flag says whether sync files can be waited on.
#[cfg(target_os = "linux")]
pub(crate) fn vulkan_request_device(
    adapter: &wgpu::Adapter,
    desc: &wgpu::DeviceDescriptor<'_>,
) -> Option<Result<(wgpu::Device, wgpu::Queue, bool), GpuError>> {
    vulkan::request_device(adapter, desc)
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)] // Tests fail by panicking.

    use std::sync::atomic::AtomicI64;
    use std::sync::{Arc, PoisonError};
    use std::time::Duration;

    use rbs_core::media::{Lease, LeaseOwner};
    use rbs_core::time::Timestamp;

    use super::{Entry, HandbackKind, Importer};
    use crate::pool::{TextureDesc, TexturePool};
    use crate::{GpuContext, GpuError, GpuRequest};

    #[derive(Default)]
    struct Producer(AtomicI64);

    // SAFETY: the counter is the only state; retain and release never fail.
    unsafe impl LeaseOwner for Producer {
        fn retain(&self, _token: u64) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn release(&self, _token: u64) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn context() -> Option<GpuContext> {
        match GpuContext::new(&GpuRequest {
            allow_software: true,
            ..GpuRequest::default()
        }) {
            Ok(ctx) => Some(ctx),
            Err(GpuError::NoAdapter) => {
                assert!(
                    std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
                    "RBS_REQUIRE_GPU is set but no GPU adapter was found"
                );
                eprintln!("SKIPPED: no GPU adapter");
                None
            }
            Err(e) => panic!("GPU context failed: {e}"),
        }
    }

    /// A surface read by a submission is handed back only after its frame is gone and the
    /// queue has finished, with the transition submit and with the work-done notification
    /// (no native producer on CI: the entry is made from a pool texture).
    #[test]
    fn a_lease_goes_back_after_the_frame_and_the_gpu_are_done() {
        let _turn = crate::TEST_DEVICE_TURN
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(ctx) = context() else {
            return;
        };
        for kind in [HandbackKind::Transition, HandbackKind::Notify] {
            let mut pool = TexturePool::new();
            let desc = TextureDesc::canvas(16, 16);
            let texture = ctx.device().create_texture(&wgpu::TextureDescriptor {
                label: Some("test surface"),
                size: wgpu::Extent3d {
                    width: 16,
                    height: 16,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: desc.format,
                usage: desc.usage,
                view_formats: &[],
            });
            let id = pool
                .insert_external(&ctx, texture.clone(), desc)
                .expect("insert");
            let producer = Arc::new(Producer::default());
            producer.retain(0);
            // SAFETY: one reference was just retained for this lease.
            let lease = unsafe { Lease::adopt(Arc::clone(&producer) as Arc<dyn LeaseOwner>, 0) };
            let mut importer = Importer {
                handback_kind: kind,
                completed: Some(Arc::default()),
                ..Importer::default()
            };
            importer.entries.push(Entry {
                key: 1,
                id,
                frame_size: (16, 16),
                last_seen: 0,
                seen_at: Timestamp::ZERO,
                native: Some(lease),
                handback: None,
            });
            let frame_lease = pool.lease(id).expect("lease");
            // A read of the surface in flight, as the compositor's would be.
            let buffer = ctx.device().create_buffer(&wgpu::BufferDescriptor {
                label: Some("test read"),
                size: 256 * 16,
                usage: wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let mut enc = ctx
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            enc.copy_texture_to_buffer(
                texture.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(256),
                        rows_per_image: None,
                    },
                },
                wgpu::Extent3d {
                    width: 16,
                    height: 16,
                    depth_or_array_layers: 1,
                },
            );
            ctx.queue().submit(Some(enc.finish()));

            importer.retire(&ctx, &mut pool, Timestamp::ZERO);
            assert_eq!(
                importer.held_leases(),
                1,
                "{kind:?}: a frame still holds it"
            );
            drop(frame_lease);
            importer.retire(&ctx, &mut pool, Timestamp::ZERO);
            assert_eq!(
                importer.held_leases(),
                1,
                "{kind:?}: the hand-back was only requested"
            );
            // Hang guard only: the queue is idle well before this.
            assert!(ctx.wait_until(Duration::from_secs(10)).expect("wait"));
            importer.retire(&ctx, &mut pool, Timestamp::ZERO);
            assert_eq!(importer.held_leases(), 0, "{kind:?}");
            assert_eq!(producer.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }
}
