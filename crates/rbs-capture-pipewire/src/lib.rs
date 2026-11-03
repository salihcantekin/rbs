//! PipeWire adapter for the RBS capture port (`docs/specs/capture-port.md`): Linux
//! video nodes delivered zero-copy as DMA-BUFs, or copied into system memory when the
//! source only offers shared memory.
//!
//! # Targets
//!
//! - Cameras: PipeWire video source nodes (`media.class = Video/Source`) exposed by the
//!   session manager (V4L2 and libcamera devices) and virtual sources, listed by
//!   `enumerate`. Their ids are `node:<node.name>`.
//! - Screens and windows: chosen by the user in the XDG desktop portal's dialog
//!   (`pick`, ScreenCast interface). Their ids are `portal:display:<token>` or
//!   `portal:window:<token>`, where the token restores the choice without a dialog.
//!   Restore tokens are single use: after each `start` the session's
//!   `current_target` names the id to store from then on.
//!
//! # How frames travel
//!
//! - `open` checks the request, reads the node's formats and narrows the request to the
//!   first pixel format and memory kind both sides support. It creates nothing that
//!   outlives the call (Standby, P13).
//! - `start` (portal targets: after restoring the cast and opening the PipeWire
//!   connection the portal grants for it) runs a PipeWire main loop on its own thread with one input stream linked
//!   to the node. The stream offers that format, as DMA-BUF with exactly the importer's
//!   modifiers or in shared memory.
//! - On each buffer the stream thread claims a free slot of the lock-free
//!   [`Ring`](rbs_capture::handoff::Ring):
//!   - DMA-BUF: it duplicates the buffer's fd into the slot and keeps the PipeWire
//!     buffer dequeued while the slot is referenced, so the source cannot write into
//!     it while a frame refers to it. The buffer goes back to PipeWire once the slot is
//!     free again.
//!   - Shared memory: it copies the picture into the slot's own buffer (counted in
//!     `cpu_copied`) and returns the PipeWire buffer at once.
//! - `latest_frame` on the render thread takes the newest slot and builds the frame
//!   from the slot's description. It does not allocate, lock or call into the OS.
//!
//! On other platforms the factory compiles and probes as unavailable, so the engine can
//! register it unconditionally.

#[cfg(target_os = "linux")]
mod frames;
#[cfg(target_os = "linux")]
pub(crate) use frames::{Consumer, Picture, Shared};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod portal;

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.capture.pipewire";

/// Slots of the hand-off ring per session. The consumer may hold [`FRAMES_IN_FLIGHT`];
/// one more waits as the latest frame and one is filled by the stream thread.
pub const RING_SLOTS: usize = 5;

/// Frames the consumer may hold at once without stopping delivery.
pub const FRAMES_IN_FLIGHT: usize = RING_SLOTS - 2;

/// PipeWire buffers asked for per stream: one per ring slot, plus room for the source
/// to render into while every slot holds one.
pub const STREAM_BUFFERS: usize = RING_SLOTS + 3;

pub use platform::PipeWireCaptureFactory;

/// Whether a PipeWire daemon socket exists for this process (`PIPEWIRE_REMOTE`,
/// `PIPEWIRE_RUNTIME_DIR` or `XDG_RUNTIME_DIR`). Does not connect. Always `false` on
/// other platforms.
pub fn daemon_present() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::socket_present()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
mod platform {
    pub use crate::linux::PipeWireCaptureFactory;
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use rbs_capture::{CaptureError, CaptureFactory, CaptureRequest, CaptureSession};
    use rbs_capture::{CaptureTarget, TargetKind};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Factory of the PipeWire adapter. Unavailable on this platform.
    #[derive(Debug, Default)]
    pub struct PipeWireCaptureFactory;

    impl PipeWireCaptureFactory {
        /// Creates the factory. Does nothing until used.
        pub fn new() -> Self {
            Self
        }
    }

    impl AdapterFactory for PipeWireCaptureFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "PipeWire"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Err(ProbeError::new("PipeWire needs Linux"))
        }
    }

    impl CaptureFactory for PipeWireCaptureFactory {
        fn enumerate(&self, _kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
            Err(CaptureError::Unsupported("PipeWire needs Linux"))
        }

        fn open(&self, _request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
            Err(CaptureError::Unsupported("PipeWire needs Linux"))
        }
    }
}
