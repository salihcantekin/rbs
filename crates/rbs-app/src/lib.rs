//! Library part of the RBS desktop app: the pieces the shell, its tests and its examples
//! share.
//!
//! - [`studio_host`]: the studio's control thread (ADR-0041 decision 7).
//! - [`background`]: keeping RBS running after the main window closes (ADR-0056);
//!   `win` (Windows): its tray icon and second-launch event.
//! - [`edit_history`]: the undo history of scene edits (U4).
//! - [`platform`]: the platform's adapters (the composition root's list, P1).
//! - [`popups`]: the popups Slint draws inside a window, as rectangles.
//! - [`canvas_profile`]: the canvas size and frame rate (and `video.toml`, which older
//!   versions kept).
//! - [`record_profile`]: the recording's output settings (a section of the output
//!   profile; `recording.toml` from older versions).
//! - [`stream_profile`]: where Go live sends the program, and how; [`stream_key`]: the
//!   stream key in the system's credential store.
//! - [`encoder_form`]: the encoder settings' choices, from the encoders' capabilities.
//! - [`hotkeys`]: key presses as shortcut chords, and global shortcuts (ADR-0060).
//! - [`remote_control`]: the control API server while remote control is on.
//! - [`user_session`]: the output profile and scene collection in use, and their one
//!   writer thread (ADR-0051).
//! - `native::MeterBridgeLayer` (macOS): audio meters as Core Animation layers, so meter
//!   updates do not make the toolkit redraw the window (ADR-0035).

pub mod background;
pub mod canvas_profile;
pub mod edit_history;
pub mod encoder_form;
pub mod hotkeys;
#[cfg(target_os = "macos")]
pub mod native;
pub mod network_access;
pub mod platform;
pub mod popups;
pub mod record_profile;
pub mod remote_control;
pub mod stream_key;
pub mod stream_profile;
pub mod studio_host;
pub mod user_session;
#[cfg(windows)]
pub mod win;
