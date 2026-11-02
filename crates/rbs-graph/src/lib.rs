//! The RBS media graph model.
//!
//! - [`node`]: node kinds, pins and descriptors.
//! - [`lifecycle`]: the Installed / Standby / Active states (ADR-0013).
//! - [`description`]: a graph description built off the real-time threads, its
//!   validation, and the [`description::SchedulePlan`] the render and audio threads run.
//!
//! The scheduler and the `Node` trait arrive in later changes. Spec:
//! `docs/specs/media-graph.md`.

pub mod description;
pub mod lifecycle;
pub mod node;
