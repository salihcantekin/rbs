//! Engine side of the cross-process preview: a ring of RGBA IOSurfaces and a Mach service.

use std::collections::VecDeque;
use std::ffi::CString;
use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use mach2::message::{
    MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MOVE_SEND, MACH_MSGH_BITS_COMPLEX, MACH_SEND_TIMED_OUT,
    mach_msg_body_t, mach_msg_header_t, mach_msg_port_descriptor_t,
};
use mach2::port::{MACH_PORT_NULL, mach_port_t};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_io_surface::{
    IOSurfaceRef, kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceHeight,
    kIOSurfacePixelFormat, kIOSurfaceWidth,
};
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLTextureUsage};
use rbs_core::media::TextureId;
use rbs_core::time::Timestamp;

use super::mach::{self, RecvBuf};
use super::{FOURCC_RGBA, MAX_PREVIEW_SURFACES, PreviewDesc, PreviewEndpoint, PreviewStats};
use crate::context::GpuContext;
use crate::error::GpuError;
use crate::interop::metal::{PlaneTexture, alias_plane};
use crate::pool::TexturePool;

type Metal = wgpu::hal::api::Metal;

/// The render format of the aliased surfaces: the target applies the sRGB encoding.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Publishers created by this process; part of the service name.
static PUBLISHERS: AtomicU32 = AtomicU32::new(0);

struct Slot {
    io: CFRetained<IOSurfaceRef>,
    /// Our send right to the surface (`IOSurfaceCreateMachPort`), copied to subscribers.
    port: mach_port_t,
    view: wgpu::TextureView,
    /// Converted, not yet sent.
    pending: bool,
    /// Sent to the subscriber and not yet released.
    held: bool,
}

struct Pending {
    slot: usize,
    submission: wgpu::SubmissionIndex,
    timestamp: Timestamp,
    sequence: u64,
}

/// Shares scaled canvases with one UI process (spec "Preview export (cross-process)").
/// Owned by the render thread.
///
/// Installed but unused costs nothing: a publisher exists only while a preview output is
/// Active (P13), and while no UI is subscribed [`Self::publish`] does no GPU work.
pub struct PreviewPublisher {
    endpoint: PreviewEndpoint,
    width: u32,
    height: u32,
    /// Receive right of the service.
    service: mach_port_t,
    /// Our own send right to the service when it was registered with `bootstrap_register`.
    service_send: mach_port_t,
    /// Send right to the subscriber's port.
    client: mach_port_t,
    generation: u32,
    pipeline: wgpu::RenderPipeline,
    slots: Vec<Slot>,
    pending: VecDeque<Pending>,
    next: usize,
    buf: Box<RecvBuf>,
    stats: PreviewStats,
}

// SAFETY: the publisher is moved between threads only as a whole (the render executor may
// be created on the control thread). IOSurfaces are thread-safe reference-counted objects,
// Mach port names are plain integers valid in the whole task, and wgpu objects are `Send`.
unsafe impl Send for PreviewPublisher {}

impl PreviewPublisher {
    /// Creates the surfaces, aliases them on the render device and checks in a Mach
    /// service under a fresh name with a random token. Not for real-time threads.
    ///
    /// `desc.surfaces` is 2 to [`MAX_PREVIEW_SURFACES`]. The device must be Metal with
    /// unified memory, else `GpuError::Unsupported`.
    pub fn new(ctx: &GpuContext, desc: PreviewDesc) -> Result<Self, GpuError> {
        if !(2..=MAX_PREVIEW_SURFACES).contains(&desc.surfaces) {
            return Err(GpuError::Unsupported("preview ring size outside 2..=8"));
        }
        let (w, h) = (desc.width, desc.height);
        if w == 0 || h == 0 || w.max(h) > ctx.max_texture_dimension() {
            return Err(GpuError::InvalidSize {
                width: w,
                height: h,
            });
        }
        // SAFETY: the HAL device is only borrowed to read one property; the guard is
        // dropped at the end of this block.
        let unified = unsafe {
            ctx.device()
                .as_hal::<Metal>()
                .ok_or(GpuError::Unsupported("render device is not Metal"))?
                .raw_device()
                .hasUnifiedMemory()
        };
        if !unified {
            return Err(GpuError::Unsupported(
                "IOSurface preview on a GPU without unified memory",
            ));
        }

        let mut slots = Vec::with_capacity(desc.surfaces);
        for _ in 0..desc.surfaces {
            slots.push(create_slot(ctx, w, h)?);
        }
        let pipeline = pipeline(ctx);

        let n = PUBLISHERS.fetch_add(1, Ordering::Relaxed);
        let service = format!("org.rbs.preview.{}.{n}", std::process::id());
        let token = random_token();
        let name = CString::new(service.clone())
            .map_err(|_| GpuError::Export("service name contains NUL"))?;
        let (port, service_send) = match mach::check_in(&name) {
            Ok(p) => p,
            Err(e) => {
                for s in &slots {
                    mach::deallocate(s.port);
                }
                return Err(e);
            }
        };
        Ok(Self {
            endpoint: PreviewEndpoint { service, token },
            width: w,
            height: h,
            service: port,
            service_send,
            client: MACH_PORT_NULL,
            generation: 0,
            pipeline,
            pending: VecDeque::with_capacity(slots.len()),
            slots,
            next: 0,
            buf: Box::new(RecvBuf::new()),
            stats: PreviewStats::default(),
        })
    }

    /// What a UI needs to connect.
    pub fn endpoint(&self) -> &PreviewEndpoint {
        &self.endpoint
    }

    /// Preview size in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Whether a UI is subscribed.
    pub fn subscribed(&self) -> bool {
        self.client != MACH_PORT_NULL
    }

    /// Surfaces the UI holds (sent and not released).
    pub fn held(&self) -> usize {
        self.slots.iter().filter(|s| s.held).count()
    }

    /// Counters since creation.
    pub fn stats(&self) -> PreviewStats {
        self.stats
    }

    /// Global IOSurface IDs of the ring, in slot order (diagnostics and tests: the UI sees
    /// the same IDs, which shows both processes use the same memory).
    pub fn surface_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.slots.iter().map(|s| s.io.id())
    }

    /// Scales `canvas` (a canvas texture in `pool`, any size) into the next free surface.
    /// The frame is sent by a later [`Self::tick`] once the GPU has finished.
    ///
    /// Hot path; does not allocate in RBS code. Does nothing while no UI is subscribed
    /// (counted in `skipped`). Returns [`GpuError::ExportBusy`] when every surface is
    /// pending or held by the UI: the frame is dropped.
    pub fn publish(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        canvas: TextureId,
        timestamp: Timestamp,
        sequence: u64,
    ) -> Result<(), GpuError> {
        if self.client == MACH_PORT_NULL {
            self.stats.skipped += 1;
            return Ok(());
        }
        let tex = pool.get(canvas).ok_or(GpuError::StaleTexture(canvas))?;
        let source = tex
            .sampled_bind_group()
            .filter(|_| tex.desc().format == crate::CANVAS_FORMAT)
            .ok_or(GpuError::TargetMismatch(canvas))?;
        let n = self.slots.len();
        let slots = &self.slots;
        let Some(i) = (0..n)
            .map(|k| (self.next + k) % n)
            .find(|&i| !slots[i].pending && !slots[i].held)
        else {
            self.stats.busy += 1;
            return Err(GpuError::ExportBusy { surfaces: n as u32 });
        };
        let mut encoder = ctx
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rbs-gpu preview"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rbs-gpu preview"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.slots[i].view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, source, &[]);
            pass.draw(0..3, 0..1);
        }
        let submission = ctx.queue().submit(Some(encoder.finish()));
        self.slots[i].pending = true;
        self.next = (i + 1) % n;
        self.stats.converted += 1;
        // At most one entry per slot, so this never grows past the reserved capacity.
        self.pending.push_back(Pending {
            slot: i,
            submission,
            timestamp,
            sequence,
        });
        Ok(())
    }

    /// Handles `SUBSCRIBE` and `RELEASE` messages and sends every converted frame whose GPU
    /// work has completed. Call once per render tick. Never blocks; hot path, does not
    /// allocate in RBS code.
    ///
    /// Returns `GpuError::DeviceLost` once the device is lost; frames converted on a lost
    /// device are never sent.
    pub fn tick(&mut self, ctx: &GpuContext) -> Result<(), GpuError> {
        // Bounded: two messages per surface (a release and a resubscribe) per tick.
        for _ in 0..(2 * self.slots.len() + 2) {
            if !mach::ok(mach::receive(self.service, &mut self.buf, 0)) {
                break;
            }
            self.handle_message();
        }
        while let Some(front) = self.pending.front() {
            match ctx.device().poll(wgpu::PollType::Wait {
                submission_index: Some(front.submission.clone()),
                timeout: Some(Duration::ZERO),
            }) {
                Ok(_) => {}
                Err(wgpu::PollError::Timeout) => return Ok(()),
                Err(wgpu::PollError::WrongSubmissionIndex(..)) => {
                    return Err(GpuError::Unsupported("submission of another device"));
                }
            }
            if ctx.metrics().device_lost() {
                self.pending.clear();
                for s in &mut self.slots {
                    s.pending = false;
                }
                return Err(GpuError::DeviceLost);
            }
            let Some(p) = self.pending.pop_front() else {
                break;
            };
            self.slots[p.slot].pending = false;
            self.send_frame(&p);
        }
        Ok(())
    }

    fn send_frame(&mut self, p: &Pending) {
        if self.client == MACH_PORT_NULL {
            self.stats.dropped += 1;
            return;
        }
        let mut msg = mach::Frame {
            header: mach_msg_header_t {
                msgh_bits: mach::bits(MACH_MSG_TYPE_COPY_SEND, 0),
                msgh_size: size_of::<mach::Frame>() as u32,
                msgh_remote_port: self.client,
                msgh_local_port: MACH_PORT_NULL,
                msgh_voucher_port: MACH_PORT_NULL,
                msgh_id: mach::ID_FRAME,
            },
            generation: self.generation,
            slot: p.slot as u32,
            sequence: mach::split(p.sequence),
            timestamp: mach::split(p.timestamp.as_nanos()),
        };
        // SAFETY: a filled `Frame` of its own size; `client` is a send right we hold and it
        // is copied, not moved. The message carries no other rights.
        let ret = unsafe { mach::send(&mut msg, 0) };
        if mach::ok(ret) {
            self.slots[p.slot].held = true;
            self.stats.sent += 1;
        } else if ret == MACH_SEND_TIMED_OUT {
            // The UI's queue is full: it is behind. Drop this frame.
            self.stats.dropped += 1;
        } else {
            // `MACH_SEND_INVALID_DEST` (the UI is gone) or any other failure.
            self.stats.dropped += 1;
            self.unsubscribe();
        }
    }

    fn handle_message(&mut self) {
        let header = self.buf.header();
        match header.msgh_id {
            mach::ID_RELEASE if header.msgh_bits & MACH_MSGH_BITS_COMPLEX == 0 => {
                if let Some(r) = self.buf.read::<mach::Release>()
                    && r.generation == self.generation
                    && let Some(slot) = self.slots.get_mut(r.slot as usize)
                {
                    slot.held = false;
                }
                // A release carries no rights, but a stranger's message might.
                self.buf.destroy();
            }
            mach::ID_SUBSCRIBE if header.msgh_bits & MACH_MSGH_BITS_COMPLEX == 0 => {
                let valid = self.buf.read::<mach::Subscribe>().is_some_and(|s| {
                    s.version == mach::VERSION && mach::join(s.token) == self.endpoint.token
                });
                // The reply right arrives as a moved send right in the remote field.
                let reply = header.msgh_remote_port;
                let moved_send = header.msgh_bits & 0x1f == MACH_MSG_TYPE_MOVE_SEND;
                if !valid || !moved_send || reply == MACH_PORT_NULL {
                    self.stats.rejected += 1;
                    self.buf.destroy();
                    return;
                }
                self.unsubscribe();
                self.client = reply;
                self.generation = self.generation.wrapping_add(1);
                self.stats.subscribes += 1;
                self.send_surfaces();
            }
            _ => {
                self.stats.rejected += 1;
                self.buf.destroy();
            }
        }
    }

    fn send_surfaces(&mut self) {
        let mut ports = [mach_msg_port_descriptor_t::new(MACH_PORT_NULL, MACH_MSG_TYPE_COPY_SEND);
            mach::MAX_SURFACES];
        for (d, s) in ports.iter_mut().zip(&self.slots) {
            *d = mach_msg_port_descriptor_t::new(s.port, MACH_MSG_TYPE_COPY_SEND);
        }
        let mut msg = mach::Surfaces {
            header: mach_msg_header_t {
                msgh_bits: mach::bits(MACH_MSG_TYPE_COPY_SEND, 0) | MACH_MSGH_BITS_COMPLEX,
                msgh_size: size_of::<mach::Surfaces>() as u32,
                msgh_remote_port: self.client,
                msgh_local_port: MACH_PORT_NULL,
                msgh_voucher_port: MACH_PORT_NULL,
                msgh_id: mach::ID_SURFACES,
            },
            body: mach_msg_body_t {
                msgh_descriptor_count: mach::MAX_SURFACES as u32,
            },
            ports,
            version: mach::VERSION,
            generation: self.generation,
            width: self.width,
            height: self.height,
            count: self.slots.len() as u32,
        };
        // SAFETY: a filled `Surfaces` of its own size; every right in it is copied (the
        // client's send right and our surface send rights), so we still own all of them
        // whether or not the send succeeds. Unused descriptors are null.
        let ret = unsafe { mach::send(&mut msg, 0) };
        if !mach::ok(ret) {
            self.unsubscribe();
        }
    }

    /// Ends the subscription; every surface is free again.
    fn unsubscribe(&mut self) {
        mach::deallocate(self.client);
        self.client = MACH_PORT_NULL;
        for s in &mut self.slots {
            s.held = false;
        }
    }
}

impl Drop for PreviewPublisher {
    fn drop(&mut self) {
        self.unsubscribe();
        for s in &self.slots {
            mach::deallocate(s.port);
        }
        mach::deallocate(self.service_send);
        mach::destroy_receive(self.service);
    }
}

fn create_slot(ctx: &GpuContext, w: u32, h: u32) -> Result<Slot, GpuError> {
    let io = rgba_surface(w, h)?;
    let texture = alias_plane(
        ctx,
        &io,
        0,
        PlaneTexture {
            format: TARGET_FORMAT,
            mtl_format: MTLPixelFormat::RGBA8Unorm_sRGB,
            width: w,
            height: h,
            mtl_usage: MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        },
    )?;
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let port = io.create_mach_port();
    if port == MACH_PORT_NULL {
        return Err(GpuError::Export("IOSurfaceCreateMachPort failed"));
    }
    Ok(Slot {
        io,
        port,
        view,
        pending: false,
        held: false,
    })
}

/// A single-plane 'RGBA' IOSurface, 4 bytes per pixel.
fn rgba_surface(w: u32, h: u32) -> Result<CFRetained<IOSurfaceRef>, GpuError> {
    // SAFETY: the key is a CoreFoundation constant that lives for the whole process.
    let row_key = unsafe { kIOSurfaceBytesPerRow };
    let row = IOSurfaceRef::align_property(row_key, w as usize * 4);
    let num = |v: usize| CFNumber::new_isize(v as isize);
    let (width, height, bpe, bpr) = (num(w as usize), num(h as usize), num(4), num(row));
    let pixel_format = CFNumber::new_i32(FOURCC_RGBA as i32);
    // SAFETY: the keys are CoreFoundation constants that live for the whole process.
    let keys: [&CFString; 5] = unsafe {
        [
            kIOSurfaceWidth,
            kIOSurfaceHeight,
            kIOSurfaceBytesPerElement,
            kIOSurfaceBytesPerRow,
            kIOSurfacePixelFormat,
        ]
    };
    let values: [&CFType; 5] = [&width, &height, &bpe, &bpr, &pixel_format];
    let props = CFDictionary::from_slices(&keys, &values);
    // SAFETY: `props` maps IOSurface property keys to numbers, the documented types.
    let io = unsafe { IOSurfaceRef::new(props.as_ref()) }
        .ok_or(GpuError::Export("IOSurfaceCreate failed"))?;
    if io.width() != w as usize
        || io.height() != h as usize
        || io.plane_count() > 1
        || io.bytes_per_element() != 4
        || io.pixel_format() != FOURCC_RGBA
    {
        return Err(GpuError::Export("IOSurface has an unexpected layout"));
    }
    Ok(io)
}

fn pipeline(ctx: &GpuContext) -> wgpu::RenderPipeline {
    let device = ctx.device();
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("rbs-gpu preview"),
        source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/preview.wgsl").into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("rbs-gpu preview"),
        bind_group_layouts: &[Some(&ctx.sampled_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("rbs-gpu preview"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_fullscreen"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_preview"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: TARGET_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// 64 random bits from the standard library's OS-seeded hasher keys.
fn random_token() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    let a = h.finish();
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(a);
    a ^ h.finish().rotate_left(32)
}
