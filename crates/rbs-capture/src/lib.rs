//! The video capture port of RBS: displays, windows and cameras.
//!
//! Every OS capture API is an adapter implementing [`CaptureFactory`] and
//! [`CaptureSession`]. Adapters declare what they can do with the keys in [`caps`] and
//! prove it with the suite in [`conformance`]. Contract: `docs/specs/capture-port.md`.
//!
//! This crate is a port: it depends only on `rbs-core` and never on an OS API, GPU
//! library or adapter (principle P1).

pub mod camera;
pub mod caps;
pub mod conformance;
mod error;
mod framebuffer;
pub mod handoff;
mod session;
mod target;

pub use error::{CaptureError, ErrorDomain};
pub use framebuffer::{DisplayReading, DisplayScale, FramebufferChoice, choose_framebuffer};
pub use session::{
    CaptureChange, CaptureFactory, CaptureRequest, CaptureSession, CaptureStats, GpuDeviceId,
    GpuImportTarget, SyncKind,
};
pub use target::{
    CameraMode, CaptureTarget, Permission, RelatedDevice, RelatedKind, THUMBNAIL_MAX, TargetId,
    TargetKind, Thumbnail,
};
