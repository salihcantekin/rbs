//! Windows Graphics Capture adapter for the RBS capture port
//! (`docs/specs/capture-port.md`): displays and windows, delivered zero-copy as
//! Direct3D 11 NT shared textures on the render adapter.
//!
//! # How frames travel
//!
//! - `open` checks the request and the target and creates nothing (Standby, P13).
//! - `start` creates a D3D11 device on the adapter named by the request's
//!   [`GpuImportTarget`](rbs_capture::GpuImportTarget) LUID, takes a shared fence from a
//!   process-wide pool (fences are never closed, so their handle values are never
//!   reused), and creates a ring of [`RING_SLOTS`] shareable BGRA8 textures and a
//!   free-threaded WGC frame pool.
//! - `stop` hands the textures to a graveyard drained by a short-lived janitor thread:
//!   they stay alive while frames reference them and for a grace period after that, so
//!   the render thread never frees them and importers never see a reused handle value.
//! - On each `FrameArrived` callback (an OS thread) the adapter copies the new frame into
//!   a free ring texture, signals the shared fence and publishes the slot through the
//!   lock-free [`Ring`].
//! - `latest_frame` on the render thread takes the newest slot and returns a
//!   `D3dSharedHandle` frame with a `TimelineFence` sync. It does not allocate, lock or
//!   call into the OS.
//!
//! The importer side is `docs/specs/gpu-compositor.md`, "Native import".
//!
//! On other platforms the factory compiles and probes as unavailable, so the engine can
//! register it unconditionally.

use std::sync::Arc;

use rbs_core::media::{
    D3dApi, Lease, LeaseOwner, NativeSurface, Size, SurfaceSync, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;

#[cfg(windows)]
mod thumb;
#[cfg(windows)]
mod win;

/// The program name in an executable path: the file name without its extension
/// (`C:\\Apps\\obs64.exe` is `obs64`), or `None` when nothing is left.
#[cfg_attr(not(windows), allow(dead_code))]
fn exe_stem(path: &str) -> Option<&str> {
    let file = path.rsplit(['\\', '/']).next()?;
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    (!stem.is_empty()).then_some(stem)
}

use rbs_capture::handoff::Ring;

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.capture.wgc";

/// Shared textures per session. The consumer may hold [`FRAMES_IN_FLIGHT`] of them;
/// one more waits as the latest frame and one is written by the OS callback.
pub const RING_SLOTS: usize = 5;

/// Frames the consumer may hold at once without stopping delivery.
pub const FRAMES_IN_FLIGHT: usize = RING_SLOTS - 2;

/// The `u64` form of a Windows adapter LUID used in [`NativeSurface`] (`adapter_luid`):
/// the eight bytes of the `LUID` struct (`LowPart` then `HighPart`, little-endian), as
/// carried by [`GpuDeviceId::Luid`](rbs_capture::GpuDeviceId::Luid), read as a
/// little-endian integer. Equal to `(HighPart as u32 as u64) << 32 | LowPart as u64`.
pub const fn luid_to_u64(luid: [u8; 8]) -> u64 {
    u64::from_le_bytes(luid)
}

pub use platform::WgcCaptureFactory;

#[cfg(windows)]
mod platform {
    pub use crate::win::WgcCaptureFactory;
}

#[cfg(not(windows))]
mod platform {
    use rbs_capture::{CaptureError, CaptureFactory, CaptureRequest, CaptureSession};
    use rbs_capture::{CaptureTarget, TargetKind};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Factory of the Windows Graphics Capture adapter. Unavailable on this platform.
    #[derive(Debug, Default)]
    pub struct WgcCaptureFactory;

    impl WgcCaptureFactory {
        /// Creates the factory. Does nothing until used.
        pub fn new() -> Self {
            Self
        }
    }

    impl AdapterFactory for WgcCaptureFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "Windows Graphics Capture"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Err(ProbeError::new("Windows Graphics Capture needs Windows"))
        }
    }

    impl CaptureFactory for WgcCaptureFactory {
        fn enumerate(&self, _kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
            Err(CaptureError::Unsupported(
                "Windows Graphics Capture needs Windows",
            ))
        }

        fn open(&self, _request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
            Err(CaptureError::Unsupported(
                "Windows Graphics Capture needs Windows",
            ))
        }
    }
}

/// Builds frames from published ring slots on the render thread.
///
/// Platform independent so it is tested everywhere; the Windows producer owns the
/// surfaces and the fence it names.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct Consumer<O: AsRef<Ring> + LeaseOwner + 'static> {
    shared: Arc<O>,
    owner: Arc<dyn LeaseOwner>,
    format: VideoFormat,
    luid: u64,
    fence: u64,
    delivered: u64,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl<O: AsRef<Ring> + LeaseOwner + 'static> Consumer<O> {
    /// A consumer of `shared`'s ring.
    ///
    /// # Safety
    ///
    /// Every handle the ring names must be a live NT shared handle of a BGRA8 D3D11
    /// texture on the adapter `luid`, of the size published with it, and `fence` a live
    /// NT shared handle of a D3D11 fence the producer signals after writing a slot. Each
    /// stays valid, and the producer does not write the texture, while its slot is
    /// referenced (a lease of `shared` is held).
    pub(crate) unsafe fn new(shared: Arc<O>, format: VideoFormat, luid: u64, fence: u64) -> Self {
        let owner: Arc<dyn LeaseOwner> = Arc::clone(&shared) as Arc<dyn LeaseOwner>;
        Self {
            shared,
            owner,
            format,
            luid,
            fence,
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
        let Some(handle) = ring.handle(slot) else {
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
        let surface = NativeSurface::d3d_shared_handle(handle, true, D3dApi::D3d11, self.luid);
        let sync = SurfaceSync::timeline_fence(self.fence, meta.sync_value);
        // SAFETY: guaranteed by the contract of `Consumer::new`: the handle and fence are
        // live, describe a texture of `self.format` on `self.luid`, and stay valid and
        // unwritten while the lease holds the slot.
        let frame = unsafe {
            VideoFrame::native(
                self.format,
                Timestamp::from_nanos(meta.timestamp),
                surface,
                sync,
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

/// The largest size with the aspect of `width` x `height` inside `max`, never larger
/// than the source, at least 1 x 1; `None` when any side is zero (capture-port.md,
/// `thumbnail`).
#[cfg_attr(not(windows), allow(dead_code))]
fn fit_size(width: u32, height: u32, max: Size) -> Option<(u32, u32)> {
    if width == 0 || height == 0 || max.width == 0 || max.height == 0 {
        return None;
    }
    let scale = (f64::from(max.width) / f64::from(width))
        .min(f64::from(max.height) / f64::from(height))
        .min(1.0);
    let scaled = |v: u32, cap: u32| ((f64::from(v) * scale).round() as u32).clamp(1, cap);
    Some((scaled(width, max.width), scaled(height, max.height)))
}

/// Box-filters a BGRA8 picture (`pitch` bytes per row) down to `dst_w` x `dst_h` opaque
/// RGBA8: each output pixel is the mean of the source pixels it covers. The output is
/// never larger than the source (callers pass a size from [`fit_size`]).
#[cfg_attr(not(windows), allow(dead_code))]
fn downscale_bgra(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    pitch: usize,
    dst_w: u32,
    dst_h: u32,
) -> Vec<u8> {
    let (sw, sh, dw, dh) = (
        src_w as usize,
        src_h as usize,
        dst_w as usize,
        dst_h as usize,
    );
    let mut out = Vec::with_capacity(dw * dh * 4);
    for y in 0..dh {
        let (y0, y1) = (
            y * sh / dh,
            ((y + 1) * sh / dh).max(y * sh / dh + 1).min(sh),
        );
        for x in 0..dw {
            let (x0, x1) = (
                x * sw / dw,
                ((x + 1) * sw / dw).max(x * sw / dw + 1).min(sw),
            );
            let mut sum = [0u64; 3];
            let mut n = 0u64;
            for row in y0..y1 {
                let line = row * pitch;
                for col in x0..x1 {
                    let i = line + col * 4;
                    if let Some(px) = src.get(i..i + 4) {
                        sum[0] += u64::from(px[2]);
                        sum[1] += u64::from(px[1]);
                        sum[2] += u64::from(px[0]);
                        n += 1;
                    }
                }
            }
            let n = n.max(1);
            out.extend_from_slice(&[
                ((sum[0] + n / 2) / n) as u8,
                ((sum[1] + n / 2) / n) as u8,
                ((sum[2] + n / 2) / n) as u8,
                255,
            ]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn thumbnails_fit_keep_the_aspect_and_never_grow() {
        let max = rbs_core::media::Size::new(192, 108);
        assert_eq!(super::fit_size(3840, 2160, max), Some((192, 108)));
        assert_eq!(super::fit_size(1080, 1920, max), Some((61, 108)));
        assert_eq!(super::fit_size(100, 50, max), Some((100, 50)));
        assert_eq!(super::fit_size(5000, 1, max), Some((192, 1)));
        assert_eq!(super::fit_size(0, 10, max), None);
        assert_eq!(
            super::fit_size(10, 10, rbs_core::media::Size::new(0, 5)),
            None
        );
    }

    #[test]
    fn bgra_is_averaged_into_opaque_rgba() {
        // 2x2 BGRA with a padded pitch of 12 bytes: blue, red / green, white.
        let mut src = vec![0u8; 24];
        src[0..4].copy_from_slice(&[255, 0, 0, 255]);
        src[4..8].copy_from_slice(&[0, 0, 255, 255]);
        src[12..16].copy_from_slice(&[0, 255, 0, 0]);
        src[16..20].copy_from_slice(&[255, 255, 255, 255]);
        assert_eq!(
            super::downscale_bgra(&src, 2, 2, 12, 1, 1),
            vec![128, 128, 128, 255]
        );
        let same = super::downscale_bgra(&src, 2, 2, 12, 2, 2);
        assert_eq!(&same[0..4], &[0, 0, 255, 255]);
        assert_eq!(&same[4..8], &[255, 0, 0, 255]);
        assert_eq!(&same[8..12], &[0, 255, 0, 255]);
    }

    #[test]
    fn exe_stem_takes_the_file_name_without_extension() {
        assert_eq!(
            super::exe_stem(r"C:\Program Files\App\app.exe"),
            Some("app")
        );
        assert_eq!(super::exe_stem("/usr/bin/tool"), Some("tool"));
        assert_eq!(super::exe_stem("noext"), Some("noext"));
        assert_eq!(super::exe_stem(r"C:\dir\"), None);
        assert_eq!(super::exe_stem(""), None);
    }

    use super::*;
    use rbs_capture::handoff::FrameMeta;
    use rbs_core::media::{ColorSpace, MemoryKind, PixelFormat};

    fn meta(sequence: u64, width: u32) -> FrameMeta {
        FrameMeta::new(1_000 + sequence, sequence, 7_000 + sequence, width, 720)
            .with_sync_value(sequence + 1)
    }

    /// A lease owner over a bare ring, for tests whose handles are never opened.
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

    // SAFETY: delegates to the ring's exact counting; the made-up handle values are
    // never opened, so there is no surface whose validity the counts would protect.
    unsafe impl LeaseOwner for Owner {
        fn retain(&self, token: u64) {
            self.0.retain(token);
        }
        fn release(&self, token: u64) {
            self.0.release(token);
        }
    }

    fn consumer(ring: &Arc<Owner>) -> Consumer<Owner> {
        let format = VideoFormat::new(Size::new(1280, 720), PixelFormat::Bgra8, ColorSpace::SRGB);
        // SAFETY: the test never dereferences the made-up handle values.
        unsafe { Consumer::new(Arc::clone(ring), format, 0x1234, 0x99) }
    }

    #[test]
    fn luid_encoding_matches_the_struct_layout() {
        // LowPart = 0x0000_C0DE, HighPart = 1.
        let bytes = [0xDE, 0xC0, 0, 0, 1, 0, 0, 0];
        assert_eq!(luid_to_u64(bytes), (1u64 << 32) | 0xC0DE);
    }

    #[test]
    fn frames_carry_handle_fence_and_metadata() {
        let ring = Arc::new(Owner(Ring::new(&[0x10, 0x20, 0x30])));
        let mut consumer = consumer(&ring);
        assert!(consumer.next().is_none());
        let Some(slot) = ring.claim() else {
            panic!("free slot")
        };
        ring.publish(slot, meta(4, 1280));
        let Some(frame) = consumer.next() else {
            panic!("a frame")
        };
        assert_eq!(frame.memory_kind(), MemoryKind::D3d11);
        assert_eq!(frame.timestamp().as_nanos(), 1_004);
        assert_eq!(frame.sequence(), 4);
        assert_eq!(frame.source_time(), Some(7_004));
        let Some((surface, sync, lease)) = frame.into_native() else {
            panic!("native payload")
        };
        assert_eq!(
            surface,
            NativeSurface::d3d_shared_handle(
                0x10 + 0x10 * slot as u64,
                true,
                D3dApi::D3d11,
                0x1234
            )
        );
        assert_eq!(sync, SurfaceSync::timeline_fence(0x99, 5));
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
        // Warm up once.
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
