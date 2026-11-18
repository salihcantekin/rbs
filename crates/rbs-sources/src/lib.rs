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

mod browser;
mod browser_sound;
mod filters;
mod layers;
pub mod permissions;
mod registry;
pub mod still;
pub mod stinger;
pub mod text;
pub mod text_file;
pub mod text_live;
pub mod text_resize;
pub mod text_worker;
pub mod types;

pub use browser::{
    BROWSER_CAP, BROWSER_KEYS, BROWSER_NODE_TYPE, BROWSER_RUNTIME_MISMATCH,
    BROWSER_RUNTIME_MISSING, BrowserBudget, BrowserClaim, BrowserControlSidecar, BrowserFrameSink,
    BrowserGrant, BrowserPictureEdge, BrowserScheme, BrowserSession, BrowserSettings,
    BrowserSourceNode, BrowserStartError, BrowserTree, DEFAULT_ZOOM, HELPER_EXIT, HELPER_MISSING,
    MAX_CSS_BYTES, MAX_HELPERS, MAX_SCRIPT_BYTES, NETWORK_KEYS as BROWSER_NETWORK_KEYS,
    NOT_PERMITTED, ShowClass, SourceReach, agent_may_set_browser_url, browser_descriptor,
    browser_scheme, browser_settings, browser_url_is_network, file_directory_scope, is_profile_id,
};
pub use browser_sound::{
    ARRIVAL as BROWSER_SOUND_ARRIVAL, BROWSER_SOUND_NODE_TYPE, BrowserSound, BrowserSoundFeed,
    BrowserSoundNode, BrowserSoundStats, browser_sound_descriptor,
};
pub use filters::{filter_settings, filter_values, filter_values_lenient};
pub use layers::{GLIDE, glide_layers, glide_max_duration, gpu_easing, node_layers, resolve_slide};
pub use registry::{
    AudioSource, InstanceContext, MonitorDevice, NodeTypeTable, PermissionRequest, SourceError,
    SourceInstance, SourceMode, SourcePicker, SourceTarget, SourceTypes, StillSource, TextSource,
    VideoSource, apply_media_live, option_keys, validate_settings,
};
pub use types::{SourceClass, class_of};

/// The separate-recording mark a source keeps after its settings become `settings`
/// (ADR-0129 decision 1, "marks follow stillness"): a picture marked `video_as: image`
/// that is no longer still goes back to `auto`, so a take is never refused for it.
/// `None` when the mark stays as it is.
pub fn mark_after_edit(
    type_id: &rbs_graph::node::NodeTypeId,
    settings: &rbs_scene::model::Settings,
    mark: rbs_scene::model::RecordSeparately,
) -> Option<rbs_scene::model::RecordSeparately> {
    use rbs_scene::model::VideoAs;
    (mark.video_as == VideoAs::Image && !is_still(type_id, settings))
        .then(|| mark.with_video_as(VideoAs::Auto))
}

/// Whether a source of `type_id` with `settings` shows a picture that changes only
/// when its settings change (ADR-0129 decision 1, superseding ADR-0117 decision 2 in
/// part). The class answers for every type except text, whose settings answer.
/// Engine, record and host code ask this, never a type id.
pub fn is_still(
    type_id: &rbs_graph::node::NodeTypeId,
    settings: &rbs_scene::model::Settings,
) -> bool {
    match class_of(type_id) {
        Some(SourceClass::Still) => true,
        Some(SourceClass::Text) => text::text_is_still(settings),
        _ => false,
    }
}
