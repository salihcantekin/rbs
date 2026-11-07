//! Capability keys of virtual camera adapters (spec, "Capabilities").

use rbs_core::capability::CapabilityKey;

/// Memory kinds `send` takes, a set: `system` today; native kinds come with the adapter
/// that needs them.
pub const INPUT_MEMORY: CapabilityKey = CapabilityKey::new_static("vcam.input.memory");
/// Pixel formats `send` takes, a set: `nv12`.
pub const INPUT_FORMAT: CapabilityKey = CapabilityKey::new_static("vcam.input.format");
/// Largest width announced to apps, an integer.
pub const MAX_WIDTH: CapabilityKey = CapabilityKey::new_static("vcam.max_width");
/// Largest height announced to apps, an integer.
pub const MAX_HEIGHT: CapabilityKey = CapabilityKey::new_static("vcam.max_height");
/// Frame rates announced to apps, a set of whole frames per second ("15", "30", ...).
pub const RATES: CapabilityKey = CapabilityKey::new_static("vcam.rates");
/// The camera stays in the OS camera list while RBS is not sending, a flag.
pub const PERSISTENT: CapabilityKey = CapabilityKey::new_static("vcam.persistent");
