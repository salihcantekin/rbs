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
//!   slot of the lock-free [`Ring`] and publishes the slot. Holding the pixel buffer
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

use std::sync::Arc;

use rbs_capture::handoff::Ring;
use rbs_core::media::{
    Lease, LeaseOwner, NativeSurface, Size, SurfaceSync, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;

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

/// Builds frames from published ring slots on the render thread.
///
/// Platform independent so it is tested everywhere; the macOS producer owns the
/// surfaces the ring names.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct Consumer<O: AsRef<Ring> + LeaseOwner + 'static> {
    shared: Arc<O>,
    owner: Arc<dyn LeaseOwner>,
    format: VideoFormat,
    delivered: u64,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl<O: AsRef<Ring> + LeaseOwner + 'static> Consumer<O> {
    /// A consumer of `shared`'s ring delivering frames of `format`.
    ///
    /// # Safety
    ///
    /// Every value the ring names must be a live `IOSurfaceRef` of the pixel format and
    /// colour of `format` and of the size published with it, complete when published.
    /// Each stays valid, and is not written, while its slot is referenced (a lease of
    /// `shared` is held).
    pub(crate) unsafe fn new(shared: Arc<O>, format: VideoFormat) -> Self {
        let owner: Arc<dyn LeaseOwner> = Arc::clone(&shared) as Arc<dyn LeaseOwner>;
        Self {
            shared,
            owner,
            format,
            delivered: 0,
        }
    }

    /// The format of the last delivered frame (the open format before the first).
    pub(crate) fn format(&self) -> VideoFormat {
        self.format
    }

    /// Frames returned by [`Consumer::next`].
    pub(crate) fn delivered(&self) -> u64 {
        self.delivered
    }

    /// The newest published frame, if one arrived since the last call. Does not
    /// allocate, free, lock or block.
    pub(crate) fn next(&mut self) -> Option<VideoFrame> {
        let ring = (*self.shared).as_ref();
        let (slot, meta) = ring.take()?;
        let Some(surface) = ring.handle(slot) else {
            ring.release(slot as u64);
            return None;
        };
        if (self.format.size.width, self.format.size.height) != (meta.width, meta.height) {
            self.format = VideoFormat::new(
                Size::new(meta.width, meta.height),
                self.format.pixel_format,
                self.format.color,
            );
        }
        // SAFETY: `take` moved exactly one reference of `slot` to this call; the lease
        // adopts it and releases it when the frame and its clones are dropped.
        let lease = unsafe { Lease::adopt(Arc::clone(&self.owner), slot as u64) };
        let surface = NativeSurface::io_surface(surface as usize);
        // SAFETY: guaranteed by the contract of `Consumer::new`: the IOSurface is live,
        // complete and of `self.format`, and stays valid and unwritten while the lease
        // holds the slot.
        let frame = unsafe {
            VideoFrame::native(
                self.format,
                Timestamp::from_nanos(meta.timestamp),
                surface,
                SurfaceSync::Ready,
                lease,
            )
        };
        self.delivered += 1;
        Some(
            frame
                .with_sequence(meta.sequence)
                .with_source_time(meta.source_time),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_capture::handoff::FrameMeta;
    use rbs_core::media::{ColorSpace, MemoryKind, PixelFormat};

    /// A lease owner over a bare ring, for tests whose values are never dereferenced.
    struct Owner(Ring);

    impl AsRef<Ring> for Owner {
        fn as_ref(&self) -> &Ring {
            &self.0
        }
    }

    // SAFETY: delegates to the ring's exact counting; the made-up surface values are
    // never dereferenced, so there is no surface whose validity the counts protect.
    unsafe impl LeaseOwner for Owner {
        fn retain(&self, token: u64) {
            self.0.retain(token);
        }
        fn release(&self, token: u64) {
            self.0.release(token);
        }
    }

    fn meta(sequence: u64) -> FrameMeta {
        FrameMeta::new(1_000 + sequence, sequence, 7_000 + sequence, 1280, 720)
    }

    fn consumer(ring: &Arc<Owner>) -> Consumer<Owner> {
        let format = VideoFormat::new(
            Size::new(1280, 720),
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        );
        // SAFETY: the test never dereferences the made-up surface values.
        unsafe { Consumer::new(Arc::clone(ring), format) }
    }

    #[test]
    fn frames_carry_the_surface_format_and_metadata() {
        let ring = Arc::new(Owner(Ring::new(&[0x1000, 0x2000, 0x3000])));
        let mut consumer = consumer(&ring);
        assert!(consumer.next().is_none());
        let Some(slot) = ring.0.claim() else {
            panic!("free slot")
        };
        ring.0.publish(slot, meta(4));
        let Some(frame) = consumer.next() else {
            panic!("a frame")
        };
        assert_eq!(frame.memory_kind(), MemoryKind::Metal);
        assert_eq!(frame.format().pixel_format, PixelFormat::Nv12);
        assert_eq!(frame.format().color, ColorSpace::BT709_LIMITED);
        assert_eq!(frame.timestamp().as_nanos(), 1_004);
        assert_eq!(frame.sequence(), 4);
        let Some((surface, sync, lease)) = frame.into_native() else {
            panic!("native payload")
        };
        assert_eq!(surface, NativeSurface::io_surface(0x1000 * (slot + 1)));
        assert_eq!(sync, SurfaceSync::Ready);
        assert!(!ring.0.is_idle());
        drop(lease);
        assert!(ring.0.is_idle());
        assert_eq!(consumer.delivered(), 1);
        assert_eq!(ring.0.misuses(), 0);
    }

    #[test]
    fn next_does_not_allocate() {
        let ring = Arc::new(Owner(Ring::new(&[1, 2, 3])));
        let mut consumer = consumer(&ring);
        if let Some(slot) = ring.0.claim() {
            ring.0.publish(slot, meta(0));
        }
        drop(consumer.next());
        for seq in 1..100 {
            if let Some(slot) = ring.0.claim() {
                ring.0.publish(slot, meta(seq));
            }
            let info = allocation_counter::measure(|| {
                drop(std::hint::black_box(consumer.next()));
            });
            assert_eq!(info.count_total, 0, "frame {seq} allocated");
        }
        assert!(ring.0.is_idle());
    }
}
