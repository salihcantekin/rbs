//! WebRTC adapter of the RBS peer port (`rbs-peer`) on str0m, a sans-IO WebRTC
//! library (ADR-0139 decision 2). Pure-Rust crypto: see [`crypto`].
//!
//! Contract: `docs/specs/peer-port.md`.

mod crypto;
pub mod engine;
mod link;
pub mod media;
pub mod net;
pub mod payload;
#[cfg(test)]
mod sim_tests;
#[cfg(test)]
mod spike_tests;
pub mod stun;

pub use link::{GuestReceiver, WebRtcMedia, WebRtcTransport};
pub use net::{TYPE_ID, WebRtcPeerFactory};
