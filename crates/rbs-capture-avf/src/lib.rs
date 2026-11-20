//! AVFoundation adapter for the RBS capture port (`docs/specs/capture-port.md`): macOS
//! cameras, delivered zero-copy as IOSurfaces.
//!
//! # How frames travel
//!
//! - `enumerate` lists the video devices with their formats; no device is opened.
//! - `open` checks permission, the request and the device and picks the device format
//!   and frame rate ([`rbs_capture::camera::choose_mode`]). It creates no capture
//!   session, so the camera light stays off (Standby, P13). It never shows the macOS
//!   camera prompt; `request_permission` does.
//! - `start` builds an `AVCaptureSession` with the device input and an
//!   `AVCaptureVideoDataOutput` that delivers NV12 (`'420v'`) or BGRA pixel buffers,
//!   whichever the request asked for first, sets the device format and frame rate and
//!   starts running.
//! - On each sample the output's delegate retains the sample's `CVPixelBuffer` in a free
//!   slot of the lock-free ring and publishes the slot. Holding the pixel buffer
//!   keeps AVFoundation from reusing its IOSurface while a frame refers to it. Buffers of
//!   free slots are given back to AVFoundation's pool on the next sample, on the
//!   delegate's queue, so a held frame never starves the pool and the render thread
//!   never releases one.
//! - `latest_frame` on the render thread takes the newest slot and returns an
//!   `IoSurface` frame with `SurfaceSync::Ready`. It does not allocate, lock or call
//!   into the OS.
//! - IOSurfaces a frame was handed out with stay retained after their last use for the
//!   grace of the native surface rules, so an importer that caches them by pointer never
//!   sees a reused pointer.
//!
//! The importer side is `docs/specs/gpu-compositor.md`, "Native import" (Metal): BGRA
//! surfaces are aliased, NV12 surfaces are decoded on the GPU.
//!
//! On other platforms the factory compiles and probes as unavailable, so the engine can
//! register it unconditionally.

#[cfg(target_os = "macos")]
mod mac;

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.capture.avf";

/// Slots of the hand-off ring per session. The consumer may hold [`FRAMES_IN_FLIGHT`];
/// one more waits as the latest frame and one is filled by the delegate.
pub const RING_SLOTS: usize = 5;

/// Frames the consumer may hold at once without stopping delivery.
pub const FRAMES_IN_FLIGHT: usize = RING_SLOTS - 2;

pub use platform::AvfCaptureFactory;

#[cfg(target_os = "macos")]
mod platform {
    pub use crate::mac::AvfCaptureFactory;
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use rbs_capture::{CaptureError, CaptureFactory, CaptureRequest, CaptureSession};
    use rbs_capture::{CaptureTarget, TargetKind};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Factory of the AVFoundation camera adapter. Unavailable on this platform.
    #[derive(Debug, Default)]
    pub struct AvfCaptureFactory;

    impl AvfCaptureFactory {
        /// Creates the factory. Does nothing until used.
        pub fn new() -> Self {
            Self
        }
    }

    impl AdapterFactory for AvfCaptureFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "AVFoundation cameras"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Err(ProbeError::new("AVFoundation needs macOS"))
        }
    }

    impl CaptureFactory for AvfCaptureFactory {
        fn enumerate(&self, _kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
            Err(CaptureError::Unsupported("AVFoundation needs macOS"))
        }

        fn open(&self, _request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
            Err(CaptureError::Unsupported("AVFoundation needs macOS"))
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) use rbs_capture::handoff::SurfaceConsumer as Consumer;
