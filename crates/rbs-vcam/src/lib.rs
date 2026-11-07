//! The virtual camera port of RBS: RBS's picture offered to other apps as a camera
//! (ADR-0066). Contract: `docs/specs/virtual-camera.md`.
//!
//! Every OS mechanism is an adapter implementing [`VirtualCameraFactory`] and
//! [`VirtualCameraSession`]; the engine never sees the OS API. Adapters declare what they
//! take with the keys in [`caps`], and build their session on [`Handoff`], the
//! non-blocking three-slot hand-off from the render thread to their sender thread.
//!
//! This crate is a port: it depends only on `rbs-core` and never on an OS API, GPU
//! library or adapter (principle P1).

pub mod caps;
pub mod conformance;
mod error;
mod handoff;
pub mod mailbox;
mod session;

pub use error::VcamError;
pub use handoff::{Handoff, HandoffReceiver, SlotToken};
pub use session::{
    CameraFormat, InstallState, VcamCounters, VcamStats, VirtualCameraConfig, VirtualCameraFactory,
    VirtualCameraSession,
};
