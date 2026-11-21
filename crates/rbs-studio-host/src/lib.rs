//! The studio host: the control thread that owns the studio and answers the window's
//! commands and the control API (ADR-0041 decision 7, ADR-0159). No toolkit, window,
//! OS binding or adapter crate: a composition root hands it adapters through
//! [`studio_host::HostConfig`] (spec `docs/specs/studio-host.md`).
//!
//! - [`studio_host`]: the control thread.
//! - [`edit_history`]: the undo history of scene edits (U4).
//! - [`canvas_profile`]: the canvas size and frame rate (and `video.toml`, which older
//!   versions kept).
//! - [`record_profile`]: the recording's output settings (a section of the output
//!   profile; `recording.toml` from older versions).
//! - [`stream_profile`]: where Go live sends the program, and how; [`stream_key`]: the
//!   stream key store and its account names.
//! - [`api_tokens`]: the control API's `tokens.*` answers.
//! - [`ui_thread`]: the UI thread's mark, the debug check for waits, and the stall
//!   watch (docs/architecture/threads.md).
//! - [`user_session`]: the output profile and scene collection in use, and their one
//!   writer thread (ADR-0051).
//! - [`recent_targets`]: the last targets picked per source type (`recent-targets.toml`, ADR-0077).
//! - [`replay_settings`]: Settings › Recording › Replay buffer and `replay.toml` (ADR-0088).

pub mod add_ons;
pub mod api_tokens;
pub mod browser_profiles;
pub mod calibration_file;
pub mod canvas_profile;
pub mod device_sync_file;
pub mod edit_history;
pub mod levels;
pub mod loudness_settings;
pub mod monitor_settings;
pub mod obs_import;
pub mod recent_targets;
pub mod record_profile;
pub mod replay_settings;
pub mod stream_destinations;
pub mod stream_key;
pub mod stream_profile;
pub mod studio_host;
pub mod text_timers;
pub mod ui_thread;
pub mod user_session;
pub mod vertical_outputs;
