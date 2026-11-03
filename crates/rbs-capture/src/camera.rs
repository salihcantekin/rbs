//! Camera mode selection shared by camera adapters (AVFoundation, Media Foundation,
//! and later V4L2): turns the native modes a device reports into the port's
//! [`CameraMode`] list and picks the mode for a [`CaptureRequest`](crate::CaptureRequest).
//! See `docs/specs/capture-port.md`, "Camera helpers".
//!
//! Like [`handoff`](crate::handoff), this is an adapter-support module: versioned with
//! the port crate but outside the port contract, so it may change in a minor version.
//! It is platform independent and never called on the render thread.

use std::cmp::Ordering;

use rbs_core::media::{PixelFormat, Size};
use rbs_core::timing::FrameRate;

use crate::{CameraMode, CaptureError};

/// One mode a device can deliver, as the adapter sees it.
///
/// `pixel_format` is the format the adapter delivers for it (after any conversion the
/// OS does on the way, such as AVFoundation's), not necessarily the sensor's. A device
/// format that can be delivered in several pixel formats is listed once per pixel
/// format. `index` is the adapter's own handle for the device format (an index into
/// its format list); the helper only hands it back.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeMode {
    /// Picture width in pixels.
    pub width: u32,
    /// Picture height in pixels.
    pub height: u32,
    /// The pixel format delivered for this mode.
    pub pixel_format: PixelFormat,
    /// Lowest frame rate of the mode.
    pub min_rate: FrameRate,
    /// Highest frame rate of the mode.
    pub max_rate: FrameRate,
    /// The adapter's handle for the device format.
    pub index: usize,
}

impl NativeMode {
    /// A mode with a rate range. `min_rate` above `max_rate` is swapped.
    pub fn new(
        width: u32,
        height: u32,
        pixel_format: PixelFormat,
        min_rate: FrameRate,
        max_rate: FrameRate,
        index: usize,
    ) -> Self {
        let (min_rate, max_rate) = if rate_cmp(min_rate, max_rate) == Ordering::Greater {
            (max_rate, min_rate)
        } else {
            (min_rate, max_rate)
        };
        Self {
            width,
            height,
            pixel_format,
            min_rate,
            max_rate,
            index,
        }
    }

    fn contains(&self, rate: FrameRate) -> bool {
        rate_cmp(rate, self.min_rate) != Ordering::Less
            && rate_cmp(rate, self.max_rate) != Ordering::Greater
    }

    fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// The mode picked for a request.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeChoice {
    /// [`NativeMode::index`] of the chosen mode.
    pub index: usize,
    /// Picture size.
    pub size: Size,
    /// Delivered pixel format.
    pub pixel_format: PixelFormat,
    /// Frame rate to configure: the requested one clamped into the mode's range (one of
    /// the range's own bounds when clamped, so adapters can pass the OS its exact
    /// value), or the mode's highest rate.
    pub rate: FrameRate,
}

/// Exact comparison of two frame rates.
pub fn rate_cmp(a: FrameRate, b: FrameRate) -> Ordering {
    let (a, b) = (a.per_second(), b.per_second());
    (u64::from(a.num()) * u64::from(b.den())).cmp(&(u64::from(b.num()) * u64::from(a.den())))
}

fn rate_f64(rate: FrameRate) -> f64 {
    let r = rate.per_second();
    f64::from(r.num()) / f64::from(r.den())
}

/// The port's mode list for a device: one [`CameraMode`] per distinct size, pixel
/// format and rate range, in the order first listed.
pub fn camera_modes(modes: &[NativeMode]) -> Vec<CameraMode> {
    let mut out: Vec<CameraMode> = Vec::with_capacity(modes.len());
    for m in modes {
        let mode = CameraMode::ranged(m.width, m.height, m.pixel_format, m.min_rate, m.max_rate);
        if !out.contains(&mode) {
            out.push(mode);
        }
    }
    out
}

/// Picks the mode for a request, following the capture port's rules:
///
/// 1. Pixel format: the first entry of `pixel_formats` that some mode delivers
///    (invariant 4), otherwise `FormatUnsupported`.
/// 2. Size: with `size`, the nearest mode size of that format (smallest sum of width
///    and height differences, ties to the larger picture); without, the largest.
/// 3. Among the modes of that size and format: one whose range contains `rate`, else
///    the one whose range is closest to it; without `rate`, the highest maximum rate.
///    Ties go to the mode listed first, so adapters list preferred device formats
///    first.
/// 4. Rate: `rate` clamped into the chosen range, or its highest rate.
pub fn choose_mode(
    modes: &[NativeMode],
    pixel_formats: &[PixelFormat],
    size: Option<Size>,
    rate: Option<FrameRate>,
) -> Result<ModeChoice, CaptureError> {
    let pixel_format = pixel_formats
        .iter()
        .copied()
        .find(|pf| modes.iter().any(|m| m.pixel_format == *pf))
        .ok_or(CaptureError::FormatUnsupported)?;
    let of_format = || modes.iter().filter(move |m| m.pixel_format == pixel_format);

    let best_size = match size {
        Some(want) => {
            let distance = |m: &NativeMode| {
                u64::from(m.width.abs_diff(want.width)) + u64::from(m.height.abs_diff(want.height))
            };
            of_format().min_by(|a, b| {
                distance(a)
                    .cmp(&distance(b))
                    .then_with(|| b.area().cmp(&a.area()))
            })
        }
        // `max_by_key` keeps the last of equal elements; reverse to keep the first.
        None => of_format().rev().max_by_key(|m| m.area()),
    }
    .ok_or(CaptureError::FormatUnsupported)?;
    let (width, height) = (best_size.width, best_size.height);
    let same_size = || of_format().filter(move |m| (m.width, m.height) == (width, height));

    let chosen = match rate {
        Some(want) => {
            let miss = |m: &NativeMode| -> f64 {
                if m.contains(want) {
                    0.0
                } else if rate_cmp(want, m.min_rate) == Ordering::Less {
                    rate_f64(m.min_rate) - rate_f64(want)
                } else {
                    rate_f64(want) - rate_f64(m.max_rate)
                }
            };
            same_size().min_by(|a, b| miss(a).total_cmp(&miss(b)))
        }
        None => same_size()
            .rev()
            .max_by(|a, b| rate_cmp(a.max_rate, b.max_rate)),
    }
    .ok_or(CaptureError::FormatUnsupported)?;

    let rate = match rate {
        Some(want) if rate_cmp(want, chosen.min_rate) == Ordering::Less => chosen.min_rate,
        Some(want) if rate_cmp(want, chosen.max_rate) == Ordering::Greater => chosen.max_rate,
        Some(want) => want,
        None => chosen.max_rate,
    };
    Ok(ModeChoice {
        index: chosen.index,
        size: Size::new(width, height),
        pixel_format,
        rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fps(n: u32) -> FrameRate {
        FrameRate::new(n, 1).unwrap_or(FrameRate::FPS_30)
    }

    fn modes() -> Vec<NativeMode> {
        vec![
            NativeMode::new(640, 480, PixelFormat::Nv12, fps(5), fps(30), 0),
            NativeMode::new(1280, 720, PixelFormat::Nv12, fps(5), fps(10), 1),
            NativeMode::new(1280, 720, PixelFormat::Nv12, fps(30), fps(60), 2),
            NativeMode::new(1920, 1080, PixelFormat::Nv12, fps(5), fps(5), 3),
            NativeMode::new(640, 480, PixelFormat::Bgra8, fps(5), fps(30), 0),
            NativeMode::new(1280, 720, PixelFormat::Bgra8, fps(5), fps(10), 1),
        ]
    }

    #[test]
    fn request_order_decides_the_pixel_format() {
        let m = modes();
        let pick = |pfs: &[PixelFormat]| choose_mode(&m, pfs, None, None).map(|c| c.pixel_format);
        assert_eq!(
            pick(&[PixelFormat::Bgra8, PixelFormat::Nv12]),
            Ok(PixelFormat::Bgra8)
        );
        assert_eq!(
            pick(&[PixelFormat::Yuy2, PixelFormat::Nv12]),
            Ok(PixelFormat::Nv12)
        );
        assert_eq!(
            pick(&[PixelFormat::Yuy2]),
            Err(CaptureError::FormatUnsupported)
        );
        assert_eq!(
            choose_mode(&[], &[PixelFormat::Nv12], None, None),
            Err(CaptureError::FormatUnsupported)
        );
    }

    #[test]
    fn without_size_the_largest_wins_and_without_rate_the_fastest() {
        let m = modes();
        let c = choose_mode(&m, &[PixelFormat::Nv12], None, None);
        assert_eq!(c.map(|c| (c.index, c.size)), Ok((3, Size::new(1920, 1080))));
        let c = choose_mode(&m, &[PixelFormat::Bgra8], None, None);
        assert_eq!(c.map(|c| (c.index, c.rate)), Ok((1, fps(10))));
    }

    #[test]
    fn nearest_size_then_a_range_containing_the_rate() {
        let m = modes();
        let c = choose_mode(&m, &[PixelFormat::Nv12], Some(Size::new(1200, 700)), None);
        assert_eq!(c.map(|c| (c.index, c.rate)), Ok((2, fps(60))));
        let c = choose_mode(
            &m,
            &[PixelFormat::Nv12],
            Some(Size::new(1280, 720)),
            Some(fps(8)),
        );
        assert_eq!(c.map(|c| (c.index, c.rate)), Ok((1, fps(8))));
        let c = choose_mode(
            &m,
            &[PixelFormat::Nv12],
            Some(Size::new(1280, 720)),
            Some(FrameRate::FPS_29_97),
        );
        assert_eq!(c.map(|c| (c.index, c.rate)), Ok((2, fps(30))));
    }

    #[test]
    fn rates_outside_every_range_clamp_to_the_nearest_bound() {
        let m = modes();
        let at = |rate| {
            choose_mode(
                &m,
                &[PixelFormat::Nv12],
                Some(Size::new(1280, 720)),
                Some(rate),
            )
            .map(|c| (c.index, c.rate))
        };
        assert_eq!(at(fps(120)), Ok((2, fps(60))));
        assert_eq!(at(fps(1)), Ok((1, fps(5))));
        assert_eq!(at(fps(12)), Ok((1, fps(10))));
        assert_eq!(at(fps(25)), Ok((2, fps(30))));
    }

    #[test]
    fn ties_go_to_the_mode_listed_first() {
        let m = vec![
            NativeMode::new(640, 480, PixelFormat::Nv12, fps(30), fps(30), 7),
            NativeMode::new(640, 480, PixelFormat::Nv12, fps(30), fps(30), 8),
        ];
        assert_eq!(
            choose_mode(&m, &[PixelFormat::Nv12], None, None).map(|c| c.index),
            Ok(7)
        );
        let c = choose_mode(&m, &[PixelFormat::Nv12], None, Some(fps(30)));
        assert_eq!(c.map(|c| c.index), Ok(7));
    }

    #[test]
    fn modes_are_listed_once_and_ranges_are_ordered() {
        let mut m = modes();
        m.push(NativeMode::new(
            640,
            480,
            PixelFormat::Nv12,
            fps(30),
            fps(5),
            9,
        ));
        let list = camera_modes(&m);
        assert_eq!(list.len(), 6);
        assert_eq!(list[0].min_frame_rate, fps(5));
        assert_eq!(list[0].max_frame_rate, fps(30));
    }

    #[test]
    fn rate_comparison_is_exact() {
        assert_eq!(rate_cmp(FrameRate::FPS_29_97, fps(30)), Ordering::Less);
        assert_eq!(
            rate_cmp(FrameRate::new(60, 2).unwrap_or(fps(1)), fps(30)),
            Ordering::Equal
        );
    }
}
