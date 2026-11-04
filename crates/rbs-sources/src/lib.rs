//! Scene source types (spec `docs/specs/scene-sources.md`, ADR-0039).
//!
//! A scene source names a node type and carries settings. This crate maps the built-in
//! source types (display, window, camera, audio input, system audio, application
//! audio) to graph nodes:
//!
//! - [`SourceTypes`] holds the capture and audio input adapters the composition root
//!   registers, and says which source types are installed (a type is installed when a
//!   probed adapter can capture its kind).
//! - [`SourceTypes::node_types`] is the descriptor table the
//!   [`SceneBinder`](rbs_scene::binder::SceneBinder) binds with: the installed source
//!   types, the compositor and the mixer.
//! - [`SourceTypes::instantiate`] builds the node instance for a source the binder
//!   created: a [`CaptureSourceNode`](rbs_nodes::CaptureSourceNode) for the render
//!   partition or an [`InputNode`](rbs_audio::InputNode) for the audio partition.
//! - [`node_layers`] turns a bound compositor's layers into the compositor node's
//!   layer list.
//!
//! The crate never depends on an adapter (P1): adapters come in as port factory trait
//! objects, and engine code never names a vendor (P3).

mod layers;
mod registry;
pub mod types;

pub use layers::node_layers;
pub use registry::{
    AudioSource, InstanceContext, NodeTypeTable, SourceError, SourceInstance, SourcePicker,
    SourceTarget, SourceTypes, VideoSource,
};
pub use types::{SourceClass, class_of};
