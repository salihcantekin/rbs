//! Windows adapter for the RBS input port (`docs/specs/input-port.md`, "Windows
//! adapter"; ADR-0165 phase 1, mouse only).
//!
//! # How events travel
//!
//! - [`WinInput::start`] spawns the thread `rbs-input-win`. It makes itself per-monitor
//!   DPI aware (v2), creates a message-only window and registers it for the mouse with
//!   Raw Input (`RIDEV_INPUTSINK`, so events arrive while another app has the focus),
//!   then runs a message loop.
//! - `WM_INPUT` is handled in that loop: `GetRawInputData` into a stack buffer, the
//!   button and wheel flags decoded, the position read with `GetCursorPos` (physical
//!   pixels of the virtual screen, the `DesktopPoint` space), the time read from QPC
//!   and brought to master time with an [`EdgeClock`](rbs_core::timing::EdgeClock).
//!   No allocation, lock or log.
//! - A mask change is an atomic the handler reads; Raw Input always delivers the whole
//!   mouse, so nothing is re-registered. `stop` posts a private message to the window
//!   and joins the thread, which removes the registration and destroys the window.
//!
//! Raw Input needs no permission. It sees neither the secure desktop (UAC, sign-in)
//! nor, from a process that is not elevated, input to an elevated window (UIPI). On
//! other platforms this crate is empty, so the workspace builds everywhere.

#[cfg_attr(not(windows), allow(dead_code))]
mod model;

#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use win::{InputStats, WinInput, codes};

/// Adapter name reported by [`rbs_input::InputBackend::name`].
pub const NAME: &str = "raw-input";
