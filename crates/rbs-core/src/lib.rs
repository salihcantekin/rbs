//! Foundation types shared by every RBS crate.
//!
//! - [`time`]: master-clock timestamps and clocks (ADR-0004).
//! - [`capability`]: the capability and requirement model used to pick backends (ADR-0003).
//! - [`registry`]: adapter factories and runtime selection by capability.
//!
//! This crate must stay free of vendor, OS media, GPU and UI dependencies
//! (architecture principle P1).

pub mod capability;
pub mod registry;
pub mod time;
