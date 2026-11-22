//! The render-thread consumer of a ring whose values are IOSurface pointers.
//!
//! ScreenCaptureKit and AVFoundation both publish that way. This type builds the
//! `Ready` frame. It does not retain the surface: the adapter's lease owner does.

use std::sync::Arc;

use rbs_core::media::{
    Lease, LeaseOwner, NativeSurface, Size, SurfaceSync, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;

use super::{FrameMeta, Ring};

/// Builds frames from published ring slots on the render thread.
///
/// The macOS producer owns the surfaces the ring names. This type never dereferences
/// the pointer.
pub struct SurfaceConsumer<O: AsRef<Ring> + LeaseOwner + 'static> {
    shared: Arc<O>,
    owner: Arc<dyn LeaseOwner>,
    format: VideoFormat,
    delivered: u64,
    last_slot: Option<usize>,
}

impl<O: AsRef<Ring> + LeaseOwner + 'static> SurfaceConsumer<O> {
    /// A consumer of `shared`'s ring delivering frames of `format`.
    ///
    /// # Safety
    ///
    /// Every value the ring names must be a live `IOSurfaceRef` of the pixel format
    /// and colour of `format` and of the size published with it, complete when
    /// published. Each stays valid, and is not written, while its slot is referenced
    /// (a lease of `shared` is held).
    pub unsafe fn new(shared: Arc<O>, format: VideoFormat) -> Self {
        let owner: Arc<dyn LeaseOwner> = Arc::clone(&shared) as Arc<dyn LeaseOwner>;
        Self {
            shared,
            owner,
            format,
            delivered: 0,
            last_slot: None,
        }
    }

    /// The format of the last delivered frame (the open format before the first).
    pub fn format(&self) -> VideoFormat {
        self.format
    }

    /// Frames returned by [`SurfaceConsumer::next`].
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// The ring slot of the frame [`SurfaceConsumer::next`] returned last, so the
    /// adapter can read what it stored beside that slot (ADR-0165 picture mapping).
    /// The slot's contents stay put while that frame is held.
    pub fn last_slot(&self) -> Option<usize> {
        self.last_slot
    }

    /// The newest published frame, if one arrived since the last call. Does not
    /// allocate, free, lock or block. Named `next` because both capture adapters
    /// already call it that way; it is not an iterator.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<VideoFrame> {
        let ring = (*self.shared).as_ref();
        let (slot, meta) = ring.take()?;
        let Some(surface) = ring.handle(slot) else {
            ring.release(slot as u64);
            return None;
        };
        self.note_size(meta);
        // SAFETY: `take` moved exactly one reference of `slot` to this call; the lease
        // adopts it and releases it when the frame and its clones are dropped.
        let lease = unsafe { Lease::adopt(Arc::clone(&self.owner), slot as u64) };
        let surface = NativeSurface::io_surface(surface as usize);
        // SAFETY: guaranteed by the contract of `SurfaceConsumer::new`.
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
        self.last_slot = Some(slot);
        Some(
            frame
                .with_sequence(meta.sequence)
                .with_source_time(meta.source_time),
        )
    }

    fn note_size(&mut self, meta: FrameMeta) {
        if (self.format.size.width, self.format.size.height) != (meta.width, meta.height) {
            self.format = VideoFormat::new(
                Size::new(meta.width, meta.height),
                self.format.pixel_format,
                self.format.color,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn consumer(ring: &Arc<Owner>, format: PixelFormat) -> SurfaceConsumer<Owner> {
        let color = if format == PixelFormat::Nv12 {
            ColorSpace::BT709_LIMITED
        } else {
            ColorSpace::SRGB
        };
        let format = VideoFormat::new(Size::new(1280, 720), format, color);
        // SAFETY: the test never dereferences the made-up surface values.
        unsafe { SurfaceConsumer::new(Arc::clone(ring), format) }
    }

    #[test]
    fn frames_carry_the_surface_and_metadata() {
        let ring = Arc::new(Owner(Ring::new(&[0x1000, 0x2000, 0x3000])));
        let mut consumer = consumer(&ring, PixelFormat::Bgra8);
        assert!(consumer.next().is_none());
        let Some(slot) = ring.claim() else {
            panic!("free slot");
        };
        ring.publish(slot, meta(4, 1280));
        let Some(frame) = consumer.next() else {
            panic!("a frame");
        };
        assert_eq!(frame.memory_kind(), MemoryKind::Metal);
        assert_eq!(frame.timestamp().as_nanos(), 1_004);
        assert_eq!(frame.sequence(), 4);
        let Some((surface, sync, lease)) = frame.into_native() else {
            panic!("native payload");
        };
        assert_eq!(surface, NativeSurface::io_surface(0x1000 * (slot + 1)));
        assert_eq!(sync, SurfaceSync::Ready);
        assert!(!ring.is_idle());
        drop(lease);
        assert!(ring.is_idle());
        assert_eq!(consumer.delivered(), 1);
        assert_eq!(ring.misuses(), 0);
    }

    #[test]
    fn an_nv12_frame_keeps_its_format_and_size_follows_the_publish() {
        let ring = Arc::new(Owner(Ring::new(&[1, 2, 3])));
        let mut consumer = consumer(&ring, PixelFormat::Nv12);
        let Some(slot) = ring.claim() else {
            panic!("free slot");
        };
        ring.publish(slot, meta(0, 800));
        let Some(frame) = consumer.next() else {
            panic!("a frame");
        };
        assert_eq!(frame.format().pixel_format, PixelFormat::Nv12);
        assert_eq!(frame.format().color, ColorSpace::BT709_LIMITED);
        assert_eq!(frame.format().size, Size::new(800, 720));
        assert_eq!(consumer.format().size, Size::new(800, 720));
    }

    #[test]
    fn next_does_not_allocate() {
        let ring = Arc::new(Owner(Ring::new(&[1, 2, 3])));
        let mut consumer = consumer(&ring, PixelFormat::Bgra8);
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
