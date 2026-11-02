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
