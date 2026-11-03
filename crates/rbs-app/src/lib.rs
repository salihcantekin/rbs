//! Library part of the RBS desktop app: native pieces the shell and its examples share.
//!
//! - `native::MeterBridgeLayer` (macOS): audio meters as Core Animation layers, so meter
//!   updates do not make the toolkit redraw the window (ADR-0035).

#[cfg(target_os = "macos")]
pub mod native;
