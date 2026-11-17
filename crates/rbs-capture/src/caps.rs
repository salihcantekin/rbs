//! Well-known capability keys of the capture port.
//!
//! See the table in `docs/specs/capture-port.md`. Symbol values in sets use
//! [`TargetKind::as_str`](crate::TargetKind::as_str) for kinds and the names defined
//! by `rbs_core::media` for pixel formats and memory kinds.

use rbs_core::capability::CapabilityKey;

/// Set of target kinds `open` accepts.
pub const TARGET_KINDS: CapabilityKey = CapabilityKey::new_static("capture.target.kinds");
/// Set of target kinds `enumerate` can list.
pub const TARGET_ENUMERATE: CapabilityKey = CapabilityKey::new_static("capture.target.enumerate");
/// Set of target kinds `CaptureFactory::pick` supports (OS picker).
pub const TARGET_PICKER: CapabilityKey = CapabilityKey::new_static("capture.target.picker");
/// Set of pixel formats the adapter can deliver natively.
pub const PIXEL_FORMATS: CapabilityKey = CapabilityKey::new_static("capture.pixel_formats");
/// Set of memory kinds frames can be delivered in.
pub const MEMORY: CapabilityKey = CapabilityKey::new_static("capture.memory");
/// Flag: the OS can composite the cursor into frames.
pub const CURSOR: CapabilityKey = CapabilityKey::new_static("capture.cursor");
/// Int: highest frame rate the adapter can deliver, in frames per second.
pub const FRAME_RATE_MAX: CapabilityKey = CapabilityKey::new_static("capture.frame_rate.max");
/// Int: frames the consumer may hold at once.
pub const FRAMES_IN_FLIGHT: CapabilityKey = CapabilityKey::new_static("capture.frames.in_flight");
/// Flag: HDR delivery for HDR displays.
pub const HDR: CapabilityKey = CapabilityKey::new_static("capture.hdr");

/// Flag: the OS has a user permission for capturing cameras (ADR-0141).
pub const PERMISSION_CAMERA: CapabilityKey = CapabilityKey::new_static("capture.permission.camera");
/// Flag: the OS has a user permission for capturing displays (ADR-0141).
pub const PERMISSION_DISPLAY: CapabilityKey =
    CapabilityKey::new_static("capture.permission.display");
/// Flag: the OS has a user permission for capturing windows (ADR-0141).
pub const PERMISSION_WINDOW: CapabilityKey = CapabilityKey::new_static("capture.permission.window");
/// Flag: a camera permission granted while the app runs takes effect only after a restart.
pub const PERMISSION_CAMERA_RESTART: CapabilityKey =
    CapabilityKey::new_static("capture.permission.camera.restart");
/// Flag: a display permission granted while the app runs takes effect only after a
/// restart (macOS screen recording).
pub const PERMISSION_DISPLAY_RESTART: CapabilityKey =
    CapabilityKey::new_static("capture.permission.display.restart");
/// Flag: a window permission granted while the app runs takes effect only after a restart.
pub const PERMISSION_WINDOW_RESTART: CapabilityKey =
    CapabilityKey::new_static("capture.permission.window.restart");
/// Flag: `request_permission(Camera)` can show the OS prompt in this process now.
pub const PERMISSION_CAMERA_PROMPT: CapabilityKey =
    CapabilityKey::new_static("capture.permission.camera.prompt");
/// Flag: `request_permission(Display)` can show the OS prompt in this process now.
pub const PERMISSION_DISPLAY_PROMPT: CapabilityKey =
    CapabilityKey::new_static("capture.permission.display.prompt");
/// Flag: `request_permission(Window)` can show the OS prompt in this process now.
pub const PERMISSION_WINDOW_PROMPT: CapabilityKey =
    CapabilityKey::new_static("capture.permission.window.prompt");
/// Flag: the OS asks the user in its own dialog on every pick (the desktop portal), so
/// there is no standing permission to show (ADR-0141).
pub const PICK_CONSENT: CapabilityKey = CapabilityKey::new_static("capture.pick.consent");

/// The permission keys of one target kind: (has a permission, applies after a restart,
/// can prompt now).
pub fn permission_keys(kind: crate::TargetKind) -> (CapabilityKey, CapabilityKey, CapabilityKey) {
    match kind {
        crate::TargetKind::Camera => (
            PERMISSION_CAMERA,
            PERMISSION_CAMERA_RESTART,
            PERMISSION_CAMERA_PROMPT,
        ),
        crate::TargetKind::Display => (
            PERMISSION_DISPLAY,
            PERMISSION_DISPLAY_RESTART,
            PERMISSION_DISPLAY_PROMPT,
        ),
        crate::TargetKind::Window => (
            PERMISSION_WINDOW,
            PERMISSION_WINDOW_RESTART,
            PERMISSION_WINDOW_PROMPT,
        ),
    }
}
