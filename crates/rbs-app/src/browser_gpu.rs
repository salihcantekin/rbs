//! Studio side of a browser page's shared textures (ADR-0109 decision 8, ADR-0157).
//!
//! The textures live in [`PageSlots`]. A frame's lease holds that `Arc`, and so does
//! a grave that the control thread and a short-lived sweeper thread drain, so the
//! last Direct3D release is not on the render thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rbs_browser_shm::FrameRing;
use rbs_core::media::{
    ColorSpace, D3dApi, Lease, LeaseOwner, NativeSurface, PixelFormat, Size, SurfaceSync,
    VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_gpu::browser_pages::PageTextures;

use crate::page_grave::Grave;

/// Textures plus the lease counts in the frame section.
pub struct PageSlots {
    ring: FrameRing,
    textures: PageTextures,
    width: u32,
    height: u32,
}

impl PageSlots {
    /// Opens the section `name` and keeps `textures` alive with it.
    pub fn open(
        name: &str,
        width: u32,
        height: u32,
        textures: PageTextures,
    ) -> std::io::Result<Arc<Self>> {
        let ring = FrameRing::open(name, width, height)?;
        Ok(Arc::new(Self {
            ring,
            textures,
            width,
            height,
        }))
    }

    /// A reader of the page fence's completed value, for the supervisor's gate.
    pub fn fence_read(&self) -> rbs_gpu::browser_pages::FenceRead {
        self.textures.fence_read()
    }

    /// A frame on slot `slot`, taking over the lease the supervisor's gate already
    /// holds (ADR-0157 decision 4). The fence has completed, so the sync is
    /// [`SurfaceSync::Ready`] (decision 2). `None`, with the lease given back, for a
    /// slot this page has no texture for.
    pub fn picture(self: &Arc<Self>, slot: u32, stamp: Option<Timestamp>) -> Option<VideoFrame> {
        let Some(handle) = self.textures.texture(slot as usize) else {
            self.ring.lease_sub(slot);
            return None;
        };
        // SAFETY: the gate counted the one reference this lease releases. The handle
        // stays open while this `Arc` lives, which the lease and the grave both hold.
        // The helper claims a slot only while its lease count is zero.
        let lease =
            unsafe { Lease::adopt(Arc::clone(self) as Arc<dyn LeaseOwner>, u64::from(slot)) };
        let format = VideoFormat::new(
            Size::new(self.width, self.height),
            PixelFormat::Bgra8,
            ColorSpace::SRGB,
        );
        let surface =
            NativeSurface::d3d_shared_handle(handle, true, D3dApi::D3d11, self.textures.luid());
        // SAFETY: the handle names this slot's texture on `luid`. It stays valid
        // until the lease releases, and the helper does not write it meanwhile.
        Some(unsafe {
            VideoFrame::native(
                format,
                stamp.unwrap_or(Timestamp::ZERO),
                surface,
                SurfaceSync::Ready,
                lease,
            )
        })
    }
}

// SAFETY: `retain` and `release` only adjust the section's lease word for `token`.
// The texture that word guards stays allocated while this value is alive, and the
// helper does not write a slot whose count is above zero.
unsafe impl LeaseOwner for PageSlots {
    fn retain(&self, token: u64) {
        self.ring.lease_add(token as u32);
    }

    fn release(&self, token: u64) {
        self.ring.lease_sub(token as u32);
    }
}

fn grave() -> &'static Mutex<Grave<PageSlots>> {
    static GRAVE: OnceLock<Mutex<Grave<PageSlots>>> = OnceLock::new();
    GRAVE.get_or_init(|| Mutex::new(Grave::default()))
}

/// Drops pages no frame has referenced for [`crate::page_grave::HANDLE_HOLD`], so no
/// importer can still match their handle values (ADR-0157 decision 13). Control thread.
pub fn sweep() {
    lock_grave().sweep(Instant::now());
}

/// The grave, also after a panic while it was held: its list stays sound, and a
/// poisoned lock must not keep a page's textures and handles forever (round-5
/// review).
fn lock_grave() -> std::sync::MutexGuard<'static, Grave<PageSlots>> {
    grave()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// True while the sweeper thread runs.
static SWEEPING: AtomicBool = AtomicBool::new(false);

/// How often the sweeper looks at the grave.
const SWEEP_EVERY: Duration = Duration::from_secs(1);

/// Keeps `slots` until every frame has released it and the handle hold has passed;
/// [`sweep`] drops it then, here and not on the render thread. A sweeper thread runs
/// only while the grave holds a page, so a page that falls back to shared memory, or
/// the last page of a removed source, is still released on time (P13, capture-port
/// invariant 8, round-4 review).
pub fn bury(slots: Arc<PageSlots>) {
    lock_grave().bury(slots, Instant::now());
    if !SWEEPING.swap(true, Ordering::AcqRel) {
        let spawned = std::thread::Builder::new()
            .name("rbs-browser-grave".into())
            .spawn(sweep_until_empty);
        if spawned.is_err() {
            // The next start or bury sweeps instead.
            SWEEPING.store(false, Ordering::Release);
        }
    }
}

fn sweep_until_empty() {
    rbs_crash::report_panics_on_this_thread();
    loop {
        #[allow(clippy::disallowed_methods)] // Its own thread, between sweeps.
        std::thread::sleep(SWEEP_EVERY);
        let mut grave = lock_grave();
        grave.sweep(Instant::now());
        if grave.is_empty() {
            // Under the lock: a page buried after this starts a new sweeper.
            SWEEPING.store(false, Ordering::Release);
            return;
        }
    }
}
