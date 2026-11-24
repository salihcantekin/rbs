//! Apple Vision person segmentation as a backend of the person mask port
//! (`rbs-segment`; ADR-0179, `docs/specs/person-mask-port.md`).
//!
//! On macOS [`VisionBackend`] runs `VNGeneratePersonSegmentationRequest` on a worker
//! thread of its own, on the Neural Engine where the system chooses it. The three
//! Vision quality levels are the port's Low, Medium and High. On other platforms the
//! same type exists and reports itself unavailable, so the composition roots list it
//! without a platform switch (principle P3).
//!
//! The first version takes small system-memory frames (`BackendInfo::input` is
//! `MemoryKind::System`), so the filter downscales on the GPU and reads that small
//! image back (the named exception of ADR-0179 decision 3). A zero-copy IOSurface path
//! follows only with measurements.

/// The type id of this backend in the registry (ADR-0162).
pub const TYPE_ID: &str = "org.rbs.segment.apple-vision";

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::VisionBackend;

#[cfg(not(target_os = "macos"))]
mod other;
#[cfg(not(target_os = "macos"))]
pub use other::VisionBackend;
