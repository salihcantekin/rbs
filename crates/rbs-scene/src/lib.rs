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

pub mod activity;
mod arrange;
mod attributes;
pub mod binder;
mod collection;
mod crop;
pub mod flatten;
pub mod look;
pub mod model;

pub use arrange::{AutoItem, AutoLayout, CenterAxis, FitMode, FlipState, auto_layout};
pub use attributes::AttributeGroup;
pub use collection::{
    Duplicated, IdCounters, LevelEdits, MAX_CANVAS_SIDE, MAX_FRAME_RATE, MAX_LAYERS,
    MAX_NESTING_DEPTH, OnAir, Removed, SceneCollection, SceneError, SourceCopy, SourceFields, Undo,
};
pub use crop::{place_chrome, recrop_keeping, visible_size};
pub use flatten::{Affine2, FlatLayer, ScrollKey};
pub use model::{
    COMPANION_OF_HINT, ChromeFill, CollectionGlide, CollectionTransition, DISPLAY_SOURCE_TYPE,
    DisplayChrome, FadeSettings, GlideEasing, MAX_GLIDE_MS, MAX_SHOW_FOR_MS, STANDBY_HINT,
    SceneAudio, SceneLevel, SceneTransition, Scroll, ScrollDirection, ScrollRepeat, ScrollStart,
    SourceGlide, SourceRegion, StingerSettings, SwipeDirection, SwipeSettings, TransitionKind,
    show_for_is_valid,
};
