//! Well-known capability keys of the input port (table in `docs/specs/input-port.md`).

use rbs_core::capability::CapabilityKey;

/// Flag: pointer position in desktop space.
pub const POINTER: CapabilityKey = CapabilityKey::new_static("input.pointer");
/// Flag: button down and up.
pub const BUTTONS: CapabilityKey = CapabilityKey::new_static("input.buttons");
/// Flag: wheel.
pub const WHEEL: CapabilityKey = CapabilityKey::new_static("input.wheel");
/// Flag: keys (ADR-0165 phase 2; no adapter declares it yet).
pub const KEYS: CapabilityKey = CapabilityKey::new_static("input.keys");
/// Flag: the OS has a user permission for input (macOS Input Monitoring).
pub const PERMISSION: CapabilityKey = CapabilityKey::new_static("input.permission");
/// Flag: [`InputMonitor::request_permission`](crate::InputMonitor::request_permission)
/// can show the OS prompt in this process.
pub const PERMISSION_PROMPT: CapabilityKey = CapabilityKey::new_static("input.permission.prompt");
/// Flag: a grant takes effect only after the app restarts.
pub const PERMISSION_RESTART: CapabilityKey = CapabilityKey::new_static("input.permission.restart");
