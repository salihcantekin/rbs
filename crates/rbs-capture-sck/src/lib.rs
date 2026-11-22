//! ScreenCaptureKit adapter for the RBS capture port (`docs/specs/capture-port.md`):
//! macOS displays and windows, delivered zero-copy as IOSurfaces.
//!
//! # How frames travel
//!
//! - `open` checks permission, the request and the target and creates nothing
//!   (Standby, P13). It never shows the macOS permission prompt; `request_permission`
//!   does.
//! - `start` creates an `SCStream` for the target with BGRA8 output and a serial
//!   dispatch queue for its sample handler.
//! - On each complete screen sample the handler retains the sample's `CVPixelBuffer`
//!   in a free slot of the lock-free ring and publishes the slot. Holding the pixel
//!   buffer keeps ScreenCaptureKit from reusing its IOSurface while a frame refers to
//!   it; the buffer is released by the handler when the slot is reused, never on the
//!   render thread.
//! - `latest_frame` on the render thread takes the newest slot and returns an
//!   `IoSurface` frame with `SurfaceSync::Ready`. It does not allocate, lock or call
//!   into the OS.
//! - IOSurfaces a frame was handed out with stay retained after `stop` for the grace
//!   of the native surface rules, so an importer that caches them by pointer never sees
//!   a reused pointer.
//!
//! - `thumbnail` grabs one frame with `SCScreenshotManager` (macOS 14, else
//!   `Unsupported`), scaled by ScreenCaptureKit to fit and converted to sRGB RGBA8. It
//!   starts no stream, shows no capture indicator and never prompts for permission.
//!   `region_grab` is that same grab with a long side of at most 960.
//!
//! The importer side is `docs/specs/gpu-compositor.md`, "Native import" (Metal).
//!
//! - When the target is resized, the sample handler reconfigures the stream so later
//!   frames come at the new size instead of scaled into the old one.
//! - Window captures draw the cursor only while no other window covers it: a watcher
//!   thread, alive only while such a capture is Active, turns the stream's cursor off
//!   while the pointer is over a window stacked above the target (`cursor` module).
//! - While the host's mapping switch is on, the same watcher thread reads the target's
//!   desktop frame and the covered test, and each frame carries the desktop to picture
//!   mapping (`latest_mapping`, ADR-0165 decision 5, `geometry` module).
//!
//! ScreenCaptureKit is linked strongly by its bindings, so binaries using this crate
//! need macOS 12.3 as their minimum deployment target; the probe's version check only
//! guards against a wrong deployment setting. On other platforms the factory compiles
//! and probes as unavailable, so the engine can register it unconditionally.

mod cursor;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod geometry;
#[cfg(target_os = "macos")]
mod mac;

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.capture.sck";

/// Slots of the hand-off ring per session. The consumer may hold [`FRAMES_IN_FLIGHT`];
/// one more waits as the latest frame and one is filled by the sample handler.
pub const RING_SLOTS: usize = 5;

/// Frames the consumer may hold at once without stopping delivery.
pub const FRAMES_IN_FLIGHT: usize = RING_SLOTS - 2;

/// Surfaces in ScreenCaptureKit's own pool (`queueDepth`, at most 8). Each ring slot
/// keeps one pixel buffer, one more is being handed to the sample handler, and one is
/// left for the OS to render into. Free slots keep their buffer until reused; if the
/// hardware run shows drops at 120 Hz, free slots can release theirs early.
pub const QUEUE_DEPTH: usize = RING_SLOTS + 2;

pub use platform::SckCaptureFactory;

/// The main display's framebuffer and Video-settings scale, from Core Graphics.
/// Does not open shareable content. macOS only.
#[cfg(target_os = "macos")]
pub use mac::main_display;

#[cfg(target_os = "macos")]
mod platform {
    pub use crate::mac::SckCaptureFactory;
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use rbs_capture::{CaptureError, CaptureFactory, CaptureRequest, CaptureSession};
    use rbs_capture::{CaptureTarget, TargetKind};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Factory of the ScreenCaptureKit adapter. Unavailable on this platform.
    #[derive(Debug, Default)]
    pub struct SckCaptureFactory;

    impl SckCaptureFactory {
        /// Creates the factory. Does nothing until used.
        pub fn new() -> Self {
            Self
        }
    }

    impl AdapterFactory for SckCaptureFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "ScreenCaptureKit"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Err(ProbeError::new("ScreenCaptureKit needs macOS"))
        }
    }

    impl CaptureFactory for SckCaptureFactory {
        fn enumerate(&self, _kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
            Err(CaptureError::Unsupported("ScreenCaptureKit needs macOS"))
        }

        fn open(&self, _request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
            Err(CaptureError::Unsupported("ScreenCaptureKit needs macOS"))
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) use rbs_capture::handoff::SurfaceConsumer as Consumer;
