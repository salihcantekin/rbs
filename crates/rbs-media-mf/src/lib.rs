//! Media Foundation adapter for the RBS media-decode port
//! (`docs/specs/media-decode-port.md`, ADR-0108): local video files on Windows.
//!
//! # How a file is read
//!
//! - `open` creates a synchronous source reader on the file with only the first video
//!   stream selected and asks for `RGB32`; the reader's advanced video processing
//!   converts and deinterlaces. The stated length comes from the presentation
//!   descriptor.
//! - Sound has its own source reader on the same file with only the first audio stream
//!   selected, created on the first `next_sound`. The two streams are read separately
//!   (spec invariant 2), so neither reader queues samples of a stream nobody reads,
//!   and a source that is never heard opens no second reader (P13).
//! - Sound is trimmed to the sound track's MP4 edit list (`edits.rs`): Media
//!   Foundation keeps the AAC encoder priming and end padding, which AVFoundation
//!   drops, so without the trim the sound would start about 21 ms before the picture.
//! - Zero-copy (`MfMediaFactory::new` opened with `open_for` and the render adapter's
//!   LUID): the decoder and its video processor run on a D3D11 device of that adapter,
//!   and each `B8G8R8A8` picture is copied on the GPU, cropped to the display aperture,
//!   into a shared NT-handle texture (`gpu.rs`). Without a LUID, when that device
//!   cannot be made or cannot decode the file, or for a picture that is not on the GPU,
//!   pictures are copied as below (spec invariant 11).
//! - Otherwise each picture is copied once from the decoder's buffer into a reused
//!   `Rgba8` buffer (`B, G, R, X` to `R, G, B, 255`, cropped to the display aperture).
//!   That buffer is returned for the worker to take with `Bytes::from`. A spare stays
//!   here so the next frame does not write a picture the caller still holds.
//!
//! The capability is `zero-copy` for `new` and `cpu-upload` for `cpu_upload` (the
//! measured fallback of ADR-0108 decision 4).
//!
//! On other platforms the factory compiles and probes as unavailable, so a composition
//! root can register it unconditionally.

// The platform-neutral half is only called by the Windows backend and the tests.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::Path;
use std::time::Duration;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_media_decode::{DecodeError, MediaDecoder, MediaDecoderFactory, caps, pixel};

mod edits;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod win;

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.media-decode.mf";

/// The factory. Holds nothing but its pixel path; Media Foundation starts with the
/// first open.
#[derive(Debug)]
pub struct MfMediaFactory {
    native: bool,
}

impl Default for MfMediaFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl MfMediaFactory {
    /// The `zero-copy` factory: opened for a render adapter, pictures are shared D3D11
    /// textures on it. Does nothing until used.
    pub fn new() -> Self {
        Self { native: true }
    }

    /// The `cpu-upload` factory: each picture is copied into `Rgba8`. The measured
    /// fallback, used by the conformance pixel checks and the benchmark comparison.
    pub fn cpu_upload() -> Self {
        Self { native: false }
    }
}

impl AdapterFactory for MfMediaFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "Media Foundation files"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        if cfg!(windows) {
            Ok(CapabilitySet::new().with(
                caps::PIXEL,
                CapabilityValue::Set(vec![
                    if self.native {
                        pixel::ZERO_COPY
                    } else {
                        pixel::CPU_UPLOAD
                    }
                    .to_owned(),
                ]),
            ))
        } else {
            Err(ProbeError::new("Media Foundation needs Windows"))
        }
    }
}

impl MediaDecoderFactory for MfMediaFactory {
    #[cfg(windows)]
    fn open(&self, path: &Path) -> Result<Box<dyn MediaDecoder>, DecodeError> {
        Ok(Box::new(win::MfDecoder::open(path, None)?))
    }

    #[cfg(windows)]
    fn open_for(
        &self,
        path: &Path,
        target: &rbs_media_decode::DecodeTarget,
    ) -> Result<Box<dyn MediaDecoder>, DecodeError> {
        let luid = if self.native {
            target.adapter_luid
        } else {
            None
        };
        let decoder = win::MfDecoder::open(path, luid)?;
        // Media Foundation's outputs (NV12, RGB32) drop alpha (ADR-0140 decision 2).
        if target.keep_alpha && decoder.has_alpha() {
            return Err(DecodeError::AlphaUnsupported);
        }
        Ok(Box::new(decoder))
    }

    #[cfg(not(windows))]
    fn open(&self, _path: &Path) -> Result<Box<dyn MediaDecoder>, DecodeError> {
        Err(DecodeError::Unsupported(
            "Media Foundation needs Windows".into(),
        ))
    }
}

/// Media Foundation time (100 ns units) as a duration; negative is unstated.
fn hns(value: i64) -> Option<Duration> {
    u64::try_from(value)
        .ok()
        .map(|v| Duration::from_nanos(v.saturating_mul(100)))
}

/// A duration in Media Foundation time (100 ns units), saturating.
fn to_hns(value: Duration) -> i64 {
    i64::try_from(value.as_nanos() / 100).unwrap_or(i64::MAX)
}

/// The nominal frame duration of `MF_MT_FRAME_RATE` (numerator in the high 32 bits).
fn nominal_duration(packed_rate: u64) -> Option<Duration> {
    let num = packed_rate >> 32;
    let den = packed_rate & 0xFFFF_FFFF;
    if num == 0 || den == 0 {
        return None;
    }
    Some(Duration::from_nanos(
        den.saturating_mul(1_000_000_000) / num,
    ))
}

/// The part of the decoded picture that is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Crop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Crop {
    /// The display aperture `(x, y, width, height)` clipped to a `frame_w` x `frame_h`
    /// picture; the whole picture when the aperture is missing or empty.
    fn of(frame_w: u32, frame_h: u32, aperture: Option<(i32, i32, i32, i32)>) -> Self {
        let whole = Self {
            x: 0,
            y: 0,
            width: frame_w,
            height: frame_h,
        };
        let Some((x, y, w, h)) = aperture else {
            return whole;
        };
        let (Ok(x), Ok(y), Ok(w), Ok(h)) = (
            u32::try_from(x),
            u32::try_from(y),
            u32::try_from(w),
            u32::try_from(h),
        ) else {
            return whole;
        };
        if w == 0 || h == 0 || x >= frame_w || y >= frame_h {
            return whole;
        }
        Self {
            x,
            y,
            width: w.min(frame_w - x),
            height: h.min(frame_h - y),
        }
    }
}

/// Copies the `crop` of a `B, G, R, X` picture into `out` as `R, G, B, 255`.
///
/// `rows[i]` is where row `i` of the decoded picture starts in `src` and is built from
/// the pitch (a bottom-up picture has a negative pitch, so its first row is last in
/// memory). Returns `None` when a row lies outside `src`.
fn bgrx_to_rgba(
    src: &[u8],
    first_row: usize,
    pitch: isize,
    crop: Crop,
    out: &mut Vec<u8>,
) -> Option<()> {
    out.clear();
    out.reserve(crop.width as usize * crop.height as usize * 4);
    for row in 0..crop.height as usize {
        let line = row + crop.y as usize;
        let start =
            isize::try_from(first_row).ok()? + pitch.checked_mul(isize::try_from(line).ok()?)?;
        let start = usize::try_from(start).ok()? + crop.x as usize * 4;
        let end = start.checked_add(crop.width as usize * 4)?;
        let bytes = src.get(start..end)?;
        for px in bytes.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    Some(())
}

/// The CPU fallback's swizzle buffer and the spare that replaces it.
///
/// One picture is swizzled into a buffer this value owns, and that buffer is returned.
/// The media worker takes the `Vec` with `Bytes::from` and does not copy the pixels.
/// `Bytes::from` may take the allocation only once a spare of the same size is already
/// held here, or the spare was large enough to swap in: the returned buffer is not the
/// one the next frame writes. The first frame may allocate. A larger frame may grow
/// the spare. A picture the caller still holds is never that spare.
struct SwizzleSpare {
    spare: Vec<u8>,
}

impl SwizzleSpare {
    fn new() -> Self {
        Self { spare: Vec::new() }
    }

    /// A buffer with room for `len` bytes. After this returns, `spare` is a different
    /// allocation (already large enough when this one had to grow), so the caller can
    /// keep the result across the next frame.
    fn take(&mut self, len: usize) -> Vec<u8> {
        if self.spare.capacity() < len {
            self.spare.reserve(len);
            let out = std::mem::take(&mut self.spare);
            self.spare.reserve(len);
            return out;
        }
        std::mem::take(&mut self.spare)
    }

    /// Puts `buf` back when the caller wrote the picture it returns somewhere else
    /// (a turn). Not for a buffer the caller still holds.
    fn recycle(&mut self, buf: Vec<u8>) {
        if buf.capacity() > self.spare.capacity() {
            self.spare = buf;
        }
    }

    /// Swizzles the crop of `src` into a buffer the next call will not write.
    /// `None` when a row lies outside `src`.
    fn swizzle(
        &mut self,
        src: &[u8],
        first_row: usize,
        pitch: isize,
        crop: Crop,
    ) -> Option<Vec<u8>> {
        let len = crop.width as usize * crop.height as usize * 4;
        let mut out = self.take(len);
        if bgrx_to_rgba(src, first_row, pitch, crop, &mut out).is_none() {
            self.recycle(out);
            return None;
        }
        Some(out)
    }
}

/// Interleaved little-endian `f32` bytes as samples.
fn f32_samples(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// What to tell the user when the platform has no decoder for a video subtype, from
/// the subtype GUID's first field (a FOURCC for compressed formats).
fn missing_codec(fourcc: u32) -> String {
    let code = fourcc.to_le_bytes();
    let text: String = code
        .iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
        .collect();
    match &code {
        b"HEVC" | b"H265" | b"HVC1" | b"hvc1" | b"hev1" => {
            "HEVC video needs the HEVC Video Extensions from the Microsoft Store".into()
        }
        b"AV01" | b"av01" => {
            "AV1 video needs the AV1 Video Extension from the Microsoft Store".into()
        }
        b"VP90" | b"vp09" => {
            "VP9 video needs the VP9 Video Extensions from the Microsoft Store".into()
        }
        b"H264" | b"AVC1" | b"avc1" => {
            "H.264 video needs Media Foundation (on Windows N, the Media Feature Pack)".into()
        }
        _ => format!("no Media Foundation decoder for the {text} video format"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    use super::*;

    struct AllocCount;

    thread_local! {
        static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
        static IN_ALLOC: Cell<bool> = const { Cell::new(false) };
    }

    fn count_allocation() {
        // `try_with` fails while this thread's local is still being created, so the
        // counter cannot re-enter itself. Other threads keep their own counts.
        let _ = IN_ALLOC.try_with(|flag| {
            if flag.replace(true) {
                return;
            }
            let _ = ALLOCATIONS.try_with(|n| n.set(n.get().saturating_add(1)));
            flag.set(false);
        });
    }

    fn allocations() -> usize {
        ALLOCATIONS.with(|n| n.get())
    }

    // SAFETY: forwards to the system allocator and only counts calls on this thread.
    unsafe impl GlobalAlloc for AllocCount {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            count_allocation();
            // SAFETY: `layout` is the size and alignment asked for by the caller.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: `ptr` was returned by `alloc` or `realloc` for this `layout`.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            count_allocation();
            // SAFETY: `ptr` was allocated with `layout`, and `new_size` is the new size
            // in bytes.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: AllocCount = AllocCount;

    #[test]
    fn media_foundation_time_is_in_hundreds_of_nanoseconds() {
        assert_eq!(hns(10_000), Some(Duration::from_millis(1)));
        assert_eq!(hns(-1), None);
        assert_eq!(to_hns(Duration::from_millis(1)), 10_000);
    }

    #[test]
    fn the_nominal_duration_comes_from_the_packed_rate() {
        assert_eq!(
            nominal_duration((30_000u64 << 32) | 1001),
            Some(Duration::from_nanos(33_366_666))
        );
        assert_eq!(nominal_duration(25u64 << 32), None);
    }

    #[test]
    fn the_aperture_crops_and_falls_back_to_the_whole_picture() {
        let crop = Crop::of(1920, 1088, Some((0, 0, 1920, 1080)));
        assert_eq!((crop.width, crop.height), (1920, 1080));
        assert_eq!(Crop::of(8, 8, None).height, 8);
        assert_eq!(Crop::of(8, 8, Some((0, 0, 0, 0))).width, 8);
        assert_eq!(Crop::of(8, 8, Some((-1, 0, 4, 4))).width, 8);
        assert_eq!(Crop::of(8, 8, Some((6, 6, 4, 4))).width, 2);
    }

    #[test]
    fn a_second_frame_reuses_the_swizzle_spare() {
        // 1x2, top-down. Two frames, same size, different pixels.
        let first_src = [1, 2, 3, 0, 10, 20, 30, 0];
        let second_src = [4, 5, 6, 0, 40, 50, 60, 0];
        let crop = Crop::of(1, 2, None);
        let mut spare = SwizzleSpare::new();
        let _ = allocations();
        let first = spare.swizzle(&first_src, 0, 4, crop).unwrap();
        let kept = first.clone();
        let before = allocations();
        let second = spare.swizzle(&second_src, 0, 4, crop).unwrap();
        let after = allocations();
        assert_eq!(after, before, "the second frame allocated");
        assert_eq!(first, kept);
        assert_eq!(first, vec![3, 2, 1, 255, 30, 20, 10, 255]);
        assert_eq!(second, vec![6, 5, 4, 255, 60, 50, 40, 255]);
        assert_ne!(first.as_ptr(), second.as_ptr());
    }

    #[test]
    fn a_bottom_up_picture_is_flipped_and_swizzled() {
        // 1x2 picture, bottom-up: memory holds row 1 then row 0.
        let src = [1, 2, 3, 0, 10, 20, 30, 0];
        let mut out = Vec::new();
        let crop = Crop::of(1, 2, None);
        bgrx_to_rgba(&src, 4, -4, crop, &mut out).unwrap();
        assert_eq!(out, vec![30, 20, 10, 255, 3, 2, 1, 255]);
        bgrx_to_rgba(&src, 0, 4, crop, &mut out).unwrap();
        assert_eq!(out, vec![3, 2, 1, 255, 30, 20, 10, 255]);
        assert!(bgrx_to_rgba(&src, 4, 4, crop, &mut out).is_none());
    }

    #[test]
    fn a_missing_codec_names_what_to_install() {
        assert!(missing_codec(u32::from_le_bytes(*b"HEVC")).contains("HEVC Video Extensions"));
        assert!(missing_codec(u32::from_le_bytes(*b"XYZW")).contains("XYZW"));
    }

    #[test]
    fn the_factory_probes_by_platform() {
        assert_eq!(MfMediaFactory::new().probe().is_ok(), cfg!(windows));
    }
}
