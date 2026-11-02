//! The RBS scene collection (glossary: Scene collection).
//!
//! - [`model`]: canvases, sources with filters, scenes scoped to one canvas or shared,
//!   and scene items with a default layout plus per-canvas layouts (UI rule U15).
//! - [`SceneCollection`]: validated, all-or-nothing edits; each returns an [`Undo`]
//!   record that restores the previous state and yields its own redo.
//! - [`flatten`]: a scene on a canvas as the flat, bottom-first layer list the
//!   compositor draws, and the set of sources that must be Active.
//!
//! Turning a collection into a media graph description and the on-disk document format
//! come in later changes. Spec: `docs/specs/scene-model.md`.

mod collection;
pub mod flatten;
pub mod model;

pub use collection::{MAX_LAYERS, MAX_NESTING_DEPTH, SceneCollection, SceneError, Undo};
pub use flatten::{Affine2, FlatLayer};
