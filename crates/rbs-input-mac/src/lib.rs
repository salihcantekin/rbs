//! macOS adapter for the RBS input port (`docs/specs/input-port.md`, "macOS
//! adapter"; ADR-0165 phase 1, mouse only).
//!
//! # How events travel
//!
//! - [`MacInput::start`] spawns the thread `rbs-input-mac`. It creates a listen-only
//!   `CGEventTap` at session level (`kCGSessionEventTap`, head insert,
//!   `kCGEventTapOptionListenOnly`) for only the event types the mask needs, adds its
//!   run-loop source to the thread's run loop and runs that loop. A tap the system
//!   refuses (NULL) is [`InputError::PermissionDenied`](rbs_input::InputError).
//! - The tap callback runs on that thread. It reads the type, location
//!   (`CGEventGetLocation`, global display points with the origin at the top-left of
//!   the main display) and timestamp, brings the time to master time with an
//!   [`EdgeClock`](rbs_core::timing::EdgeClock) over mach time, and writes the
//!   pointer history and the event rings. No allocation, lock or log.
//! - A mask change or stop is posted to the run loop as a block that stops it
//!   (`CFRunLoopPerformBlock` + `CFRunLoopWakeUp`), backed by atomics the thread checks
//!   after each run, so neither is lost when it arrives before the loop runs. A new
//!   mask rebuilds the tap with the new types; the callback also filters by the wanted
//!   mask, so a narrower mask applies at once.
//! - When the system disables the tap (`kCGEventTapDisabledByTimeout`,
//!   `kCGEventTapDisabledByUserInput`) the callback enables it again and counts it; the
//!   counts are logged when the tap stops.
//!
//! A listen-only tap needs no Accessibility permission, but on macOS 26 it needs
//! Input Monitoring even for mouse events: without it the tap is created and receives
//! nothing, so `start` checks `CGPreflightListenEventAccess` (which never prompts) and
//! returns `PermissionDenied`. Asking for the permission is the host's job (ADR-0141).
//! On other platforms this crate is empty, so the workspace builds everywhere.

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod model;

#[cfg(target_os = "macos")]
mod mac;

#[cfg(target_os = "macos")]
pub use mac::{MacInput, TapStats, codes};

/// Adapter name reported by [`rbs_input::InputBackend::name`].
pub const NAME: &str = "cg-event-tap";
