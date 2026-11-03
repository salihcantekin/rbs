//! VideoToolbox adapter for the RBS encoder port (`docs/specs/output-ports.md`): H.264
//! and HEVC on macOS.
//!
//! # Input
//!
//! - `metal`: native `IoSurface` frames from the `rbs-gpu` exporter (ADR-0025),
//!   bi-planar `'420v'`/`'420f'` (NV12) or `'x420'`/`'xf20'` (P010). Each surface is
//!   wrapped once in a `CVPixelBuffer` (`CVPixelBufferCreateWithIOSurface`) and kept in
//!   a small per-session cache keyed by the surface address, so the steady state wraps
//!   nothing and copies nothing (P4). The frame's lease is cloned into a per-frame slot
//!   and released by VideoToolbox's output callback, the moment VideoToolbox no longer
//!   reads the surface; until then the exporter does not write it again.
//! - `system`: NV12 or P010 frames in system memory, copied row by row into a pixel
//!   buffer from VideoToolbox's own pool. This is the fallback path and costs one copy
//!   per frame.
//!
//! This crate never depends on `rbs-gpu` (ADR-0025, decision 3): it reads the handle
//! from the frame payload.
//!
//! # Output
//!
//! Length-prefixed (4-byte) H.264 or HEVC with the `avcC` / `hvcC` record as extradata.
//! VideoToolbox only reveals its parameter sets with the first encoded frame, so
//! `create` encodes one black priming frame, takes the record from its format
//! description and discards the packet; the first real frame is forced to be a
//! keyframe. Should the parameter sets ever change mid-session (they never did in
//! testing), the session fails unrecoverably instead of producing packets that do not
//! match `stream()`.
//!
//! Packets are built in VideoToolbox's output callback (its own threads): one exact-size
//! `Vec` per packet is the one payload allocation the port allows. Each frame owns a
//! slot that the callback claims with one compare-exchange and fills; `drain` takes
//! finished slots in submission order, so `submit`, `drain` and the callback never wait
//! on a lock (P6) and the output order never depends on callback order.
//!
//! # Adapters
//!
//! [`VideoToolboxFactory::new`] (`org.rbs.codec.videotoolbox`) uses hardware encoders
//! only and declares `adapter.hardware`. [`VideoToolboxFactory::with_software`]
//! (`org.rbs.codec.videotoolbox-software`) also offers Apple's software encoders, for
//! machines without a media engine (virtual machines, CI) and as a low-priority
//! fallback; it declares `adapter.hardware = false`.
//!
//! On other platforms the factory compiles and probes to an empty set, so the
//! composition root can register it unconditionally.
//!
//! Licence: the system frameworks are part of macOS; the bindings (`objc2-*`) are
//! MIT OR Apache-2.0 OR Zlib.

/// Id of the hardware-only adapter.
pub const ADAPTER_ID: &str = "org.rbs.codec.videotoolbox";

/// Id of the adapter that also offers Apple's software encoders.
pub const SOFTWARE_ADAPTER_ID: &str = "org.rbs.codec.videotoolbox-software";

/// Frames a session accepts before `submit` returns `Busy`: frames VideoToolbox is
/// still encoding plus packets not yet drained. Also the number of lease slots, so a
/// session holds at most this many exporter surfaces.
pub const FRAMES_IN_FLIGHT: usize = 16;

/// IOSurfaces whose `CVPixelBuffer` wrapper a session keeps. The `rbs-gpu` exporter
/// ring has at most 32 surfaces; a smaller ring never evicts.
pub const SURFACE_CACHE: usize = 32;

/// Counters of one session, for metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionStats {
    /// Frames handed to VideoToolbox.
    pub submitted: u64,
    /// Packets moved out by `drain` or `flush`.
    pub packets: u64,
    /// Frames that produced no packet (dropped by VideoToolbox, or failed).
    pub dropped: u64,
    /// System-memory frames copied into a pixel buffer.
    pub copied: u64,
    /// IOSurfaces wrapped in a new `CVPixelBuffer` (cache misses).
    pub wrapped: u64,
}

#[cfg(target_os = "macos")]
mod mac;

pub use platform::VideoToolboxFactory;

/// The concrete session type, for callers that want its [`SessionStats`].
#[cfg(target_os = "macos")]
pub use mac::VideoToolboxSession;

#[cfg(target_os = "macos")]
mod platform {
    pub use crate::mac::VideoToolboxFactory;
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use rbs_codec::{CreateError, VideoEncoder, VideoEncoderConfig, VideoEncoderFactory};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Factory of the VideoToolbox adapter. Unavailable on this platform: it probes to
    /// an empty set and refuses every session.
    #[derive(Debug, Default)]
    pub struct VideoToolboxFactory {
        software: bool,
    }

    impl VideoToolboxFactory {
        /// The hardware-only adapter.
        pub fn new() -> Self {
            Self { software: false }
        }

        /// The adapter that also offers software encoders.
        pub fn with_software() -> Self {
            Self { software: true }
        }
    }

    impl AdapterFactory for VideoToolboxFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(if self.software {
                crate::SOFTWARE_ADAPTER_ID
            } else {
                crate::ADAPTER_ID
            })
        }

        fn display_name(&self) -> &str {
            "VideoToolbox (unavailable on this platform)"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Ok(CapabilitySet::new())
        }
    }

    impl VideoEncoderFactory for VideoToolboxFactory {
        fn create(&self, _: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError> {
            Err(CreateError::Unsupported {
                what: "VideoToolbox is only available on macOS".into(),
            })
        }
    }
}
