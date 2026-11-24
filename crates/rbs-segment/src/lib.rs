//! The person mask port of RBS: turn a camera frame into a matte, one coverage value
//! per pixel (1 for the person, 0 for the background).
//!
//! Every way of producing a matte (an OS API such as Apple Vision, a network run on
//! the GPU by RBS, an inference runtime) is a backend implementing
//! [`PersonMaskBackend`] and [`PersonMaskSession`]. Backends declare what they offer
//! in [`BackendInfo`], and prove the contract with the suite in [`conformance`].
//! Contract: `docs/specs/person-mask-port.md`; decisions: ADR-0179.
//!
//! This crate is a port: it depends only on `rbs-core` and never on an OS API, GPU
//! library, model runtime or adapter (principle P1).

pub mod conformance;
mod error;
mod info;
mod matte;
mod session;

pub use error::SegmentError;
pub use info::{BackendInfo, ComputeUnit, CostHint, LevelId, QualityLevel};
pub use matte::{GpuMatte, Matte, MattePlane, MatteRelease};
pub use session::{
    ModelRef, PersonMaskBackend, PersonMaskSession, SegmentHost, SessionRequest, SubmitOutcome,
};
