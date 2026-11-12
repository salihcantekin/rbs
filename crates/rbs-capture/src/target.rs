//! Capture targets: displays, windows and cameras.

use std::fmt;

use rbs_core::media::{PixelFormat, Size};
use rbs_core::timing::FrameRate;

use crate::framebuffer::DisplayScale;

/// What kind of thing a capture target is.
///
/// Non-exhaustive: more kinds (for example per-application capture) may be added in a
/// minor version.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetKind {
    /// A whole display (monitor).
    Display,
    /// A single top-level window.
    Window,
    /// A camera or other video input device.
    Camera,
}

impl TargetKind {
    /// All kinds known to this version, in a fixed order.
    pub const ALL: [TargetKind; 3] = [TargetKind::Display, TargetKind::Window, TargetKind::Camera];

    /// The symbol used in capability sets (`display`, `window`, `camera`).
    pub const fn as_str(self) -> &'static str {
        match self {
            TargetKind::Display => "display",
            TargetKind::Window => "window",
            TargetKind::Camera => "camera",
        }
    }
}

impl fmt::Display for TargetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An opaque, adapter-defined target id.
///
/// Stable for the lifetime of the target. For displays and cameras it should also be
/// stable across restarts where the OS allows it, because scenes store it. Targets
/// chosen through [`CaptureFactory::pick`](crate::CaptureFactory::pick) carry the OS
/// restore token in the id (for example the PipeWire portal restore token), so a scene
/// can reopen them without showing the picker again.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetId(String);

impl TargetId {
    /// Creates an id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TargetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One uncompressed mode a camera can deliver.
///
/// Compressed camera modes (MJPEG, H.264) are out of scope of this version of the
/// port; see the spec.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraMode {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Native pixel format of the mode.
    pub pixel_format: PixelFormat,
    /// Lowest frame rate of the mode.
    pub min_frame_rate: FrameRate,
    /// Highest frame rate of the mode.
    pub max_frame_rate: FrameRate,
}

impl CameraMode {
    /// A mode with one fixed frame rate.
    pub const fn fixed(
        width: u32,
        height: u32,
        pixel_format: PixelFormat,
        rate: FrameRate,
    ) -> Self {
        Self {
            width,
            height,
            pixel_format,
            min_frame_rate: rate,
            max_frame_rate: rate,
        }
    }

    /// A mode with a range of frame rates.
    pub const fn ranged(
        width: u32,
        height: u32,
        pixel_format: PixelFormat,
        min_frame_rate: FrameRate,
        max_frame_rate: FrameRate,
    ) -> Self {
        Self {
            width,
            height,
            pixel_format,
            min_frame_rate,
            max_frame_rate,
        }
    }
}

/// A target that can be captured, as returned by
/// [`CaptureFactory::enumerate`](crate::CaptureFactory::enumerate) or
/// [`CaptureFactory::pick`](crate::CaptureFactory::pick).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureTarget {
    /// Id to pass back in a [`CaptureRequest`](crate::CaptureRequest).
    pub id: TargetId,
    /// What the target is.
    pub kind: TargetKind,
    /// Human readable name: monitor name, window title, camera model.
    pub name: String,
    /// Native size in pixels when known (displays and windows).
    ///
    /// For a display this is the framebuffer in physical pixels. It is not multiplied
    /// by [`Self::scale`] (ADR-0102 decision 8).
    pub size: Option<(u32, u32)>,
    /// OS pixels per logical point of a display, when the adapter read it.
    /// `None` for windows, cameras and displays whose scale was not read.
    /// The canvas size is [`Self::size`], not this ratio.
    pub scale: Option<DisplayScale>,
    /// Supported modes for cameras; empty for displays and windows.
    pub modes: Vec<CameraMode>,
    /// Name of the application that owns the window, when the OS tells it
    /// (`None` for displays, cameras and adapters that cannot know). Only for
    /// grouping and display; hosts never branch on its value (P3).
    pub app: Option<String>,
    /// A device of another kind that belongs with this target, when the adapter can
    /// tell (a camera's built-in microphone). `None` when unknown. See [`RelatedDevice`].
    pub related: Option<RelatedDevice>,
    /// The display's work area in the same pixels as [`Self::size`] (ADR-0104).
    /// `None` for windows, cameras, and displays whose adapter cannot report one
    /// (the PipeWire portal, and any adapter that has not read it).
    pub work_rect: Option<WorkRect>,
}

/// A display's work area, origin at the top-left of the capture framebuffer (ADR-0104).
///
/// Pixels, not points. Optional: an adapter that cannot read it leaves the field
/// absent, and "Reset to system" is then a no-op.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkRect {
    /// Left edge, in framebuffer pixels.
    pub x: u32,
    /// Top edge, in framebuffer pixels.
    pub y: u32,
    /// Width in framebuffer pixels.
    pub width: u32,
    /// Height in framebuffer pixels.
    pub height: u32,
}

/// Maps a monitor rectangle and its work area onto a capture framebuffer.
///
/// `monitor` and `work` are `(left, top, right, bottom)` in one coordinate space
/// (Windows `rcMonitor` / `rcWork`, or points before scaling). `framebuffer` is
/// the capture size in pixels. When the monitor rectangle's size differs from the
/// framebuffer, the work area is scaled into it. `None` when either rectangle is
/// empty or the work area does not sit inside the monitor.
pub fn work_rect_in_framebuffer(
    framebuffer: (u32, u32),
    monitor: (i32, i32, i32, i32),
    work: (i32, i32, i32, i32),
) -> Option<WorkRect> {
    let (fw, fh) = framebuffer;
    if fw == 0 || fh == 0 {
        return None;
    }
    let mon_w = i64::from(monitor.2) - i64::from(monitor.0);
    let mon_h = i64::from(monitor.3) - i64::from(monitor.1);
    if mon_w <= 0 || mon_h <= 0 {
        return None;
    }
    let x = i64::from(work.0) - i64::from(monitor.0);
    let y = i64::from(work.1) - i64::from(monitor.1);
    let w = i64::from(work.2) - i64::from(work.0);
    let h = i64::from(work.3) - i64::from(work.1);
    if w <= 0 || h <= 0 || x < 0 || y < 0 || x + w > mon_w || y + h > mon_h {
        return None;
    }
    let map = |value: i64, span: i64, pixels: u32| -> Option<u32> {
        let scaled = value.checked_mul(i64::from(pixels))? / span;
        u32::try_from(scaled).ok()
    };
    let x0 = map(x, mon_w, fw)?;
    let y0 = map(y, mon_h, fh)?;
    let x1 = map(x + w, mon_w, fw)?;
    let y1 = map(y + h, mon_h, fh)?;
    if x1 <= x0 || y1 <= y0 || x1 > fw || y1 > fh {
        return None;
    }
    Some(WorkRect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

/// The kind of a [`RelatedDevice`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelatedKind {
    /// An audio input device.
    Audio,
}

/// A hint that a capture target has a companion device (ADR-0096, decision 5).
///
/// `id` is opaque: it is an id the audio adapter of the same platform can resolve to
/// one of its input devices. Hosts only pass it on; they never parse it or branch on
/// vendor or model names (P3). If the audio adapter cannot resolve it, the suggestion
/// is not shown.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedDevice {
    /// What kind of device it is.
    pub kind: RelatedKind,
    /// Opaque id the audio adapter can resolve.
    pub id: String,
}

impl RelatedDevice {
    /// An audio input device with the given opaque id.
    pub fn audio(id: impl Into<String>) -> Self {
        Self {
            kind: RelatedKind::Audio,
            id: id.into(),
        }
    }
}

impl CaptureTarget {
    /// A target with no known size and no modes.
    pub fn new(id: TargetId, kind: TargetKind, name: impl Into<String>) -> Self {
        Self {
            id,
            kind,
            name: name.into(),
            size: None,
            scale: None,
            modes: Vec::new(),
            app: None,
            related: None,
            work_rect: None,
        }
    }

    /// Sets the OS scale of a display. The pixel size is unchanged.
    pub fn with_scale(mut self, scale: DisplayScale) -> Self {
        self.scale = Some(scale);
        self
    }

    /// Sets the related device.
    pub fn with_related(mut self, related: RelatedDevice) -> Self {
        self.related = Some(related);
        self
    }

    /// Sets the native size.
    pub fn with_size(mut self, width: u32, height: u32) -> Self {
        self.size = Some((width, height));
        self
    }

    /// Sets the name of the owning application.
    pub fn with_app(mut self, app: impl Into<String>) -> Self {
        self.app = Some(app.into());
        self
    }

    /// Sets the camera modes.
    pub fn with_modes(mut self, modes: Vec<CameraMode>) -> Self {
        self.modes = modes;
        self
    }

    /// Sets the display work area. `None` clears it.
    pub fn with_work_rect(mut self, work: Option<WorkRect>) -> Self {
        self.work_rect = work;
        self
    }
}

/// Whether the application may capture a kind of target.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Capture is allowed (or the OS has no permission model for it).
    Granted,
    /// The user or a policy refused it. Only the user can change this, in OS settings.
    Denied,
    /// The user has not been asked yet; [`CaptureFactory::request_permission`] will ask.
    ///
    /// [`CaptureFactory::request_permission`]: crate::CaptureFactory::request_permission
    NotDetermined,
}

/// The largest thumbnail an adapter may return, in pixels (about 80 KiB of RGBA8).
pub const THUMBNAIL_MAX: Size = Size {
    width: 192,
    height: 108,
};

/// Longest side of a region-section grab, in pixels (ADR-0103 decision 4).
///
/// This is not [`THUMBNAIL_MAX`]. A region preview is a separate grab.
pub const REGION_GRAB_LONG: u32 = 960;

/// A small owned RGBA8 picture of a target, for browsing a list (never on the frame path).
///
/// A bounded exception to P4: it is one picture on request, at most [`THUMBNAIL_MAX`],
/// and is never stored or sent anywhere. The fields are private so the size always
/// matches the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    size: Size,
    rgba: Vec<u8>,
}

impl Thumbnail {
    /// Builds a thumbnail, or `None` if the size is zero, larger than [`THUMBNAIL_MAX`]
    /// or does not match the length of `rgba` (`width * height * 4` bytes, sRGB RGBA8,
    /// straight alpha, rows top to bottom, no padding).
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Option<Thumbnail> {
        let len = u64::from(width) * u64::from(height) * 4;
        let ok = width > 0
            && height > 0
            && width <= THUMBNAIL_MAX.width
            && height <= THUMBNAIL_MAX.height;
        (ok && len == rgba.len() as u64).then_some(Thumbnail {
            size: Size { width, height },
            rgba,
        })
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.size.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.size.height
    }

    /// The pixels, `width * height * 4` bytes.
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// The pixels, by value.
    pub fn into_rgba(self) -> Vec<u8> {
        self.rgba
    }
}

/// One owned RGBA8 picture for the source Region section (ADR-0103 decision 4).
///
/// Not a thumbnail: the long side is at most [`REGION_GRAB_LONG`], and
/// [`THUMBNAIL_MAX`] does not apply. UI-only; adapters do not log or store it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionGrab {
    size: Size,
    rgba: Vec<u8>,
}

impl RegionGrab {
    /// Builds a grab, or `None` if the size is zero, the long side is over
    /// [`REGION_GRAB_LONG`], or `rgba` is not `width * height * 4` bytes.
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Option<RegionGrab> {
        let len = u64::from(width) * u64::from(height) * 4;
        let ok = width > 0 && height > 0 && width.max(height) <= REGION_GRAB_LONG;
        (ok && len == rgba.len() as u64).then_some(RegionGrab {
            size: Size { width, height },
            rgba,
        })
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.size.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.size.height
    }

    /// The pixels, `width * height * 4` bytes.
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// The pixels, by value.
    pub fn into_rgba(self) -> Vec<u8> {
        self.rgba
    }
}

#[cfg(test)]
mod thumbnail_tests {
    use super::*;

    #[test]
    fn a_thumbnail_is_bounded_and_exact() {
        assert!(Thumbnail::new(2, 1, vec![0; 8]).is_some());
        assert!(Thumbnail::new(2, 1, vec![0; 7]).is_none());
        assert!(Thumbnail::new(0, 1, vec![]).is_none());
        assert!(Thumbnail::new(193, 1, vec![0; 193 * 4]).is_none());
        assert!(Thumbnail::new(1, 109, vec![0; 109 * 4]).is_none());
        assert!(Thumbnail::new(1, 108, vec![0; 108 * 4]).is_some());
        assert!(Thumbnail::new(192, 108, vec![0; 192 * 108 * 4]).is_some());
    }

    #[test]
    fn a_region_grab_allows_a_longer_side_than_a_thumbnail() {
        assert!(RegionGrab::new(960, 540, vec![0; 960 * 540 * 4]).is_some());
        assert!(RegionGrab::new(961, 1, vec![0; 961 * 4]).is_none());
        assert!(RegionGrab::new(192, 108, vec![0; 192 * 108 * 4]).is_some());
        assert!(Thumbnail::new(960, 540, vec![0; 960 * 540 * 4]).is_none());
    }

    #[test]
    fn a_work_area_scales_into_the_framebuffer() {
        let same = work_rect_in_framebuffer((1920, 1080), (0, 0, 1920, 1080), (0, 40, 1920, 1080));
        assert_eq!(
            same,
            Some(WorkRect {
                x: 0,
                y: 40,
                width: 1920,
                height: 1040,
            })
        );
        // Points on a 2x framebuffer become physical pixels.
        let hidpi = work_rect_in_framebuffer((3840, 2160), (0, 0, 1920, 1080), (0, 40, 1920, 1040));
        assert_eq!(
            hidpi,
            Some(WorkRect {
                x: 0,
                y: 80,
                width: 3840,
                height: 2000,
            })
        );
        assert!(
            work_rect_in_framebuffer((0, 1080), (0, 0, 1920, 1080), (0, 0, 1920, 1080)).is_none()
        );
    }
}
