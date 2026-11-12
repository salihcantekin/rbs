//! Framebuffer size of a display, distinct from the OS scale (ADR-0102 decision 8).
//!
//! No operating-system calls. Adapters pass what they read; this chooses the pixel
//! size and the Video settings label. The size is never multiplied or divided by the
//! scale.

/// Pixels per logical point of one display.
///
/// Windows stores the monitor's effective DPI over 96, so the scale is `dpi / 96`.
/// macOS stores the framebuffer width over the display's width in points
/// (`display_scale`). The canvas size is not this ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayScale {
    num: u32,
    den: u32,
}

impl DisplayScale {
    /// Windows effective DPI. The scale is `dpi / 96`. `None` when `dpi` is 0.
    pub fn from_dpi(dpi: u32) -> Option<Self> {
        (dpi > 0).then_some(Self { num: dpi, den: 96 })
    }

    /// macOS pixels per point, as the framebuffer width over the point width.
    /// `None` when either side is 0.
    pub fn from_pixels_per_point(pixels: u32, points: u32) -> Option<Self> {
        (pixels > 0 && points > 0).then_some(Self {
            num: pixels,
            den: points,
        })
    }

    /// Integer percent closest to this scale. A half rounds away from zero.
    /// 96 DPI and 1 pixel per point are 100.
    pub fn percent(self) -> u32 {
        nearest_percent(u64::from(self.num), u64::from(self.den))
    }
}

/// What an adapter read for one display. Absent fields were not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DisplayReading {
    /// `GetMonitorInfo` rectangle, when it was read.
    pub monitor: Option<(u32, u32)>,
    /// Pixel mode (`EnumDisplaySettings` current mode, or `CGDisplayMode`), when read.
    pub mode: Option<(u32, u32)>,
    /// OS scale, when read. `None` is a missing DPI or a missing scale.
    pub scale: Option<DisplayScale>,
}

/// The framebuffer to offer as a canvas, and the scale percent for its label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramebufferChoice {
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
    /// Nearest integer percent. `None` when no scale was read, including at a
    /// missing DPI. 100 is included when the scale was read and is 1.
    pub percent: Option<u32>,
}

impl FramebufferChoice {
    /// Video settings row for this framebuffer.
    ///
    /// `{width} x {height} (main display, {percent}%)`. A portrait size keeps
    /// `(vertical)` ahead of that, as the canvas list already marks portrait
    /// presets. A missing percent omits `, {percent}%`.
    pub fn label(self) -> String {
        let size = if self.height > self.width {
            format!("{} x {} (vertical)", self.width, self.height)
        } else {
            format!("{} x {}", self.width, self.height)
        };
        match self.percent {
            Some(percent) => format!("{size} (main display, {percent}%)"),
            None => format!("{size} (main display)"),
        }
    }
}

/// Chooses the framebuffer size and the scale label.
///
/// The pixel mode wins when it was read and both sides are non-zero. Otherwise the
/// monitor rectangle is used. The scale never changes the size: a 200% framebuffer
/// stays its pixel size, and is not replaced by logical points. `None` when neither
/// size was read.
#[must_use]
pub fn choose_framebuffer(reading: DisplayReading) -> Option<FramebufferChoice> {
    let (width, height) = usable(reading.mode).or_else(|| usable(reading.monitor))?;
    Some(FramebufferChoice {
        width,
        height,
        percent: reading.scale.map(DisplayScale::percent),
    })
}

fn usable(size: Option<(u32, u32)>) -> Option<(u32, u32)> {
    size.filter(|(width, height)| *width > 0 && *height > 0)
}

/// Integer closest to `num / den * 100`. A half rounds away from zero.
/// `den` of 0 does not occur for a [`DisplayScale`]; it yields 0 rather than panicking.
fn nearest_percent(num: u64, den: u64) -> u32 {
    if den == 0 {
        return 0;
    }
    let scaled = num.saturating_mul(100);
    let quotient = scaled / den;
    let remainder = scaled % den;
    let quotient = if remainder.saturating_mul(2) >= den {
        quotient.saturating_add(1)
    } else {
        quotient
    };
    u32::try_from(quotient).unwrap_or(u32::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn scale2() -> DisplayScale {
        DisplayScale::from_pixels_per_point(2, 1).unwrap()
    }

    #[test]
    fn matching_sizes_at_200_percent_stay_the_framebuffer() {
        let choice = choose_framebuffer(DisplayReading {
            monitor: Some((3840, 2160)),
            mode: Some((3840, 2160)),
            scale: Some(scale2()),
        })
        .unwrap();
        assert_eq!((choice.width, choice.height), (3840, 2160));
        assert_eq!(choice.percent, Some(200));
        assert_eq!(choice.label(), "3840 x 2160 (main display, 200%)");
        assert_ne!((choice.width, choice.height), (1920, 1080));
        assert_ne!((choice.width, choice.height), (7680, 4320));
    }

    #[test]
    fn a_windows_mismatch_uses_the_pixel_mode_and_144_dpi_is_150_percent() {
        let choice = choose_framebuffer(DisplayReading {
            monitor: Some((1280, 720)),
            mode: Some((1920, 1080)),
            scale: DisplayScale::from_dpi(144),
        })
        .unwrap();
        assert_eq!((choice.width, choice.height), (1920, 1080));
        assert_eq!(choice.percent, Some(150));
        assert_eq!(choice.label(), "1920 x 1080 (main display, 150%)");
    }

    #[test]
    fn a_missing_dpi_keeps_the_mode_size_and_omits_the_percent() {
        let choice = choose_framebuffer(DisplayReading {
            monitor: Some((1280, 720)),
            mode: Some((1920, 1080)),
            scale: None,
        })
        .unwrap();
        assert_eq!((choice.width, choice.height), (1920, 1080));
        assert_eq!(choice.percent, None);
        assert_eq!(choice.label(), "1920 x 1080 (main display)");

        let monitor_only = choose_framebuffer(DisplayReading {
            monitor: Some((1280, 720)),
            mode: None,
            scale: None,
        })
        .unwrap();
        assert_eq!((monitor_only.width, monitor_only.height), (1280, 720));
        assert_eq!(monitor_only.percent, None);
    }

    #[test]
    fn one_hundred_percent_is_labelled_and_a_half_rounds_away_from_zero() {
        let full = choose_framebuffer(DisplayReading {
            monitor: None,
            mode: Some((1920, 1080)),
            scale: DisplayScale::from_dpi(96),
        })
        .unwrap();
        assert_eq!(full.percent, Some(100));
        assert_eq!(full.label(), "1920 x 1080 (main display, 100%)");

        // 12 * 100 / 96 = 12.5, which rounds away from zero to 13.
        assert_eq!(DisplayScale::from_dpi(12).unwrap().percent(), 13);
        // 11 * 100 / 96 = 11.458, which rounds to 11.
        assert_eq!(DisplayScale::from_dpi(11).unwrap().percent(), 11);
        assert_eq!(DisplayScale::from_dpi(192).unwrap().percent(), 200);
        assert!(DisplayScale::from_dpi(0).is_none());
        assert!(DisplayScale::from_pixels_per_point(0, 1080).is_none());
    }

    #[test]
    fn the_scale_does_not_resize_a_monitor_rectangle_and_zero_modes_are_ignored() {
        let logical = choose_framebuffer(DisplayReading {
            monitor: Some((1920, 1080)),
            mode: None,
            scale: Some(scale2()),
        })
        .unwrap();
        assert_eq!((logical.width, logical.height), (1920, 1080));
        assert_eq!(logical.percent, Some(200));

        let skipped = choose_framebuffer(DisplayReading {
            monitor: Some((2560, 1440)),
            mode: Some((0, 1440)),
            scale: None,
        })
        .unwrap();
        assert_eq!((skipped.width, skipped.height), (2560, 1440));

        assert!(
            choose_framebuffer(DisplayReading {
                monitor: None,
                mode: None,
                scale: Some(scale2()),
            })
            .is_none()
        );
    }

    #[test]
    fn a_portrait_framebuffer_keeps_the_vertical_mark() {
        let choice = choose_framebuffer(DisplayReading {
            monitor: None,
            mode: Some((1080, 1920)),
            scale: Some(scale2()),
        })
        .unwrap();
        assert_eq!(
            choice.label(),
            "1080 x 1920 (vertical) (main display, 200%)"
        );
    }
}
