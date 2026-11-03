//! Library part of the RBS desktop app: the pieces the shell, its tests and its examples
//! share.
//!
//! - [`studio_host`]: the studio's control thread (ADR-0041 decision 7).
//! - [`platform`]: the platform's adapters (the composition root's list, P1).
//! - [`record_profile`]: the recording's output settings and their file.
//! - `native::MeterBridgeLayer` (macOS): audio meters as Core Animation layers, so meter
//!   updates do not make the toolkit redraw the window (ADR-0035).

#[cfg(target_os = "macos")]
pub mod native;
pub mod platform;
pub mod record_profile;
pub mod studio_host;
