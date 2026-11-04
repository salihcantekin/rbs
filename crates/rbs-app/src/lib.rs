//! Library part of the RBS desktop app: the pieces the shell, its tests and its examples
//! share.
//!
//! - [`studio_host`]: the studio's control thread (ADR-0041 decision 7).
//! - [`platform`]: the platform's adapters (the composition root's list, P1).
//! - [`canvas_profile`]: the canvas size and frame rate and their file.
//! - [`record_profile`]: the recording's output settings and their file.
//! - [`encoder_form`]: the encoder settings' choices, from the encoders' capabilities.
//! - [`remote_control`]: the control API server while remote control is on.
//! - `native::MeterBridgeLayer` (macOS): audio meters as Core Animation layers, so meter
//!   updates do not make the toolkit redraw the window (ADR-0035).

pub mod canvas_profile;
pub mod encoder_form;
#[cfg(target_os = "macos")]
pub mod native;
pub mod network_access;
pub mod platform;
pub mod record_profile;
pub mod remote_control;
pub mod studio_host;
