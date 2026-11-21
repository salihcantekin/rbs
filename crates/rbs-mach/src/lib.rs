//! Mach port and message helpers (macOS).
//!
//! Shared by the cross-process preview (ADR-0028, `rbs-gpu` `share`) and the browser
//! surface handover (ADR-0167, [`browser`]), so both sides of each protocol use one
//! copy of the port calls. The crate has no dependency beyond `mach2` and builds
//! empty on other systems.
//!
//! Marked low-level crate (principle P11, CLAUDE.md rule 7): every `unsafe` block
//! states its invariant.

#![cfg_attr(not(target_vendor = "apple"), allow(unused))]

#[cfg(target_vendor = "apple")]
mod port;
#[cfg(target_vendor = "apple")]
pub use port::*;

#[cfg(target_vendor = "apple")]
pub mod browser;

/// A failed Mach call: what failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachError(pub &'static str);

impl std::fmt::Display for MachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for MachError {}
