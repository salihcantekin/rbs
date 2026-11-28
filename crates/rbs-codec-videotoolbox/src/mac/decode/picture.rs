//! Turns a decoded pixel buffer into a port picture: a native `IoSurface` frame with a
//! lease on a pool slot, or the counted copy to system memory (P4).
//!
//! Low-level module (principle P11): every `unsafe` block states its invariant.

use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use rbs_codec::VideoDecodeError;
use rbs_core::media::{Bytes, NativeSurface, PlaneLayout, SurfaceSync, SystemFrame, VideoFrame};

use super::input::Placement;
use super::slots::{HandOut, Slots};
use super::vt::{FOURCC_420F, FOURCC_420V};

/// Not decoded, but the session continues.
pub(super) fn skipped(what: &str) -> VideoDecodeError {
    VideoDecodeError::Failed {
        reason: what.into(),
        recoverable: true,
    }
}

/// Size of a pixel buffer.
fn size_of(buffer: &CVPixelBuffer) -> (u32, u32) {
    let w = u32::try_from(CVPixelBufferGetWidth(buffer)).unwrap_or(u32::MAX);
    let h = u32::try_from(CVPixelBufferGetHeight(buffer)).unwrap_or(u32::MAX);
    (w, h)
}

fn is_nv12(buffer: &CVPixelBuffer) -> bool {
    let fourcc = CVPixelBufferGetPixelFormatType(buffer);
    (fourcc == FOURCC_420V || fourcc == FOURCC_420F) && CVPixelBufferGetPlaneCount(buffer) == 2
}

/// What [`native`] made of a buffer.
pub(super) enum Native {
    Frame(VideoFrame),
    /// Every slot is leased: the picture was dropped.
    NoSlot,
    /// The buffer cannot be handed out as it is (no IOSurface, another layout, or a
    /// crop origin a surface cannot express): copy it to system memory.
    Copy(CFRetained<CVPixelBuffer>),
}

/// Hands `buffer` out as a native NV12 `IoSurface` picture when it holds exactly the
/// display picture.
pub(super) fn native(
    slots: &mut Slots,
    buffer: CFRetained<CVPixelBuffer>,
    at: Placement,
) -> Native {
    let (w, h) = size_of(&buffer);
    if !is_nv12(&buffer) || (w, h) != (at.size.width, at.size.height) {
        return Native::Copy(buffer);
    }
    let (surface, lease) = match slots.hand_out(buffer) {
        HandOut::Leased(surface, lease) => (surface, lease),
        HandOut::Full => return Native::NoSlot,
        HandOut::NoSurface(buffer) => return Native::Copy(buffer),
    };
    let native = NativeSurface::io_surface(CFRetained::as_ptr(&surface).as_ptr() as usize);
    // SAFETY: the IOSurface belongs to the pixel buffer the pool keeps in the lease's
    // slot until every reference to that slot is released, and VideoToolbox does not
    // write a buffer it handed out while it is retained. The decode has completed
    // (synchronous decode), so the surface is ready.
    let frame =
        unsafe { VideoFrame::native(at.format(), at.pts, native, SurfaceSync::Ready, lease) };
    Native::Frame(frame)
}

/// Copies the cropped NV12 picture of `buffer` to system memory (P4, counted by the
/// caller). A new buffer per picture: this is the fallback path.
pub(super) fn system_copy(
    buffer: &CVPixelBuffer,
    at: Placement,
) -> Result<VideoFrame, VideoDecodeError> {
    if !is_nv12(buffer) {
        return Err(skipped(
            "the decoder gave a picture of another pixel format",
        ));
    }
    let (bw, bh) = size_of(buffer);
    let (left, top) = at
        .origin_in(bw, bh)
        .ok_or_else(|| skipped("picture outside the decoded buffer"))?;
    let (w, h) = (at.size.width as usize, at.size.height as usize);
    let (left, top) = (left as usize, top as usize);
    let mut bytes = vec![0u8; w * h * 3 / 2];
    // SAFETY: the buffer is locked read-only between lock and unlock; each plane's
    // slice covers its rows (`height * bytes_per_row` readable bytes at its base
    // address) and is dropped before unlocking. Rows are read with checked `get`.
    let copied = unsafe {
        if CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return Err(skipped("lock picture"));
        }
        let plane = |i: usize| -> Option<(&[u8], usize)> {
            let base = CVPixelBufferGetBaseAddressOfPlane(buffer, i).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, i);
            let rows = CVPixelBufferGetHeightOfPlane(buffer, i);
            if base.is_null() {
                return None;
            }
            Some((
                std::slice::from_raw_parts(base, stride.checked_mul(rows)?),
                stride,
            ))
        };
        let mut ok = false;
        if let (Some((luma, ys)), Some((chroma, cs))) = (plane(0), plane(1)) {
            let rows = (0..h)
                .map(|r| (luma, (top + r) * ys + left, r * w))
                .chain((0..h / 2).map(|r| (chroma, (top / 2 + r) * cs + left, w * h + r * w)));
            ok = true;
            for (src, from, to) in rows {
                match (src.get(from..from + w), bytes.get_mut(to..to + w)) {
                    (Some(s), Some(d)) => d.copy_from_slice(s),
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
        }
        let _ = CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly);
        ok
    };
    if !copied {
        return Err(skipped("picture buffer smaller than its size"));
    }
    let planes = [
        PlaneLayout {
            offset: 0,
            stride: w,
        },
        PlaneLayout {
            offset: w * h,
            stride: w,
        },
    ];
    let frame = SystemFrame::new(at.format(), Bytes::from(bytes), &planes)
        .map_err(|e| skipped(&format!("picture layout: {e}")))?;
    Ok(VideoFrame::system(at.pts, frame, None))
}
