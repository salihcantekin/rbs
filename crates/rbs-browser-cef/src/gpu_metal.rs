//! Copies CEF's accelerated-paint IOSurface into the studio's slot surfaces on macOS
//! (ADR-0167). The callback never waits on the CPU: it claims a slot, records a Metal
//! blit, adds a completed handler and commits. The handler only marks the slot done;
//! CEF's UI thread publishes finished copies in claim order ([`Copier::collect`]), so
//! the frame section keeps one writer.
//!
//! Marked low-level module (P11). Every `unsafe` block states its invariant.

use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CFRetained;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder,
    MTLCommandQueue, MTLCopyAllDevices, MTLDevice, MTLPixelFormat, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage,
};
use rbs_browser_ipc::MetalShare;
use rbs_browser_shm::{Collected, CopyDone, CopyQueue, FrameRing};

/// How long the studio's reply to the surface request may take (decision 3).
const FETCH_TIMEOUT_MS: u32 = 2_000;

/// What one accelerated paint did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Copied {
    /// The copy is recorded and committed; it is published once it completes.
    Recorded,
    /// Every slot was held: the frame is dropped (not counted as a failure).
    NoSlot,
    /// CEF's surface has another size, or recording the copy failed.
    Failed,
}

type Device = ProtocolObject<dyn MTLDevice>;
type Texture = ProtocolObject<dyn MTLTexture>;

pub struct Copier {
    device: Retained<Device>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    slots: Vec<Retained<Texture>>,
    _surfaces: Vec<CFRetained<IOSurfaceRef>>,
    width: usize,
    height: usize,
    /// Set by the completed handlers (any thread). This `Arc` lives in a static for
    /// the whole process, so a handler never drops the last reference (decision 4).
    done: Arc<CopyDone>,
    /// Claim order. CEF's UI thread only.
    queue_state: RefCell<CopyQueue>,
    /// Nanoseconds a copy may stay in flight: 30 page frames (decision 5).
    limit: u64,
    /// CEF's UI thread, which opened the copier; `copy` and `collect` do nothing on
    /// any other, so the `RefCell` is never shared (the `Sync` claim below).
    owner: std::thread::ThreadId,
}

/// `'BGRA'`, the only CEF surface layout the copy takes.
const FOURCC_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

// SAFETY: CEF calls `copy` only on its UI thread, which is the thread that runs the
// runner's message loop (`collect`), opens the copier and keeps it until exit; the
// `RefCell` and the Metal objects are therefore used from one thread. The completed
// handlers touch only `done` (atomics) and the pump's wake, never `self`. The static
// `OnceLock` needs `Sync` only to exist.
unsafe impl Send for Copier {}
// SAFETY: as `Send`: one thread uses the value; the handlers use only `done`.
unsafe impl Sync for Copier {}

impl Copier {
    /// Fetches the studio's surfaces and opens them on the studio's Metal device.
    /// Waits for the studio's reply (at most 2 s), so it runs before CEF starts.
    /// `Err` names the fallback reason: `adapter` (not exactly one Metal device, or
    /// not the studio's) or `open` (the handover or the surfaces failed).
    pub fn open(
        share: &MetalShare,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, &'static str> {
        let devices = MTLCopyAllDevices();
        if devices.count() != 1 {
            return Err("adapter");
        }
        let device = devices.objectAtIndex(0);
        if device.registryID() != share.registry_id {
            return Err("adapter");
        }
        let ports = rbs_mach::browser::fetch(&share.service, share.token, FETCH_TIMEOUT_MS)
            .map_err(|_| "open")?;
        let mut surfaces = Vec::with_capacity(ports.len());
        for port in ports {
            let surface = IOSurfaceRef::lookup_from_mach_port(port);
            rbs_mach::deallocate(port);
            surfaces.push(surface);
        }
        let surfaces: Vec<CFRetained<IOSurfaceRef>> =
            surfaces.into_iter().collect::<Option<_>>().ok_or("open")?;
        let (w, h) = (width as usize, height as usize);
        if surfaces.len() != share.count as usize
            || surfaces.iter().any(|s| s.width() != w || s.height() != h)
        {
            return Err("open");
        }
        let slots = surfaces
            .iter()
            .map(|s| texture(&device, s, MTLTextureUsage::ShaderRead))
            .collect::<Option<Vec<_>>>()
            .ok_or("open")?;
        let queue = device.newCommandQueue().ok_or("open")?;
        Ok(Self {
            device,
            queue,
            slots,
            _surfaces: surfaces,
            width: w,
            height: h,
            done: Arc::new(CopyDone::new()),
            queue_state: RefCell::new(CopyQueue::new()),
            limit: 30 * 1_000_000_000 / u64::from(fps.clamp(1, 60)),
            owner: std::thread::current().id(),
        })
    }

    /// Records a copy of `source` (CEF's surface, valid for this callback) into a
    /// free slot painted at `painted`. Never waits (ADR-0157 decision 1).
    pub fn copy(&self, source: *mut std::ffi::c_void, ring: &FrameRing, painted: u64) -> Copied {
        let Some(source) = NonNull::new(source.cast::<IOSurfaceRef>()) else {
            return Copied::Failed;
        };
        // SAFETY: CEF passes an `IOSurfaceRef` that is valid for this callback; the
        // retain keeps it alive until the completed handler drops it.
        let source: CFRetained<IOSurfaceRef> = unsafe { CFRetained::retain(source) };
        if std::thread::current().id() != self.owner {
            return Copied::Failed;
        }
        if source.width() != self.width
            || source.height() != self.height
            || source.pixel_format() != FOURCC_BGRA
        {
            return Copied::Failed;
        }
        let Ok(mut queue) = self.queue_state.try_borrow_mut() else {
            return Copied::Failed;
        };
        let now = rbs_browser_shm::os_monotonic_nanos();
        let Some((slot, _)) = ring.claim_copy(&mut queue, &self.done, painted, now) else {
            return Copied::NoSlot;
        };
        // A panic while recording gives the claim back instead of leaving it in
        // flight until the 30-frame timeout.
        let recorded =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.record(&source, slot)))
                .unwrap_or(false);
        if !recorded {
            ring.cancel_copy(&mut queue, &self.done);
            return Copied::Failed;
        }
        Copied::Recorded
    }

    fn record(&self, source: &CFRetained<IOSurfaceRef>, slot: u32) -> bool {
        let Some(dest) = self.slots.get(slot as usize) else {
            return false;
        };
        // Per callback: CEF may hand another surface under the same pointer later
        // (ADR-0157 decision 7).
        let Some(src) = texture(&self.device, source, MTLTextureUsage::ShaderRead) else {
            return false;
        };
        let Some(buffer) = self.queue.commandBuffer() else {
            return false;
        };
        let Some(blit) = buffer.blitCommandEncoder() else {
            return false;
        };
        // SAFETY: both textures are `BGRA8Unorm` of the slot size (checked above and
        // at open).
        unsafe { blit.copyFromTexture_toTexture(&src, dest) };
        blit.endEncoding();
        // Decision 7: CEF's pool can see the surface in use until the copy is done.
        source.increment_use_count();
        let done = Arc::clone(&self.done);
        let held = source.clone();
        // The handler stores a word, gives the use count back and wakes the loop. It
        // does not allocate, log or panic. The wake takes the pump's mutex, held by the
        // UI thread only for a few instructions, so Metal's completion thread may wait
        // that long; nothing on Metal's thread waits on the UI thread's work.
        let handler = block2::RcBlock::new(
            move |buffer: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                // SAFETY: Metal passes the completed command buffer, valid for the call.
                let status = unsafe { buffer.as_ref() }.status();
                done.complete(slot, status == MTLCommandBufferStatus::Completed);
                held.decrement_use_count();
                crate::pump::PUMP.wake();
            },
        );
        // SAFETY: Metal copies the block before `addCompletedHandler` returns, and
        // the block is `'static` (it owns everything it uses).
        unsafe { buffer.addCompletedHandler(block2::RcBlock::as_ptr(&handler)) };
        buffer.commit();
        true
    }

    /// Publishes the finished copies in claim order (CEF's UI thread). `now` is OS
    /// monotonic nanoseconds. Never waits.
    pub fn collect(&self, ring: &FrameRing, now: u64) -> Collected {
        if std::thread::current().id() != self.owner {
            return Collected::default();
        }
        match self.queue_state.try_borrow_mut() {
            Ok(mut queue) => ring.collect_copies(&mut queue, &self.done, now, self.limit),
            Err(_) => Collected::default(),
        }
    }
}

fn texture(
    device: &Device,
    surface: &IOSurfaceRef,
    usage: MTLTextureUsage,
) -> Option<Retained<Texture>> {
    // SAFETY: plain descriptor constructor with the surface's own size.
    let desc = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm,
            surface.width(),
            surface.height(),
            false,
        )
    };
    desc.setUsage(usage);
    device.newTextureWithDescriptor_iosurface_plane(&desc, surface, 0)
}
