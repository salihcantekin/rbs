//! The video capture port of RBS: displays, windows and cameras.
//!
//! Every OS capture API is an adapter implementing [`CaptureFactory`] and
//! [`CaptureSession`]. Adapters declare what they can do with the keys in [`caps`] and
//! prove it with the suite in [`conformance`]. [`edge`] converts an OS capture time to
//! master time; adapters call it instead of pasting the conversion. Contract:
//! `docs/specs/capture-port.md`.
//!
//! This crate is a port: it depends only on `rbs-core` and never on an OS API, GPU
//! library or adapter (principle P1).

pub mod camera;
pub mod caps;
pub mod conformance;
/// The clock edge of a capture adapter. Moved to [`rbs_core::timing`] by ADR-0165;
/// re-exported here so adapters keep `rbs_capture::edge::EdgeClock`.
pub mod edge {
    pub use rbs_core::timing::EdgeClock;
}
mod error;
mod framebuffer;
pub mod handoff;
mod mapping;
mod session;
mod target;

pub use error::{CaptureError, ErrorDomain};
pub use framebuffer::{DisplayReading, DisplayScale, FramebufferChoice, choose_framebuffer};
pub use mapping::{Cover, MappingCell, MappingRegistration, MappingSwitch, PictureMapping};
pub use session::{
    CaptureChange, CaptureFactory, CaptureRequest, CaptureSession, CaptureStats, GpuDeviceId,
    GpuImportTarget, SyncKind,
};
pub use target::{
    CameraMode, CaptureTarget, Permission, REGION_GRAB_LONG, RegionGrab, RelatedDevice,
    RelatedKind, THUMBNAIL_MAX, TargetId, TargetKind, Thumbnail, WorkRect,
    work_rect_in_framebuffer,
};
