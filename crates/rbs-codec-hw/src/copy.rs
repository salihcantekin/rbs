//! Copies a system-memory 4:2:0 frame (NV12 or P010) into an encoder input buffer.

use rbs_core::media::{PixelFormat, SystemFrame};

/// Bytes per row and rows of the two planes of `format` at `width` x `height`, or
/// `None` for a format that is not bi-planar 4:2:0.
pub fn plane_sizes(format: PixelFormat, width: u32, height: u32) -> Option<[(usize, usize); 2]> {
    if !matches!(format, PixelFormat::Nv12 | PixelFormat::P010) {
        return None;
    }
    let mut planes = [(0, 0); 2];
    for (i, p) in planes.iter_mut().enumerate() {
        let row = usize::try_from(format.min_row_bytes(i, width)?).ok()?;
        let rows = usize::try_from(format.plane_rows(i, height)?).ok()?;
        *p = (row, rows);
    }
    Some(planes)
}

/// A destination plane: its bytes and the distance between rows.
#[derive(Debug)]
pub struct DstPlane<'a> {
    /// The plane, at least `pitch * (rows - 1) + row_bytes` bytes long.
    pub data: &'a mut [u8],
    /// Bytes from one row to the next.
    pub pitch: usize,
}

/// Copies both planes of `frame` (whose format is `format`, `width` x `height`) into
/// `dst`, row by row. Returns `false`, having copied an unspecified part, when the
/// frame is not bi-planar 4:2:0 or a source or destination plane is too small.
pub fn copy_420(
    frame: &SystemFrame,
    format: PixelFormat,
    width: u32,
    height: u32,
    dst: [DstPlane<'_>; 2],
) -> bool {
    let Some(planes) = plane_sizes(format, width, height) else {
        return false;
    };
    for (plane, ((row_bytes, rows), dst)) in planes.into_iter().zip(dst).enumerate() {
        let Some((src, stride)) = frame.plane(plane) else {
            return false;
        };
        if dst.pitch < row_bytes || stride < row_bytes {
            return false;
        }
        for y in 0..rows {
            let (Some(from), Some(to)) = (
                src.get(y * stride..y * stride + row_bytes),
                dst.data.get_mut(y * dst.pitch..y * dst.pitch + row_bytes),
            ) else {
                return false;
            };
            to.copy_from_slice(from);
        }
    }
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use rbs_codec::conformance::{FrameSource, SystemFrameSource};
    use rbs_core::media::{ColorSpace, FramePayload, Size, VideoFormat};
    use rbs_core::time::Timestamp;

    #[test]
    fn copies_rows_into_a_wider_pitch() {
        let format = VideoFormat::new(
            Size::new(64, 32),
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        );
        let frame = SystemFrameSource::new()
            .frame(&format, 3, Timestamp::from_nanos(0))
            .unwrap();
        let FramePayload::System(system) = frame.payload() else {
            panic!("system frame expected");
        };
        let mut luma = vec![0u8; 80 * 32];
        let mut chroma = vec![0u8; 80 * 16];
        assert!(copy_420(
            system,
            PixelFormat::Nv12,
            64,
            32,
            [
                DstPlane {
                    data: &mut luma,
                    pitch: 80
                },
                DstPlane {
                    data: &mut chroma,
                    pitch: 80
                },
            ],
        ));
        let (src, stride) = system.plane(0).unwrap();
        assert_eq!(
            &luma[80 * 5..80 * 5 + 64],
            &src[stride * 5..stride * 5 + 64]
        );
        let (src, stride) = system.plane(1).unwrap();
        assert_eq!(
            &chroma[80 * 15..80 * 15 + 64],
            &src[stride * 15..stride * 15 + 64]
        );

        let mut short = vec![0u8; 10];
        let mut chroma = vec![0u8; 80 * 16];
        assert!(!copy_420(
            system,
            PixelFormat::Nv12,
            64,
            32,
            [
                DstPlane {
                    data: &mut short,
                    pitch: 80
                },
                DstPlane {
                    data: &mut chroma,
                    pitch: 80
                },
            ],
        ));
        assert!(plane_sizes(PixelFormat::Bgra8, 64, 32).is_none());
        assert_eq!(
            plane_sizes(PixelFormat::P010, 64, 32),
            Some([(128, 32), (128, 16)])
        );
    }
}
