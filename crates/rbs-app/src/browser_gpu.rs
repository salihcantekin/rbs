//! Studio side of a browser page's shared textures (ADR-0109 decision 8, ADR-0157).
//!
//! The textures live in [`PageSlots`]. A frame's lease holds that `Arc`, and so does
//! a grave the control thread drains, so the last Direct3D release is not on the
//! render thread.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

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
    if let Ok(mut grave) = grave().lock() {
        grave.sweep(Instant::now());
    }
}

/// Keeps `slots` until every frame has released it and the handle hold has passed;
/// [`sweep`] drops it then, here and not on the render thread.
pub fn bury(slots: Arc<PageSlots>) {
    if let Ok(mut grave) = grave().lock() {
        grave.bury(slots, Instant::now());
    }
}
