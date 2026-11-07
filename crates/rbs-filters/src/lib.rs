//! Built-in GPU video filters (spec `docs/specs/video-filters.md`, ADR-0062).
//!
//! A filter type ([`FilterType`]) is a schema, one or two WGSL passes and a pure
//! function from values to uniforms. [`FilterNode`] runs any filter type as a
//! `VideoFilter` node on the render executor: textures of its input's size from the
//! pool, settings through its parameter channel (ADR-0023), no readback (P4).
//! [`catalogue`] lists the built-in types; [`conformance`] is the suite every type
//! passes.

pub mod catalogue;
pub mod conformance;
pub mod node;
pub mod types;

pub use catalogue::{catalogue, find};
pub use node::{FilterNode, FilterStats, PRELUDE, descriptor};
pub use types::{
    Field, FieldKind, FilterError, FilterType, MAX_FIELDS, MAX_PASSES, Pass, PassSource, Scale,
    Uniforms, Value, Values, color_text, parse_color,
};
