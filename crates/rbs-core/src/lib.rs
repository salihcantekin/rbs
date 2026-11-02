//! Foundation types shared by every RBS crate.
//!
//! - [`time`]: master-clock timestamps and clocks (ADR-0004).
//! - [`timing`]: exact rates, cadences, timebases and drift estimation.
//! - [`media`]: video frames, audio blocks, encoded packets and leases (ADR-0018).
//! - [`capability`]: the capability and requirement model used to pick backends (ADR-0003).
//! - [`lifecycle`]: Installed / Standby / Active states (ADR-0013).
//! - [`registry`]: adapter factories and runtime selection by capability.
//!
//! This crate must stay free of vendor, OS media, GPU and UI dependencies
//! (architecture principle P1).

pub mod capability;
pub mod lifecycle;
pub mod media;
pub mod registry;
pub mod time;
pub mod timing;
