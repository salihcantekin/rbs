//! WebRTC adapter of the RBS peer port (`rbs-peer`) on str0m, a sans-IO WebRTC
//! library (ADR-0139 decision 2). Pure-Rust crypto: see [`crypto`].
//!
//! Contract: `docs/specs/peer-port.md`.

mod crypto;
pub mod engine;
pub mod net;
pub mod payload;
#[cfg(test)]
mod sim_tests;
#[cfg(test)]
mod spike_tests;
pub mod stun;

pub use net::{TYPE_ID, WebRtcMedia, WebRtcPeerFactory};
