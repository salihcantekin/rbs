//! The RBS scene collection (glossary: Scene collection).
//!
//! - [`model`]: canvases, sources with filters, scenes scoped to one canvas or shared,
//!   and scene items with a default layout plus per-canvas layouts (UI rule U15).
//! - [`SceneCollection`]: validated, all-or-nothing edits; each returns an [`Undo`]
//!   record that restores the previous state and yields its own redo.
//! - [`flatten`]: a scene on a canvas as the flat, bottom-first layer list the
//!   compositor draws, and the set of sources that must be Active.
//!
//! - [`binder`]: keeps a media graph description in step with a collection, with
//!   stable node ids, compositor layer lists and wanted lifecycle states.
//!
//! [`SceneCollection::from_parts`] is the validating loader for documents; the on-disk
//! format lives with the user data layer (`rbs-user-data`, ADR-0051). Spec:
//! `docs/specs/scene-model.md`.

mod arrange;
pub mod binder;
mod collection;
mod crop;
pub mod flatten;
pub mod look;
pub mod model;

pub use arrange::{AutoItem, AutoLayout, CenterAxis, FitMode, FlipState, auto_layout};
pub use collection::{
    IdCounters, MAX_CANVAS_SIDE, MAX_FRAME_RATE, MAX_LAYERS, MAX_NESTING_DEPTH, SceneCollection,
    SceneError, Undo,
};
pub use flatten::{Affine2, FlatLayer};
