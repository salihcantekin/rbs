//! Capture targets: displays, windows and cameras.

use std::fmt;

use rbs_core::media::PixelFormat;
use rbs_core::timing::FrameRate;

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
    pub size: Option<(u32, u32)>,
    /// Supported modes for cameras; empty for displays and windows.
    pub modes: Vec<CameraMode>,
    /// Name of the application that owns the window, when the OS tells it
    /// (`None` for displays, cameras and adapters that cannot know). Only for
    /// grouping and display; hosts never branch on its value (P3).
    pub app: Option<String>,
}

impl CaptureTarget {
    /// A target with no known size and no modes.
    pub fn new(id: TargetId, kind: TargetKind, name: impl Into<String>) -> Self {
        Self {
            id,
            kind,
            name: name.into(),
            size: None,
            modes: Vec::new(),
            app: None,
        }
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
pub const THUMBNAIL_MAX: (u32, u32) = (192, 108);

/// A small owned RGBA8 picture of a target, for browsing a list (never on the frame path).
///
/// A bounded exception to P4: it is one picture on request, at most [`THUMBNAIL_MAX`],
/// and is never stored or sent anywhere.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    /// Width in pixels, at least 1.
    pub width: u32,
    /// Height in pixels, at least 1.
    pub height: u32,
    /// `width * height * 4` bytes, RGBA8, straight alpha, rows top to bottom, no padding.
    pub rgba: Vec<u8>,
}

impl Thumbnail {
    /// Builds a thumbnail, or `None` if the size is zero, larger than [`THUMBNAIL_MAX`]
    /// or does not match the length of `rgba`.
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Option<Thumbnail> {
        let (mw, mh) = THUMBNAIL_MAX;
        let len = u64::from(width) * u64::from(height) * 4;
        let ok = width > 0 && height > 0 && width <= mw && height <= mh;
        (ok && len == rgba.len() as u64).then_some(Thumbnail {
            width,
            height,
            rgba,
        })
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
        assert!(Thumbnail::new(192, 108, vec![0; 192 * 108 * 4]).is_some());
    }
}
