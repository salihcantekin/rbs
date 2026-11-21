//! Studio side of a macOS browser page's slot surfaces (ADR-0167).
//!
//! The surfaces live in [`PageSlots`]; a frame's lease holds that `Arc`. The helper
//! fetches them once through [`Offer`], a Mach service the supervisor polls.

use std::sync::Arc;

use rbs_browser_runner::{HandoverPoll, SurfaceHandover};
use rbs_browser_shm::FrameRing;
use rbs_core::media::{
    ColorSpace, Lease, LeaseOwner, NativeSurface, PixelFormat, Size, SurfaceSync, VideoFormat,
    VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_gpu::browser_surfaces::PageSurfaces;

/// Surfaces plus the lease counts in the frame section.
pub struct PageSlots {
    ring: FrameRing,
    surfaces: PageSurfaces,
    width: u32,
    height: u32,
}

impl PageSlots {
    /// Opens the section `name` and keeps `surfaces` alive with it.
    pub fn open(
        name: &str,
        width: u32,
        height: u32,
        surfaces: PageSurfaces,
    ) -> std::io::Result<Arc<Self>> {
        let ring = FrameRing::open(name, width, height)?;
        Ok(Arc::new(Self {
            ring,
            surfaces,
            width,
            height,
        }))
    }

    /// A frame on slot `slot`, taking over the lease the supervisor's gate already
    /// holds. The helper published it only after its copy completed, so the sync is
    /// [`SurfaceSync::Ready`] (decision 6). `None`, with the lease given back, for a
    /// slot this page has no surface for.
    pub fn picture(self: &Arc<Self>, slot: u32, stamp: Option<Timestamp>) -> Option<VideoFrame> {
        let Some(surface) = self.surfaces.surface(slot as usize) else {
            self.ring.lease_sub(slot);
            return None;
        };
        // SAFETY: the gate counted the one reference this lease releases. The surface
        // stays alive while this `Arc` lives, which the lease holds, and the helper
        // claims a slot only while its lease count is zero.
        let lease =
            unsafe { Lease::adopt(Arc::clone(self) as Arc<dyn LeaseOwner>, u64::from(slot)) };
        let format = VideoFormat::new(
            Size::new(self.width, self.height),
            PixelFormat::Bgra8,
            ColorSpace::SRGB,
        );
        // SAFETY: `surface` is this slot's `IOSurfaceRef`. It stays valid until the
        // lease releases, and the helper does not write it meanwhile.
        Some(unsafe {
            VideoFrame::native(
                format,
                stamp.unwrap_or(Timestamp::ZERO),
                NativeSurface::io_surface(surface),
                SurfaceSync::Ready,
                lease,
            )
        })
    }
}

// SAFETY: `retain` and `release` only adjust the section's lease word for `token`.
// The surface that word guards stays allocated while this value is alive, and the
// helper does not write a slot whose count is above zero.
unsafe impl LeaseOwner for PageSlots {
    fn retain(&self, token: u64) {
        self.ring.lease_add(token as u32);
    }

    fn release(&self, token: u64) {
        self.ring.lease_sub(token as u32);
    }
}

/// A page's surfaces on offer to its helper (ADR-0167 decision 3).
#[derive(Debug)]
pub struct Offer(rbs_mach::browser::Handover);

impl Offer {
    /// Offers `surfaces` to the helper `pid` on a fresh service. `None` when the
    /// service cannot be made: the page uses shared memory.
    pub fn new(surfaces: &PageSurfaces, pid: u32) -> Option<Self> {
        let ports = surfaces.mach_ports().ok()?;
        rbs_mach::browser::Handover::new(ports, pid).ok().map(Self)
    }

    /// The `Configure` GPU block for this offer.
    pub fn share(&self, registry_id: u64) -> rbs_browser_ipc::GpuShare {
        rbs_browser_ipc::GpuShare::Metal(rbs_browser_ipc::MetalShare {
            registry_id,
            service: self.0.name().to_owned(),
            token: self.0.token(),
            count: self.0.count(),
        })
    }
}

impl SurfaceHandover for Offer {
    fn poll(&mut self) -> HandoverPoll {
        match self.0.poll() {
            rbs_mach::browser::State::Waiting => HandoverPoll::Waiting,
            rbs_mach::browser::State::Done => HandoverPoll::Done,
            rbs_mach::browser::State::Failed => HandoverPoll::Failed,
        }
    }

    fn close(&mut self) {
        self.0.close();
    }
}

/// The macOS GPU page path against the real CEF runner (ADR-0167 decision 10).
/// Ignored: it needs the Mac's GPU, a built runner and the engine.
///
/// ```text
/// RBS_TEST_CEF_RUNNER=<rbs-browser-cef.app/Contents/MacOS/rbs-browser-cef> \
/// RBS_TEST_CEF_RUNTIME=<add-ons root>/org.rbs.browser-engine/<version> \
/// cargo test -p rbs-app --release --lib browser_metal -- --ignored --nocapture
/// ```
#[cfg(test)]
mod device_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::time::{Duration, Instant};

    use rbs_browser_ipc::Configure;
    use rbs_browser_runner::{FenceView, runtime_env, spawn_helper_prepared};
    use rbs_browser_shm::SlotLayout;

    use super::*;

    struct NoImports;

    impl FenceView for NoImports {
        fn completed(&self) -> u64 {
            0
        }
    }

    /// A page that paints one solid colour per frame, a new one every frame.
    const PAGE: &str = "data:text/html,<body style='margin:0;background:%23000'><script>\
        let n=0;function f(){n++;document.body.style.background=\
        'rgb('+(n*37%25256)+','+(n*101%25256)+','+(n*53%25256)+')';\
        requestAnimationFrame(f)}f()</script></body>";

    #[test]
    #[ignore = "slow: real CEF on the Mac's GPU; needs RBS_TEST_CEF_RUNNER and RBS_TEST_CEF_RUNTIME"]
    fn slow_gpu_pages_arrive_whole_and_change() {
        let (Some(runner), Some(runtime)) = (
            std::env::var_os("RBS_TEST_CEF_RUNNER"),
            std::env::var_os("RBS_TEST_CEF_RUNTIME"),
        ) else {
            return;
        };
        let secs: u64 = std::env::var("RBS_TEST_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20);
        let runtime = std::path::PathBuf::from(runtime);
        let library = rbs_browser_ipc::runtime::read(&runtime)
            .map_or_else(|_| runtime.join("Release"), |rt| rt.library_dir);
        let ctx = rbs_gpu::GpuContext::new(&rbs_gpu::GpuRequest::default()).unwrap();
        assert_eq!(rbs_gpu::browser_surfaces::metal_device_count(), 1);
        let registry = rbs_gpu::browser_surfaces::metal_registry_id(&ctx).unwrap();
        let (w, h) = (1280, 720);
        let surfaces = PageSurfaces::create(w, h, 4).unwrap();
        let config = Configure {
            url: PAGE.into(),
            width: w,
            height: h,
            fps: 30,
            transparent: false,
            frames: String::new(),
            sound: String::new(),
            sample_rate: 0,
            page: rbs_browser_ipc::PageOptions::default(),
            gpu: None,
        };
        let mut offer = None;
        let mut child = spawn_helper_prepared(
            std::path::Path::new(&runner),
            &[],
            &runtime_env(&runtime, &library),
            &config,
            SlotLayout::Rgba8Straight,
            |pid| {
                let made = Offer::new(&surfaces, pid)?;
                let share = made.share(registry);
                offer = Some(made);
                Some(share)
            },
        )
        .unwrap();
        child.set_fenceless_gate(Box::new(NoImports), 30);
        child.set_handover(Box::new(offer.unwrap()));
        let start = Instant::now();
        let (mut pictures, mut torn, mut changes) = (0u32, 0u32, 0u32);
        let mut last: Option<[u8; 4]> = None;
        while start.elapsed() < Duration::from_secs(secs) {
            assert!(child.check_heartbeat(Instant::now()), "heartbeat lost");
            assert!(child.process_alive(), "helper exited {:?}", child.exit_cause());
            if let Some(p) = child.take_gpu(Instant::now()) {
                pictures += 1;
                let px = surfaces.sample(p.slot as usize, 64).unwrap();
                if px.iter().any(|q| q != &px[0]) {
                    torn += 1;
                }
                if last != Some(px[0]) {
                    changes += 1;
                }
                last = Some(px[0]);
                child.release_gpu(p.slot);
            }
            assert!(!child.gpu_failed(), "fell back");
            std::thread::yield_now();
        }
        rbs_crash::errln!("pictures {pictures}, torn {torn}, changed {changes} in {secs} s");
        child.close(rbs_browser_runner::STOP_GRACE);
        assert!(pictures > 25 * secs as u32, "too few pictures: {pictures}");
        assert_eq!(torn, 0, "torn pictures");
        assert!(changes * 10 >= pictures * 9, "pictures did not change: {changes}");
    }
}
