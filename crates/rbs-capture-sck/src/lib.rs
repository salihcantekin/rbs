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
//!   in a free slot of the lock-free [`Ring`] and publishes the slot. Holding the pixel
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
//! The importer side is `docs/specs/gpu-compositor.md`, "Native import" (Metal).
//!
//! - When the target is resized, the sample handler reconfigures the stream so later
//!   frames come at the new size instead of scaled into the old one.
//! - Window captures draw the cursor only while no other window covers it: a watcher
//!   thread, alive only while such a capture is Active, turns the stream's cursor off
//!   while the pointer is over a window stacked above the target (`cursor` module).
//!
//! ScreenCaptureKit is linked strongly by its bindings, so binaries using this crate
//! need macOS 12.3 as their minimum deployment target; the probe's version check only
//! guards against a wrong deployment setting. On other platforms the factory compiles
//! and probes as unavailable, so the engine can register it unconditionally.

use std::sync::Arc;

use rbs_capture::handoff::Ring;
use rbs_core::media::{
    Lease, LeaseOwner, NativeSurface, Size, SurfaceSync, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;

mod cursor;
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
    /// A consumer of `shared`'s ring.
    ///
    /// # Safety
    ///
    /// Every value the ring names must be a live `IOSurfaceRef` of a BGRA8 surface of
    /// the size published with it, complete when published. Each stays valid, and is
    /// not written, while its slot is referenced (a lease of `shared` is held).
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

    impl std::ops::Deref for Owner {
        type Target = Ring;
        fn deref(&self) -> &Ring {
            &self.0
        }
    }

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

    fn meta(sequence: u64, width: u32) -> FrameMeta {
        FrameMeta::new(1_000 + sequence, sequence, 7_000 + sequence, width, 720)
    }

    fn consumer(ring: &Arc<Owner>) -> Consumer<Owner> {
        let format = VideoFormat::new(Size::new(1280, 720), PixelFormat::Bgra8, ColorSpace::SRGB);
        // SAFETY: the test never dereferences the made-up surface values.
        unsafe { Consumer::new(Arc::clone(ring), format) }
    }

    #[test]
    fn frames_carry_the_surface_and_metadata() {
        let ring = Arc::new(Owner(Ring::new(&[0x1000, 0x2000, 0x3000])));
        let mut consumer = consumer(&ring);
        assert!(consumer.next().is_none());
        let Some(slot) = ring.claim() else {
            panic!("free slot")
        };
        ring.publish(slot, meta(4, 1280));
        let Some(frame) = consumer.next() else {
            panic!("a frame")
        };
        assert_eq!(frame.memory_kind(), MemoryKind::Metal);
        assert_eq!(frame.timestamp().as_nanos(), 1_004);
        assert_eq!(frame.sequence(), 4);
        let Some((surface, sync, lease)) = frame.into_native() else {
            panic!("native payload")
        };
        assert_eq!(surface, NativeSurface::io_surface(0x1000 * (slot + 1)));
        assert_eq!(sync, SurfaceSync::Ready);
        assert!(!ring.is_idle());
        drop(lease);
        assert!(ring.is_idle());
        assert_eq!(consumer.delivered(), 1);
    }

    #[test]
    fn size_follows_published_frames() {
        let ring = Arc::new(Owner(Ring::new(&[1, 2, 3])));
        let mut consumer = consumer(&ring);
        let Some(slot) = ring.claim() else {
            panic!("free slot")
        };
        ring.publish(slot, meta(0, 800));
        let Some(frame) = consumer.next() else {
            panic!("a frame")
        };
        assert_eq!(frame.format().size, Size::new(800, 720));
        assert_eq!(consumer.format().size, Size::new(800, 720));
    }

    #[test]
    fn next_does_not_allocate() {
        let ring = Arc::new(Owner(Ring::new(&[1, 2, 3])));
        let mut consumer = consumer(&ring);
        if let Some(slot) = ring.claim() {
            ring.publish(slot, meta(0, 1280));
        }
        drop(consumer.next());
        for seq in 1..100 {
            if let Some(slot) = ring.claim() {
                ring.publish(slot, meta(seq, 1280));
            }
            let info = allocation_counter::measure(|| {
                drop(std::hint::black_box(consumer.next()));
            });
            assert_eq!(info.count_total, 0, "frame {seq} allocated");
        }
        assert!(ring.is_idle());
    }
}
