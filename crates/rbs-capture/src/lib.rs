//! The video capture port of RBS: displays, windows and cameras.
//!
//! Every OS capture API is an adapter implementing [`CaptureFactory`] and
//! [`CaptureSession`]. Adapters declare what they can do with the keys in [`caps`] and
//! prove it with the suite in [`conformance`]. Contract: `docs/specs/capture-port.md`.
//!
//! This crate is a port: it depends only on `rbs-core` and never on an OS API, GPU
//! library or adapter (principle P1).

pub mod caps;
pub mod conformance;
mod error;
mod session;
mod target;

pub use error::CaptureError;
pub use session::{CaptureFactory, CaptureRequest, CaptureSession, CaptureStats};
pub use target::{CameraMode, CaptureTarget, TargetId, TargetKind, TargetSelector};
