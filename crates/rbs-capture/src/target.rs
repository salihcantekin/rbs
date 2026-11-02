//! Capture targets: displays, windows and cameras.

use std::fmt;

use rbs_core::media::PixelFormat;
use rbs_core::timing::FrameRate;

/// What kind of thing a capture target is.
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
    /// All kinds, in a fixed order.
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
/// stable across restarts where the OS allows it, because scenes store it.
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

/// One mode a camera can deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraMode {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Native pixel format of the mode.
    pub pixel_format: PixelFormat,
    /// Frame rate of the mode.
    pub frame_rate: FrameRate,
}

/// A target that can be captured, as returned by
/// [`CaptureFactory::enumerate`](crate::CaptureFactory::enumerate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureTarget {
    /// Id to pass back in a [`TargetSelector::Id`].
    pub id: TargetId,
    /// What the target is.
    pub kind: TargetKind,
    /// Human readable name: monitor name, window title, camera model.
    pub name: String,
    /// Native size in pixels when known (displays and windows).
    pub size: Option<(u32, u32)>,
    /// Supported modes for cameras; empty for displays and windows.
    pub modes: Vec<CameraMode>,
}

/// Which target a session captures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetSelector {
    /// A target returned by `enumerate`.
    Id(TargetId),
    /// Let the operating system show its own picker (PipeWire portal, macOS content
    /// picker). Only valid when the adapter declares the kind in
    /// [`caps::TARGET_PICKER`](crate::caps::TARGET_PICKER).
    Picker(TargetKind),
}
