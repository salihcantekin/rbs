//! Ring slots and the render-thread consumer: the platform-neutral half of the
//! hand-off between the stream thread and `latest_frame`.

use std::cell::UnsafeCell;
use std::sync::Arc;

use rbs_capture::handoff::Ring;

use crate::RING_SLOTS;
use rbs_core::media::{
    Bytes, ColorSpace, DmaBufSurface, Lease, LeaseOwner, NativeSurface, PixelFormat, PlaneLayout,
    Size, SurfaceSync, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;

/// What a ring slot holds: the picture of the frame published from it.
pub(crate) enum Picture {
    /// Nothing yet.
    Empty,
    /// A single-plane DMA-BUF. `fd` owns the duplicated fd the surface names; `None`
    /// only in tests, which never import the surface.
    DmaBuf {
        surface: DmaBufSurface,
        #[allow(dead_code, reason = "owns the fd the surface names; closed on drop")]
        fd: Option<std::os::fd::OwnedFd>,
    },
    /// A copy in system memory, one packed RGBA-order plane.
    System { data: Bytes, stride: usize },
}

/// One slot's picture and pixel format.
pub(crate) struct Slot {
    pub(crate) picture: Picture,
    pub(crate) pixel_format: PixelFormat,
}

/// The ring and the pictures its slots hold. Frames hold an `Arc` of it.
pub(crate) struct Shared {
    pub(crate) ring: Ring,
    slots: Box<[UnsafeCell<Slot>]>,
}

// SAFETY: a slot's cell is written only by the stream thread while it holds the slot's
// claim (reference count 1, not yet published), and read only by the consumer after
// `take` handed it a reference. The ring's protocol makes these exclusive: a claimed,
// unpublished slot is never taken, and a slot is claimed again only once every
// reference is released. The ring's AcqRel publish and take order the write before the
// read; the lease's Release decrement and the claim's Acquire order the read before the
// next write. The contents (`OwnedFd`, `Bytes`, plain data) are `Send`.
unsafe impl Sync for Shared {}

impl Shared {
    pub(crate) fn new() -> Self {
        Self {
            ring: Ring::new(&[0; RING_SLOTS]),
            slots: (0..RING_SLOTS)
                .map(|_| {
                    UnsafeCell::new(Slot {
                        picture: Picture::Empty,
                        pixel_format: PixelFormat::Bgra8,
                    })
                })
                .collect(),
        }
    }

    /// The slot's contents, for the stream thread.
    ///
    /// # Safety
    ///
    /// The caller holds `slot`'s claim (returned by `Ring::claim`, not yet published or
    /// abandoned) and no other reference to the slot exists.
    #[allow(
        clippy::mut_from_ref,
        reason = "interior mutability through UnsafeCell"
    )]
    pub(crate) unsafe fn slot_mut(&self, slot: usize) -> Option<&mut Slot> {
        // SAFETY: exclusive by the caller's contract and the ring protocol (see the
        // `Sync` impl).
        self.slots.get(slot).map(|cell| unsafe { &mut *cell.get() })
    }
}

// SAFETY: delegates to `Ring`, whose counting is exact when its calling protocol is
// followed (`rbs_capture::handoff::Ring`): the stream thread publishes, abandons and
// replaces only slots it claimed, and every lease releases exactly the reference it
// adopted or retained. A slot's picture (duplicated fds, or a system copy) is owned by
// `slots` and only replaced while the slot is claimed, so it stays valid and unchanged
// while any lease (which holds an `Arc<Shared>`) refers to it; the PipeWire buffer of a
// DMA-BUF slot stays dequeued while the slot is referenced. Neither method locks.
unsafe impl LeaseOwner for Shared {
    fn retain(&self, token: u64) {
        self.ring.retain(token);
    }

    fn release(&self, token: u64) {
        self.ring.release(token);
    }
}

/// Builds frames from published slots on the render thread.
pub(crate) struct Consumer {
    shared: Arc<Shared>,
    owner: Arc<dyn LeaseOwner>,
    format: VideoFormat,
    delivered: u64,
}

impl Consumer {
    /// A consumer of `shared`'s ring.
    ///
    /// # Safety
    ///
    /// Every published DMA-BUF slot describes a complete, single-plane picture of the
    /// published size in its pixel format, valid and unwritten while the slot is
    /// referenced.
    pub(crate) unsafe fn new(shared: Arc<Shared>, format: VideoFormat) -> Self {
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

    /// The shared ring.
    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// The newest published frame, if one arrived since the last call. Does not
    /// allocate, free, lock or block.
    pub(crate) fn next(&mut self) -> Option<VideoFrame> {
        let ring = &self.shared.ring;
        let (slot, meta) = ring.take()?;
        // SAFETY: `take` moved exactly one reference of `slot` to this call; the lease
        // adopts it and releases it when the frame and its clones are dropped.
        let lease = unsafe { Lease::adopt(Arc::clone(&self.owner), slot as u64) };
        let cell = self.shared.slots.get(slot)?;
        // SAFETY: as above, the slot is read-only while referenced.
        let contents = unsafe { &*cell.get() };
        let size = Size::new(meta.width, meta.height);
        let format = VideoFormat::new(size, contents.pixel_format, ColorSpace::SRGB);
        let timestamp = Timestamp::from_nanos(meta.timestamp);
        let frame = match &contents.picture {
            Picture::Empty => return None,
            Picture::DmaBuf { surface, .. } => {
                // SAFETY: guaranteed by the contract of `Consumer::new`: the DMA-BUF is
                // complete, of `format`, and stays valid and unwritten while the lease
                // holds the slot.
                unsafe {
                    VideoFrame::native(
                        format,
                        timestamp,
                        NativeSurface::DmaBuf(*surface),
                        SurfaceSync::Ready,
                        lease,
                    )
                }
            }
            Picture::System { data, stride } => {
                let planes = [PlaneLayout {
                    offset: 0,
                    stride: *stride,
                }];
                // A refcount clone: the stream thread promoted the buffer to shared
                // storage before publishing it.
                let system = SystemFrame::new(format, data.clone(), &planes).ok()?;
                VideoFrame::system(timestamp, system, Some(lease))
            }
        };
        self.format = format;
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
    use rbs_core::media::{DmaBufPlane, MemoryKind};

    fn consumer(shared: &Arc<Shared>) -> Consumer {
        let format = VideoFormat::new(Size::new(64, 32), PixelFormat::Bgra8, ColorSpace::SRGB);
        // SAFETY: the test never imports the made-up DMA-BUF descriptions.
        unsafe { Consumer::new(Arc::clone(shared), format) }
    }

    fn system_bytes(len: usize) -> Bytes {
        let bytes = Bytes::from(vec![0x7f; len]);
        drop(bytes.clone());
        bytes
    }

    fn publish(shared: &Shared, sequence: u64, picture: Picture, pf: PixelFormat) {
        let Some(slot) = shared.ring.claim() else {
            panic!("free slot")
        };
        // SAFETY: the slot was just claimed by this thread.
        match unsafe { shared.slot_mut(slot) } {
            Some(contents) => {
                contents.picture = picture;
                contents.pixel_format = pf;
            }
            None => panic!("slot exists"),
        }
        shared.ring.publish(
            slot,
            FrameMeta::new(1_000 + sequence, sequence, 7 + sequence, 64, 32),
        );
    }

    #[test]
    fn system_copies_become_system_frames() {
        let shared = Arc::new(Shared::new());
        let mut consumer = consumer(&shared);
        assert!(consumer.next().is_none());
        publish(
            &shared,
            3,
            Picture::System {
                data: system_bytes(64 * 4 * 32),
                stride: 64 * 4,
            },
            PixelFormat::Rgba8,
        );
        let Some(frame) = consumer.next() else {
            panic!("a frame")
        };
        assert_eq!(frame.memory_kind(), MemoryKind::System);
        assert_eq!(frame.format().pixel_format, PixelFormat::Rgba8);
        assert_eq!(frame.sequence(), 3);
        assert!(!shared.ring.is_idle());
        drop(frame);
        assert!(shared.ring.is_idle());
        assert_eq!(consumer.delivered(), 1);
    }

    #[test]
    fn dmabufs_become_native_frames() {
        let shared = Arc::new(Shared::new());
        let mut consumer = consumer(&shared);
        let plane = DmaBufPlane {
            fd: 42,
            offset: 0,
            stride: 256,
        };
        let Ok(surface) = DmaBufSurface::new(0x3432_5258, 0, &[plane]) else {
            panic!("valid surface")
        };
        publish(
            &shared,
            0,
            Picture::DmaBuf { surface, fd: None },
            PixelFormat::Bgra8,
        );
        let Some(frame) = consumer.next() else {
            panic!("a frame")
        };
        assert_eq!(frame.memory_kind(), MemoryKind::DmaBuf);
        let Some((native, sync, _lease)) = frame.into_native() else {
            panic!("native payload")
        };
        assert_eq!(native, NativeSurface::DmaBuf(surface));
        assert_eq!(sync, SurfaceSync::Ready);
    }

    #[test]
    fn next_does_not_allocate() {
        let shared = Arc::new(Shared::new());
        let mut consumer = consumer(&shared);
        let data = system_bytes(64 * 4 * 32);
        for seq in 0..100 {
            publish(
                &shared,
                seq,
                Picture::System {
                    data: data.clone(),
                    stride: 64 * 4,
                },
                PixelFormat::Bgra8,
            );
            let info = allocation_counter::measure(|| {
                drop(std::hint::black_box(consumer.next()));
            });
            assert_eq!(info.count_total, 0, "frame {seq} allocated");
        }
        assert!(shared.ring.is_idle());
    }
}
