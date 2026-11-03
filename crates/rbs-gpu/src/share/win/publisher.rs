//! Engine side of the cross-process preview on Windows: a ring of shared D3D12 textures
//! and a named pipe (ADR-0035).

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rbs_core::media::TextureId;
use rbs_core::time::Timestamp;
use windows::Win32::Foundation::{
    DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_BROKEN_PIPE,
    ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GENERIC_ALL,
    HANDLE,
};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_HEAP_FLAG_SHARED, D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_DIMENSION_TEXTURE2D, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
    D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS, D3D12_RESOURCE_STATE_COMMON,
    D3D12_TEXTURE_LAYOUT_UNKNOWN, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE};
use windows::core::PCWSTR;

use super::pipe::{self, Op, Owned, Poll};
use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::TexturePool;
use crate::share::{MAX_PREVIEW_SURFACES, PreviewDesc, PreviewEndpoint, PreviewStats};

type Dx12 = wgpu::hal::api::Dx12;

/// The render format of the shared textures. Flip-model presentation and the composition
/// swapchain take no sRGB formats, so the surfaces are `R8G8B8A8_UNORM` and the preview
/// shader encodes sRGB itself (`fs_preview_encode`).
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Publishers created by this process; part of the pipe name.
static PUBLISHERS: AtomicU32 = AtomicU32::new(0);

/// Writes in flight at most: one per surface (frames) plus `SURFACES`.
const WRITES: usize = MAX_PREVIEW_SURFACES + 2;

struct Slot {
    /// Keeps the resource alive as long as its shared handle exists.
    _resource: ID3D12Resource,
    /// NT handle of the resource in this process, duplicated into each subscriber.
    shared: Owned,
    view: wgpu::TextureView,
    pending: bool,
    held: bool,
}

struct Pending {
    slot: usize,
    submission: wgpu::SubmissionIndex,
    timestamp: Timestamp,
    sequence: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Conn {
    /// No connect started (the last one ended or failed).
    Idle,
    /// `ConnectNamedPipe` runs.
    Listening,
    /// A client is connected; its first message must be a valid `SUBSCRIBE`.
    Connected,
    /// The client subscribed and has the surfaces.
    Subscribed,
}

/// Shares scaled canvases with one UI process (spec "Preview export (cross-process)",
/// ADR-0035). Owned by the render thread.
///
/// Installed but unused costs nothing: a publisher exists only while a preview output is
/// Active (P13), and while no UI is subscribed [`Self::publish`] does no GPU work.
pub struct PreviewPublisher {
    endpoint: PreviewEndpoint,
    width: u32,
    height: u32,
    adapter_luid: u64,
    pipe: Owned,
    conn: Conn,
    connect: Box<Op>,
    read: Box<Op>,
    #[allow(
        clippy::vec_box,
        reason = "the kernel holds pointers into each operation while it runs"
    )]
    writes: Vec<Box<Op>>,
    generation: u32,
    pipeline: wgpu::RenderPipeline,
    slots: Vec<Slot>,
    pending: VecDeque<Pending>,
    next: usize,
    stats: PreviewStats,
}

// SAFETY: the publisher is moved between threads only as a whole. Kernel handles are
// process-wide, COM resources of a D3D12 device are free-threaded, the overlapped
// operations are boxed (their addresses do not move) and wgpu objects are `Send`.
unsafe impl Send for PreviewPublisher {}

impl PreviewPublisher {
    /// Creates the shared textures on the render device and a named pipe under a fresh
    /// name with a random token. Not for real-time threads.
    ///
    /// `desc.surfaces` is 2 to [`MAX_PREVIEW_SURFACES`]. The device must be D3D12, else
    /// `GpuError::Unsupported`.
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
        let adapter_luid = ctx
            .adapter_luid()
            .ok_or(GpuError::Unsupported("render device is not D3D12"))?;
        let mut slots = Vec::with_capacity(desc.surfaces);
        for _ in 0..desc.surfaces {
            slots.push(create_slot(ctx, w, h)?);
        }
        let pipeline = crate::share::preview_pipeline(ctx, TARGET_FORMAT, "fs_preview_encode");

        let n = PUBLISHERS.fetch_add(1, Ordering::Relaxed);
        let mut random = [0u8; 16];
        pipe::random_bytes(&mut random)?;
        let token = u64::from_le_bytes(random[..8].try_into().unwrap_or_default());
        let nonce = u32::from_le_bytes(random[8..12].try_into().unwrap_or_default());
        let service = format!("org.rbs.preview.{}.{n}.{nonce:08x}", std::process::id());
        let path = pipe::pipe_path(&service);
        // SAFETY: `path` is a NUL-terminated UTF-16 string that outlives the call. One
        // instance, created first (a squatter makes this fail), local clients only, default
        // security (only the creating user, SYSTEM and administrators may write).
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(path.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                pipe::PIPE_BUFFER,
                pipe::PIPE_BUFFER,
                0,
                None,
            )
        };
        if handle.is_invalid() {
            return Err(GpuError::Export("CreateNamedPipeW failed"));
        }
        let pipe = Owned(handle);
        let mut writes = Vec::with_capacity(WRITES);
        for _ in 0..WRITES {
            writes.push(Op::new()?);
        }
        let mut publisher = Self {
            endpoint: PreviewEndpoint { service, token },
            width: w,
            height: h,
            adapter_luid,
            pipe,
            conn: Conn::Idle,
            connect: Op::new()?,
            read: Op::new()?,
            writes,
            generation: 0,
            pipeline,
            pending: VecDeque::with_capacity(slots.len()),
            slots,
            next: 0,
            stats: PreviewStats::default(),
        };
        publisher.listen();
        Ok(publisher)
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
        self.conn == Conn::Subscribed
    }

    /// Surfaces the UI holds (sent and not released).
    pub fn held(&self) -> usize {
        self.slots.iter().filter(|s| s.held).count()
    }

    /// Counters since creation.
    pub fn stats(&self) -> PreviewStats {
        self.stats
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
        if self.conn != Conn::Subscribed {
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
        let submission =
            crate::share::render_preview(ctx, &self.pipeline, source, &self.slots[i].view);
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

    /// Accepts a UI, handles `SUBSCRIBE`, `RELEASE` and the UI's end, and sends every
    /// converted frame whose GPU work has completed. Call once per render tick. Never
    /// blocks: every pipe operation is overlapped and only polled; hot path, does not
    /// allocate in RBS code.
    ///
    /// Returns `GpuError::DeviceLost` once the device is lost; frames converted on a lost
    /// device are never sent.
    pub fn tick(&mut self, ctx: &GpuContext) -> Result<(), GpuError> {
        if self.conn == Conn::Idle {
            self.listen();
        }
        if self.conn == Conn::Listening {
            // SAFETY: the connect operation was started on our pipe.
            match unsafe { self.connect.poll(self.pipe.raw()) } {
                Poll::Pending => {}
                Poll::Done(_) => self.connected(),
                Poll::Failed(_) => self.disconnect(),
            }
        }
        // Bounded: one release per surface plus `SUBSCRIBE`; a UI holds at most one
        // release per surface, so this drains what a well-behaved UI can have sent.
        for _ in 0..=self.slots.len() {
            if !matches!(self.conn, Conn::Connected | Conn::Subscribed) {
                break;
            }
            // SAFETY: the read was started on our pipe.
            match unsafe { self.read.poll(self.pipe.raw()) } {
                Poll::Pending => break,
                Poll::Done(n) => {
                    self.handle_message(n as usize);
                    self.start_read();
                }
                Poll::Failed(_) => {
                    // Broken pipe (the UI closed its end or exited), a too long message or
                    // a cancelled read: the UI can hold no surface any more, or broke the
                    // protocol.
                    self.disconnect();
                }
            }
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

    /// Starts waiting for a client.
    fn listen(&mut self) {
        // A read or write of the last connection may still be finishing; wait for the
        // kernel to give them back before listening again (next tick).
        if self.read.running() || self.writes.iter().any(|w| w.running()) {
            self.conn = Conn::Idle;
            return;
        }
        // Completions of the last connection are stale.
        // SAFETY: each operation was started on our pipe; none runs (checked above).
        unsafe {
            let _ = self.read.poll(self.pipe.raw());
            for w in &mut self.writes {
                let _ = w.poll(self.pipe.raw());
            }
        }
        let ov = self.connect.prepare_raw();
        // SAFETY: the pipe is open and not connected; `ov` points into the boxed connect
        // operation, which outlives the call (cancelled on drop).
        let r = unsafe { ConnectNamedPipe(self.pipe.raw(), Some(ov)) };
        match r {
            Ok(()) => self.connected(),
            Err(e) if pipe::win32(&e) == ERROR_IO_PENDING => {
                self.connect.mark_issued();
                self.conn = Conn::Listening;
            }
            // A client connected between creating (or disconnecting) the pipe and now.
            Err(e) if pipe::win32(&e) == ERROR_PIPE_CONNECTED => self.connected(),
            // For example a client that disconnected before we called; try again next tick.
            Err(_) => {
                // SAFETY: our own pipe handle.
                let _ = unsafe { DisconnectNamedPipe(self.pipe.raw()) };
                self.conn = Conn::Idle;
            }
        }
    }

    fn connected(&mut self) {
        self.conn = Conn::Connected;
        self.start_read();
    }

    fn start_read(&mut self) {
        if !matches!(self.conn, Conn::Connected | Conn::Subscribed) {
            return;
        }
        // SAFETY: our open pipe; the read is not issued (its completion was just taken).
        if unsafe { self.read.read(self.pipe.raw()) }.is_err() {
            self.disconnect();
        }
    }

    /// One message from the client: `SUBSCRIBE` first, then `RELEASE`s.
    fn handle_message(&mut self, len: usize) {
        let bytes = &self.read.buf[..len.min(pipe::MSG_BYTES)];
        match self.conn {
            Conn::Connected => {
                let valid = pipe::decode::<{ pipe::SUBSCRIBE_WORDS }>(bytes).is_some_and(|w| {
                    w[0] == pipe::ID_SUBSCRIBE
                        && w[1] == pipe::VERSION
                        && pipe::join([w[2], w[3]]) == self.endpoint.token
                });
                if !valid {
                    // A wrong token or another protocol: the pipe has one instance, so a
                    // stranger is not allowed to keep it.
                    self.stats.rejected += 1;
                    self.disconnect();
                    return;
                }
                self.subscribe();
            }
            Conn::Subscribed => match pipe::decode::<{ pipe::RELEASE_WORDS }>(bytes) {
                Some([pipe::ID_RELEASE, generation, slot]) if generation == self.generation => {
                    if let Some(s) = self.slots.get_mut(slot as usize) {
                        s.held = false;
                    }
                }
                _ => self.stats.rejected += 1,
            },
            Conn::Idle | Conn::Listening => {}
        }
    }

    /// Duplicates the surfaces into the client's process and sends `SURFACES`.
    fn subscribe(&mut self) {
        let mut pid = 0u32;
        // SAFETY: our connected pipe; `pid` is a valid out pointer.
        if unsafe { GetNamedPipeClientProcessId(self.pipe.raw(), &mut pid) }.is_err() {
            self.stats.rejected += 1;
            self.disconnect();
            return;
        }
        // SAFETY: opens the client process for handle duplication only.
        let process = match unsafe { OpenProcess(PROCESS_DUP_HANDLE, false, pid) } {
            Ok(h) => Owned(h),
            Err(_) => {
                self.stats.rejected += 1;
                self.disconnect();
                return;
            }
        };
        let mut handles = [0u64; pipe::MAX_SURFACES];
        let mut ok = true;
        for (out, slot) in handles.iter_mut().zip(&self.slots) {
            let mut target = HANDLE::default();
            // SAFETY: duplicates our own live NT handle into the client process with the
            // same access; `target` receives the value valid in that process.
            let r = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    slot.shared.raw(),
                    process.raw(),
                    &mut target,
                    0,
                    false,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            if r.is_err() {
                ok = false;
                break;
            }
            *out = target.0 as usize as u64;
        }
        self.generation = self.generation.wrapping_add(1);
        let sent = ok && self.send_surfaces(&handles);
        if !sent {
            // The client never learns these handle values: close them in its process.
            for &h in handles.iter().filter(|&&h| h != 0) {
                // SAFETY: closes a handle we duplicated into the client process above.
                let _ = unsafe {
                    DuplicateHandle(
                        process.raw(),
                        HANDLE(h as usize as *mut c_void),
                        HANDLE::default(),
                        std::ptr::null_mut(),
                        0,
                        false,
                        DUPLICATE_CLOSE_SOURCE,
                    )
                };
            }
            self.stats.rejected += 1;
            self.disconnect();
            return;
        }
        self.conn = Conn::Subscribed;
        self.stats.subscribes += 1;
    }

    fn send_surfaces(&mut self, handles: &[u64; pipe::MAX_SURFACES]) -> bool {
        let mut words = [0u32; pipe::SURFACES_WORDS];
        let luid = pipe::split(self.adapter_luid);
        words[..8].copy_from_slice(&[
            pipe::ID_SURFACES,
            pipe::VERSION,
            self.generation,
            self.width,
            self.height,
            self.slots.len() as u32,
            luid[0],
            luid[1],
        ]);
        for (pair, &h) in words[8..].chunks_exact_mut(2).zip(handles) {
            pair.copy_from_slice(&pipe::split(h));
        }
        self.write(words).is_ok()
    }

    fn send_frame(&mut self, p: &Pending) {
        if self.conn != Conn::Subscribed {
            self.stats.dropped += 1;
            return;
        }
        let seq = pipe::split(p.sequence);
        let ts = pipe::split(p.timestamp.as_nanos());
        let words = [
            pipe::ID_FRAME,
            self.generation,
            p.slot as u32,
            seq[0],
            seq[1],
            ts[0],
            ts[1],
        ];
        match self.write(words) {
            Ok(()) => {
                self.slots[p.slot].held = true;
                self.stats.sent += 1;
            }
            Err(gone) => {
                self.stats.dropped += 1;
                if gone {
                    // The UI's end is closed: it holds no surface any more.
                    self.disconnect();
                }
            }
        }
    }

    /// Writes one message without waiting. `Err(true)` when the UI is gone, `Err(false)`
    /// when the message was dropped (every write buffer busy: a UI that stopped reading).
    fn write<const N: usize>(&mut self, words: [u32; N]) -> Result<(), bool> {
        let pipe = self.pipe.raw();
        let Some(op) = self.writes.iter_mut().find(|w| !w.running()) else {
            return Err(false);
        };
        // SAFETY: the operation does not run (checked above); taking its old completion
        // does not wait.
        let _ = unsafe { op.poll(pipe) };
        let len = pipe::encode(words, &mut op.buf);
        // SAFETY: our open pipe; the boxed operation outlives the write.
        match unsafe { op.write(pipe, len) } {
            Ok(()) => Ok(()),
            Err(e) => {
                Err(e == ERROR_BROKEN_PIPE || e == ERROR_NO_DATA || e == ERROR_PIPE_NOT_CONNECTED)
            }
        }
    }

    /// Ends the connection; every surface is free again. Called only once the UI holds no
    /// surface: its end of the pipe is closed (it dropped the subscriber and every frame,
    /// or exited), or it never subscribed. The next tick listens again.
    fn disconnect(&mut self) {
        // SAFETY: our own pipe handle; pending reads and writes on it fail and complete.
        let _ = unsafe { DisconnectNamedPipe(self.pipe.raw()) };
        self.conn = Conn::Idle;
        for s in &mut self.slots {
            s.held = false;
        }
    }
}

impl Drop for PreviewPublisher {
    fn drop(&mut self) {
        let pipe = self.pipe.raw();
        // SAFETY: every operation was started on `pipe`; cancelling waits until the kernel
        // no longer uses its memory, before the boxes are freed.
        unsafe {
            self.connect.cancel(pipe);
            self.read.cancel(pipe);
            for w in &mut self.writes {
                w.cancel(pipe);
            }
            let _ = DisconnectNamedPipe(pipe);
        }
    }
}

fn create_slot(ctx: &GpuContext, w: u32, h: u32) -> Result<Slot, GpuError> {
    // SAFETY: the HAL device is borrowed to create one resource and its shared handle; no
    // wgpu state is touched and the guard is dropped at the end of the block.
    let (resource, shared) = unsafe {
        let hal = ctx
            .device()
            .as_hal::<Dx12>()
            .ok_or(GpuError::Unsupported("render device is not D3D12"))?;
        let device = hal.raw_device();
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: u64::from(w),
            Height: h,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            // Simultaneous access: the texture decays to COMMON after each submission, so
            // another device (the desktop compositor) may read it without a transition.
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
                | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
        };
        let mut resource: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_SHARED,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut resource,
            )
            .map_err(|_| GpuError::Export("CreateCommittedResource failed"))?;
        let resource = resource.ok_or(GpuError::Export("CreateCommittedResource returned null"))?;
        let shared = device
            .CreateSharedHandle(&resource, None, GENERIC_ALL.0, PCWSTR::null())
            .map_err(|_| GpuError::Export("CreateSharedHandle failed"))?;
        (resource, Owned(shared))
    };
    let size = wgpu::Extent3d {
        width: w,
        height: h,
        depth_or_array_layers: 1,
    };
    // SAFETY: `resource` was created on this device as a single-mip, single-sample 2D
    // R8G8B8A8_UNORM texture of `size` with render-target use; the wrapper keeps its own
    // COM reference.
    let hal_texture = unsafe {
        wgpu::hal::dx12::Device::texture_from_raw(
            resource.clone(),
            TARGET_FORMAT,
            wgpu::TextureDimension::D2,
            size,
            1,
            1,
        )
    };
    // SAFETY: the HAL texture belongs to this device and matches the descriptor; it starts
    // in COMMON, which wgpu calls PRESENT.
    let texture = unsafe {
        ctx.device().create_texture_from_hal::<Dx12>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("rbs-gpu preview"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TARGET_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            },
            wgpu::TextureUses::PRESENT,
        )
    };
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Ok(Slot {
        _resource: resource,
        shared,
        view,
        pending: false,
        held: false,
    })
}
