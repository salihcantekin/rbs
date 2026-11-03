//! Media Foundation adapter for the RBS capture port (`docs/specs/capture-port.md`):
//! Windows cameras, delivered in their native uncompressed formats in system memory.
//!
//! # How frames travel
//!
//! - `enumerate` lists video capture devices and reads each one's native media types
//!   through a short-lived media source; nothing stays open.
//! - `open` reads the device's types again, picks one with
//!   [`rbs_capture::camera::choose_mode`] and releases the device (Standby, P13: the
//!   camera light stays off).
//! - `start` creates the media source and an asynchronous source reader with format
//!   converters disabled, sets the chosen native type and requests samples. Media
//!   Foundation calls the reader callback on its work queue threads.
//! - The callback copies each sample's picture into the buffer of a free slot of the
//!   lock-free [`Ring`] (one copy, counted in `cpu_copied`) and publishes the slot.
//!   USB cameras deliver into system memory, so this copy replaces the upload Media
//!   Foundation's own Direct3D path would do; nothing is read back from a GPU.
//! - `latest_frame` on the render thread takes the newest slot and returns a system
//!   frame sharing the slot's buffer. It does not allocate, lock or call into the OS.
//!
//! On other platforms the factory compiles and probes as unavailable, so the engine can
//! register it unconditionally.

// The platform-neutral half is only called by the Windows backend and the tests.
#![cfg_attr(not(windows), allow(dead_code))]

use std::cell::UnsafeCell;
use std::sync::Arc;

use bytes::BytesMut;
use rbs_capture::handoff::Ring;
use rbs_core::media::{
    Bytes, ColorPrimaries, ColorRange, ColorSpace, Lease, LeaseOwner, MatrixCoefficients,
    PixelFormat, PlaneLayout, Size, SystemFrame, TransferFunction, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;

/// Planes a frame can have (`rbs_core`'s limit for system frames).
const MAX_PLANES: usize = 3;

#[cfg(windows)]
mod win;

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.capture.mf";

/// Slots of the hand-off ring per session. The consumer may hold [`FRAMES_IN_FLIGHT`];
/// one more waits as the latest frame and one is filled by the reader callback.
pub const RING_SLOTS: usize = 5;

/// Frames the consumer may hold at once without stopping delivery.
pub const FRAMES_IN_FLIGHT: usize = RING_SLOTS - 2;

/// The pixel formats this adapter delivers, in the order it lists them.
pub const PIXEL_FORMATS: [PixelFormat; 4] = [
    PixelFormat::Nv12,
    PixelFormat::Yuy2,
    PixelFormat::I420,
    PixelFormat::Bgra8,
];

pub use platform::MfCaptureFactory;

#[cfg(windows)]
mod platform {
    pub use crate::win::MfCaptureFactory;
}

#[cfg(not(windows))]
mod platform {
    use rbs_capture::{CaptureError, CaptureFactory, CaptureRequest, CaptureSession};
    use rbs_capture::{CaptureTarget, TargetKind};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Factory of the Media Foundation camera adapter. Unavailable on this platform.
    #[derive(Debug, Default)]
    pub struct MfCaptureFactory;

    impl MfCaptureFactory {
        /// Creates the factory. Does nothing until used.
        pub fn new() -> Self {
            Self
        }
    }

    impl AdapterFactory for MfCaptureFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "Media Foundation cameras"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Err(ProbeError::new("Media Foundation needs Windows"))
        }
    }

    impl CaptureFactory for MfCaptureFactory {
        fn enumerate(&self, _kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
            Err(CaptureError::Unsupported("Media Foundation needs Windows"))
        }

        fn open(&self, _request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
            Err(CaptureError::Unsupported("Media Foundation needs Windows"))
        }
    }
}

/// `D3DFMT_X8R8G8B8`, the first field of `MFVideoFormat_RGB32`: B, G, R, X bytes.
const SUBTYPE_RGB32: u32 = 22;

/// The pixel format of a Media Foundation video subtype, from the subtype GUID's first
/// field (`MFVideoFormat_*` GUIDs are a FOURCC or `D3DFORMAT` plus a fixed base).
/// Compressed and other formats are `None`.
pub(crate) fn pixel_format_of_subtype(data1: u32) -> Option<PixelFormat> {
    const fn fourcc(code: &[u8; 4]) -> u32 {
        u32::from_le_bytes(*code)
    }
    match data1 {
        x if x == fourcc(b"NV12") => Some(PixelFormat::Nv12),
        x if x == fourcc(b"YUY2") => Some(PixelFormat::Yuy2),
        x if x == fourcc(b"I420") || x == fourcc(b"IYUV") => Some(PixelFormat::I420),
        SUBTYPE_RGB32 => Some(PixelFormat::Bgra8),
        _ => None,
    }
}

/// `MFVideoTransferMatrix_BT709`.
pub(crate) const MATRIX_BT709: u32 = 1;
/// `MFVideoTransferMatrix_BT601`.
pub(crate) const MATRIX_BT601: u32 = 2;
/// `MFNominalRange_0_255`.
pub(crate) const RANGE_FULL: u32 = 1;
/// `MFNominalRange_16_235`.
pub(crate) const RANGE_LIMITED: u32 = 2;

/// The colour of frames of `pf` from a media type's `MF_MT_YUV_MATRIX` and
/// `MF_MT_VIDEO_NOMINAL_RANGE`. Without them Media Foundation's defaults apply: BT.601
/// below 720 lines, BT.709 from 720 lines, limited range. RGB is sRGB.
pub(crate) fn color_of(
    pf: PixelFormat,
    matrix: Option<u32>,
    range: Option<u32>,
    height: u32,
) -> ColorSpace {
    if pf == PixelFormat::Bgra8 {
        return ColorSpace::SRGB;
    }
    let matrix = match matrix {
        Some(MATRIX_BT601) => MatrixCoefficients::Bt601,
        Some(MATRIX_BT709) => MatrixCoefficients::Bt709,
        _ if height < 720 => MatrixCoefficients::Bt601,
        _ => MatrixCoefficients::Bt709,
    };
    let range = match range {
        Some(RANGE_FULL) => ColorRange::Full,
        Some(RANGE_LIMITED) | Some(_) | None => ColorRange::Limited,
    };
    ColorSpace::new(
        ColorPrimaries::Bt709,
        TransferFunction::Bt709,
        matrix,
        range,
    )
}

/// Where the picture of a locked Media Foundation buffer lies: `bytes` is the whole
/// buffer, `scan0` the offset of the first row of the top line in it, `pitch` the
/// bytes from one row to the next (negative for bottom-up RGB). Planes after the first
/// follow the first after `height` rows, with the chroma pitch of the format.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SourceLayout {
    pub(crate) scan0: usize,
    pub(crate) pitch: isize,
}

/// Copies a `pf` picture of `size` from `bytes` (laid out as `layout` says) into `out`,
/// tightly packed, and returns the plane layouts. BGRX pixels get an opaque alpha.
/// Returns `None`, leaving `out` cleared, when the layout does not fit in `bytes`.
/// Reuses `out`'s capacity: allocates only when the picture grows.
pub(crate) fn copy_picture(
    pf: PixelFormat,
    size: Size,
    bytes: &[u8],
    layout: SourceLayout,
    out: &mut BytesMut,
) -> Option<([PlaneLayout; MAX_PLANES], usize)> {
    out.clear();
    let count = pf.plane_count();
    if count == 0 || count > MAX_PLANES || size.is_empty() {
        return None;
    }
    // Planar formats are never stored bottom-up.
    if count > 1 && layout.pitch <= 0 {
        return None;
    }
    let mut planes = [PlaneLayout::default(); MAX_PLANES];
    let mut total = 0usize;
    for (plane, entry) in planes.iter_mut().enumerate().take(count) {
        let row = usize::try_from(pf.min_row_bytes(plane, size.width)?).ok()?;
        let rows = pf.plane_rows(plane, size.height)? as usize;
        *entry = PlaneLayout {
            offset: total,
            stride: row,
        };
        total = total.checked_add(row.checked_mul(rows)?)?;
    }
    out.resize(total, 0);

    // Source pitch and start of each plane, relative to `scan0`.
    let luma_pitch = layout.pitch;
    let mut start = 0isize;
    for (plane, entry) in planes.iter().copied().enumerate().take(count) {
        let pitch = match (pf, plane) {
            (PixelFormat::I420, 1 | 2) => luma_pitch / 2,
            _ => luma_pitch,
        };
        // Rows of one plane must not overlap (an odd I420 width with a tight pitch).
        if pitch.unsigned_abs() < entry.stride {
            out.clear();
            return None;
        }
        let rows = pf.plane_rows(plane, size.height)? as usize;
        let dst = out.get_mut(entry.offset..entry.offset + entry.stride * rows)?;
        for (index, dst_row) in dst.chunks_exact_mut(entry.stride).enumerate() {
            let from = (layout.scan0 as isize)
                .checked_add(start)?
                .checked_add(pitch.checked_mul(index as isize)?)?;
            let from = usize::try_from(from).ok()?;
            let Some(src_row) = bytes.get(from..from + entry.stride) else {
                out.clear();
                return None;
            };
            dst_row.copy_from_slice(src_row);
            if pf == PixelFormat::Bgra8 {
                for pixel in dst_row.chunks_exact_mut(4) {
                    pixel[3] = 0xff;
                }
            }
        }
        start = start.checked_add(pitch.checked_mul(rows as isize)?)?;
    }
    Some((planes, count))
}

/// One slot's picture.
pub(crate) struct Slot {
    pub(crate) data: Bytes,
    pub(crate) planes: [PlaneLayout; MAX_PLANES],
    pub(crate) plane_count: usize,
}

/// The ring and the pictures its slots hold. Frames hold an `Arc` of it.
pub(crate) struct Shared {
    pub(crate) ring: Ring,
    slots: Box<[UnsafeCell<Slot>]>,
}

// SAFETY: a slot's cell is written only by the producer while it holds the slot's claim
// (reference count 1, not yet published), and read only by the consumer after `take`
// handed it a reference. The ring's protocol makes these exclusive: a claimed,
// unpublished slot is never taken, and a slot is claimed again only once every
// reference is released. The ring's AcqRel publish and take order the write before the
// read; the lease's Release decrement and the claim's Acquire order the read before the
// next write. The contents (`Bytes`, plain data) are `Send`.
unsafe impl Sync for Shared {}

impl Shared {
    pub(crate) fn new() -> Self {
        Self {
            ring: Ring::new(&[0; RING_SLOTS]),
            slots: (0..RING_SLOTS)
                .map(|_| {
                    UnsafeCell::new(Slot {
                        data: Bytes::new(),
                        planes: [PlaneLayout::default(); MAX_PLANES],
                        plane_count: 0,
                    })
                })
                .collect(),
        }
    }

    /// The slot's contents, for the producer.
    ///
    /// # Safety
    ///
    /// The caller holds `slot`'s claim (returned by `Ring::claim`, not yet published or
    /// abandoned), or the producer has ended and no frame refers to the slot.
    #[allow(
        clippy::mut_from_ref,
        reason = "interior mutability through UnsafeCell"
    )]
    pub(crate) unsafe fn slot_mut(&self, slot: usize) -> Option<&mut Slot> {
        // SAFETY: exclusive by the caller's contract and the ring protocol (see the
        // `Sync` impl).
        self.slots.get(slot).map(|cell| unsafe { &mut *cell.get() })
    }

    /// Bytes held by the slots.
    ///
    /// # Safety
    ///
    /// No slot is being written: the producer has ended.
    pub(crate) unsafe fn held_bytes(&self) -> u64 {
        self.slots
            .iter()
            // SAFETY: no writer by the caller's contract; readers only read.
            .map(|cell| unsafe { (*cell.get()).data.len() as u64 })
            .sum()
    }
}

// SAFETY: delegates to `Ring`, whose counting is exact when its calling protocol is
// followed (`rbs_capture::handoff::Ring`): the producer publishes, abandons and
// replaces only slots it claimed, and every lease releases exactly the reference it
// adopted or retained. A slot's buffer is only replaced while the slot is claimed, so
// it stays unchanged while any lease (which holds an `Arc<Shared>`) refers to it.
// Neither method locks.
unsafe impl LeaseOwner for Shared {
    fn retain(&self, token: u64) {
        self.ring.retain(token);
    }

    fn release(&self, token: u64) {
        self.ring.release(token);
    }
}

/// Fills a claimed slot with a copy of a picture. On `None` the slot is unchanged
/// except that its buffer may be emptied.
///
/// # Safety
///
/// The caller holds `slot`'s claim (see [`Shared::slot_mut`]).
pub(crate) unsafe fn fill_slot(
    shared: &Shared,
    slot: usize,
    pf: PixelFormat,
    size: Size,
    bytes: &[u8],
    layout: SourceLayout,
) -> Option<()> {
    // SAFETY: the caller holds the claim.
    let contents = unsafe { shared.slot_mut(slot) }?;
    let previous = std::mem::take(&mut contents.data);
    // The slot is free, so no frame shares its buffer and it converts back without a
    // copy; a buffer still shared somewhere (never, by the ring protocol) is replaced.
    let mut out = previous.try_into_mut().unwrap_or_default();
    let (planes, count) = copy_picture(pf, size, bytes, layout, &mut out)?;
    let data = out.freeze();
    // Promote to shared storage here, so the render thread's clone only bumps a
    // reference count. This allocates the small shared header of `bytes`, never the
    // pixel buffer, whose capacity is reused.
    drop(data.clone());
    contents.data = data;
    contents.planes = planes;
    contents.plane_count = count;
    Some(())
}

/// Builds frames from published slots on the render thread.
pub(crate) struct Consumer {
    shared: Arc<Shared>,
    owner: Arc<dyn LeaseOwner>,
    format: VideoFormat,
    delivered: u64,
}

impl Consumer {
    /// A consumer of `shared`'s ring delivering frames of `format` (its size follows
    /// the published frames).
    pub(crate) fn new(shared: Arc<Shared>, format: VideoFormat) -> Self {
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
        let ring = &self.shared.ring;
        let (slot, meta) = ring.take()?;
        // SAFETY: `take` moved exactly one reference of `slot` to this call; the lease
        // adopts it and releases it when the frame and its clones are dropped.
        let lease = unsafe { Lease::adopt(Arc::clone(&self.owner), slot as u64) };
        let cell = self.shared.slots.get(slot)?;
        // SAFETY: the slot is read-only while referenced (see the `Sync` impl).
        let contents = unsafe { &*cell.get() };
        let size = Size::new(meta.width, meta.height);
        let format = VideoFormat::new(size, self.format.pixel_format, self.format.color);
        let planes = contents.planes.get(..contents.plane_count)?;
        // A reference count clone: the producer promoted the buffer to shared storage.
        let system = SystemFrame::new(format, contents.data.clone(), planes).ok()?;
        let frame = VideoFrame::system(Timestamp::from_nanos(meta.timestamp), system, Some(lease));
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
    use rbs_core::media::{FramePayload, MemoryKind};

    #[test]
    fn subtypes_map_to_pixel_formats() {
        let f = |c: &[u8; 4]| pixel_format_of_subtype(u32::from_le_bytes(*c));
        assert_eq!(f(b"NV12"), Some(PixelFormat::Nv12));
        assert_eq!(f(b"YUY2"), Some(PixelFormat::Yuy2));
        assert_eq!(f(b"I420"), Some(PixelFormat::I420));
        assert_eq!(f(b"IYUV"), Some(PixelFormat::I420));
        assert_eq!(f(b"MJPG"), None);
        assert_eq!(pixel_format_of_subtype(22), Some(PixelFormat::Bgra8));
    }

    #[test]
    fn colour_follows_the_media_type_and_its_defaults() {
        let c = color_of(PixelFormat::Nv12, None, None, 480);
        assert_eq!(
            (c.matrix, c.range),
            (MatrixCoefficients::Bt601, ColorRange::Limited)
        );
        let c = color_of(PixelFormat::Nv12, None, None, 720);
        assert_eq!(c.matrix, MatrixCoefficients::Bt709);
        let c = color_of(
            PixelFormat::Yuy2,
            Some(MATRIX_BT601),
            Some(RANGE_FULL),
            1080,
        );
        assert_eq!(
            (c.matrix, c.range),
            (MatrixCoefficients::Bt601, ColorRange::Full)
        );
        assert_eq!(
            color_of(PixelFormat::Bgra8, None, None, 480),
            ColorSpace::SRGB
        );
    }

    #[test]
    fn nv12_planes_are_packed_from_a_padded_pitch() {
        // 4x2 NV12 with pitch 8: Y rows then one UV row, two bytes of padding each.
        let mut src = vec![0u8; 8 * 3];
        for (i, b) in src.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut out = BytesMut::new();
        let layout = SourceLayout { scan0: 0, pitch: 8 };
        let Some((planes, count)) =
            copy_picture(PixelFormat::Nv12, Size::new(4, 2), &src, layout, &mut out)
        else {
            panic!("fits")
        };
        assert_eq!(count, 2);
        assert_eq!(planes[1].offset, 8);
        assert_eq!(&out[..], &[0, 1, 2, 3, 8, 9, 10, 11, 16, 17, 18, 19]);
    }

    #[test]
    fn i420_chroma_uses_half_the_pitch() {
        // 4x2 I420, pitch 6: Y 2 rows of 6, U 1 row of 3, V 1 row of 3.
        let src: Vec<u8> = (0..18).collect();
        let mut out = BytesMut::new();
        let layout = SourceLayout { scan0: 0, pitch: 6 };
        let r = copy_picture(PixelFormat::I420, Size::new(4, 2), &src, layout, &mut out);
        assert!(r.is_some());
        assert_eq!(&out[..], &[0, 1, 2, 3, 6, 7, 8, 9, 12, 13, 15, 16]);
    }

    #[test]
    fn bottom_up_rgb_is_flipped_and_made_opaque() {
        // 1x2 BGRX stored bottom-up: scan0 points at the last row, pitch is negative.
        let src = [10, 11, 12, 0, 20, 21, 22, 0];
        let mut out = BytesMut::new();
        let layout = SourceLayout {
            scan0: 4,
            pitch: -4,
        };
        let r = copy_picture(PixelFormat::Bgra8, Size::new(1, 2), &src, layout, &mut out);
        assert!(r.is_some());
        assert_eq!(&out[..], &[20, 21, 22, 255, 10, 11, 12, 255]);
    }

    #[test]
    fn overlapping_chroma_rows_are_refused() {
        // 5x2 I420 with pitch 5: chroma rows need 3 bytes but the pitch gives 2.
        let src = [0u8; 64];
        let mut out = BytesMut::new();
        let layout = SourceLayout { scan0: 0, pitch: 5 };
        assert!(copy_picture(PixelFormat::I420, Size::new(5, 2), &src, layout, &mut out).is_none());
    }

    #[test]
    fn layouts_outside_the_buffer_are_refused() {
        let src = [0u8; 10];
        let mut out = BytesMut::new();
        let short = SourceLayout { scan0: 0, pitch: 8 };
        assert!(copy_picture(PixelFormat::Yuy2, Size::new(4, 2), &src, short, &mut out).is_none());
        assert!(out.is_empty());
        let up = SourceLayout {
            scan0: 0,
            pitch: -4,
        };
        assert!(copy_picture(PixelFormat::Bgra8, Size::new(1, 2), &src, up, &mut out).is_none());
        let planar_up = SourceLayout {
            scan0: 8,
            pitch: -4,
        };
        assert!(
            copy_picture(
                PixelFormat::Nv12,
                Size::new(4, 2),
                &src,
                planar_up,
                &mut out
            )
            .is_none()
        );
    }

    fn publish(shared: &Shared, sequence: u64, src: &[u8]) -> bool {
        let Some(slot) = shared.ring.claim() else {
            return false;
        };
        let layout = SourceLayout {
            scan0: 0,
            pitch: 128,
        };
        // SAFETY: the slot was just claimed by this thread.
        let filled = unsafe {
            fill_slot(
                shared,
                slot,
                PixelFormat::Yuy2,
                Size::new(64, 32),
                src,
                layout,
            )
        };
        if filled.is_none() {
            shared.ring.abandon(slot);
            return false;
        }
        shared
            .ring
            .publish(slot, FrameMeta::new(100 + sequence, sequence, 0, 64, 32));
        true
    }

    fn consumer(shared: &Arc<Shared>) -> Consumer {
        let format = VideoFormat::new(
            Size::new(64, 32),
            PixelFormat::Yuy2,
            ColorSpace::BT709_LIMITED,
        );
        Consumer::new(Arc::clone(shared), format)
    }

    #[test]
    fn copies_become_system_frames() {
        let shared = Arc::new(Shared::new());
        let mut consumer = consumer(&shared);
        assert!(consumer.next().is_none());
        let src = vec![7u8; 128 * 32];
        assert!(publish(&shared, 3, &src));
        let Some(frame) = consumer.next() else {
            panic!("a frame")
        };
        assert_eq!(frame.memory_kind(), MemoryKind::System);
        assert_eq!(frame.format().pixel_format, PixelFormat::Yuy2);
        assert_eq!(frame.sequence(), 3);
        assert_eq!(frame.timestamp().as_nanos(), 103);
        let FramePayload::System(system) = frame.payload() else {
            panic!("system payload")
        };
        assert_eq!(system.plane(0).map(|p| (p.0.len(), p.1)), Some((4096, 128)));
        assert!(!shared.ring.is_idle());
        drop(frame);
        assert!(shared.ring.is_idle());
        assert_eq!(consumer.delivered(), 1);
        assert_eq!(shared.ring.misuses(), 0);
    }

    #[test]
    fn steady_state_reuses_buffers_and_never_allocates_on_the_render_thread() {
        let shared = Arc::new(Shared::new());
        let mut consumer = consumer(&shared);
        let src = vec![7u8; 128 * 32];
        // Warm every slot's buffer once.
        for seq in 0..RING_SLOTS as u64 * 2 {
            assert!(publish(&shared, seq, &src));
            drop(consumer.next());
        }
        for seq in 100..200 {
            let produced = allocation_counter::measure(|| {
                assert!(publish(&shared, seq, &src));
            });
            // The pixel buffer is reused; only `bytes`' small shared header is made
            // again when a frozen buffer is shared (off the render thread).
            assert!(
                produced.bytes_total < 256,
                "producer allocated {} bytes at {seq}",
                produced.bytes_total
            );
            let info = allocation_counter::measure(|| {
                drop(std::hint::black_box(consumer.next()));
            });
            assert_eq!(info.count_total, 0, "frame {seq} allocated");
        }
        assert!(shared.ring.is_idle());
    }
}
