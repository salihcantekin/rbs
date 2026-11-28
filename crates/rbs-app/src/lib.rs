//! Library part of the RBS desktop app: the pieces the shell, its tests and its examples
//! share.
//!
//! - [`studio_host`]: the studio's control thread (ADR-0041 decision 7), from
//!   `rbs-studio-host` (ADR-0159) like the settings modules re-exported below.
//! - [`background`]: keeping RBS running after the main window closes (ADR-0056);
//!   `win` (Windows): its tray icon and second-launch event.
//! - [`edit_history`]: the undo history of scene edits (U4).
//! - [`platform`]: the platform's adapters (the composition root's list, P1).
//! - [`popups`]: the popups Slint draws inside a window, as rectangles.
//! - [`caption`]: the merged title bar's hit test on Windows (ADR-0071); `win` has its
//!   Win32 part.
//! - [`canvas_profile`]: the canvas size and frame rate (and `video.toml`, which older
//!   versions kept).
//! - [`record_profile`]: the recording's output settings (a section of the output
//!   profile; `recording.toml` from older versions).
//! - [`stream_profile`]: where Go live sends the program, and how; [`stream_key`]: the
//!   stream key in the system's credential store.
//! - [`encoder_form`]: the encoder settings' choices, from the encoders' capabilities.
//! - [`hotkeys`]: key presses as shortcut chords, and global shortcuts (ADR-0060).
//! - [`remote_control`]: the control API server while remote control is on.
//! - [`ui_thread`]: the UI thread's mark, the debug check for waits, and the stall
//!   watch (docs/architecture/threads.md).
//! - [`user_session`]: the output profile and scene collection in use, and their one
//!   writer thread (ADR-0051).
//! - [`latest`]: the latest-only frame hand-off to the UI thread (stability plan WP-J).
//! - [`recent_targets`]: the last targets picked per source type (`recent-targets.toml`, ADR-0077).
//! - [`replay_settings`]: Settings › Recording › Replay buffer and `replay.toml` (ADR-0088).
//! - [`guest_settings`]: Settings › Guests and `guest.toml` (ADR-0139).
//! - [`guest_view`]: the Share and Add guest window's pages and the control-API calls it
//!   makes (ADR-0139 phase 1d).
//! - [`vcam_settings`]: Settings › Virtual camera and `vcam.toml` (ADR-0066).
//! - `native::MeterBridgeLayer` (macOS): audio meters as Core Animation layers, so meter
//!   updates do not make the toolkit redraw the window (ADR-0035).

pub mod add_on_text;
pub mod add_ons;
pub mod ai_tools;
pub mod background;
#[cfg(windows)]
mod browser_gpu;
#[cfg(target_os = "macos")]
mod browser_metal;
pub use rbs_studio_host::browser_profiles;
pub mod browser_tree;
pub use rbs_studio_host::calibration_file;
pub use rbs_studio_host::canvas_profile;
pub mod caption;
pub mod chapters;
pub use rbs_studio_host::device_sync_file;
pub mod diag_log;
pub use rbs_studio_host::edit_history;
pub mod encoder_form;
pub mod hotkeys;
pub mod instance;
pub mod latest;
pub use rbs_studio_host::levels;
pub use rbs_studio_host::loudness_settings;
pub mod menu_layout;
pub use rbs_studio_host::monitor_settings;
#[cfg(target_os = "macos")]
pub mod native;
pub mod network_access;
pub use rbs_studio_host::obs_import;
#[cfg_attr(not(windows), allow(dead_code))] // Used by the Windows GPU page path.
mod page_grave;
pub mod permission_text;
pub mod platform;
pub mod popups;
pub use rbs_studio_host::recent_targets;
pub use rbs_studio_host::record_profile;
pub mod remote_control;
pub use rbs_studio_host::replay_settings;
pub mod segment;
pub mod segment_models;
pub mod shutdown;
pub use rbs_studio_host::stream_destinations;
pub mod stream_key;
pub use rbs_studio_host::stream_profile;
pub mod stream_services;
pub use rbs_studio_host::studio_host;
pub mod system_probe;
pub use rbs_studio_host::text_timers;
pub mod theme_hints;
pub mod thumbnail;
pub use rbs_studio_host::ui_thread;
pub use rbs_studio_host::user_session;
pub mod guest_settings;
pub mod guest_view;
pub mod vcam_settings;
pub use rbs_studio_host::vertical_outputs;
#[cfg(windows)]
pub mod win;
pub mod window_events;
