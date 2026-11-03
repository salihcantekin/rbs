//! The built-in source type ids and their settings keys.

use rbs_audio_io::EndpointKind;
use rbs_capture::TargetKind;
use rbs_graph::node::NodeTypeId;

/// A whole display (monitor).
pub const DISPLAY_SOURCE: &str = "org.rbs.source.display";
/// One top-level window.
pub const WINDOW_SOURCE: &str = "org.rbs.source.window";
/// A camera or other video input device.
pub const CAMERA_SOURCE: &str = "org.rbs.source.camera";
/// A microphone, line input or audio interface input.
pub const AUDIO_INPUT_SOURCE: &str = "org.rbs.source.audio-input";
/// What an output device plays (system audio).
pub const AUDIO_OUTPUT_SOURCE: &str = "org.rbs.source.audio-output";
/// The audio one application plays.
pub const APPLICATION_AUDIO_SOURCE: &str = "org.rbs.source.application-audio";

/// Every built-in source type, in a fixed order.
pub const ALL: [&str; 6] = [
    DISPLAY_SOURCE,
    WINDOW_SOURCE,
    CAMERA_SOURCE,
    AUDIO_INPUT_SOURCE,
    AUDIO_OUTPUT_SOURCE,
    APPLICATION_AUDIO_SOURCE,
];

/// Settings keys. Keys a type does not use are ignored, so a document written by a
/// newer version still loads.
pub mod keys {
    /// Text, every type: the adapter id. Missing: the best adapter for the kind.
    /// Target ids are adapter-defined, so a stored target also stores its adapter.
    pub const ADAPTER: &str = "adapter";
    /// Text, every type: the capture target id or audio endpoint id. Missing: the
    /// default (see [`SourceTypes::instantiate`](crate::SourceTypes::instantiate)).
    pub const TARGET: &str = "target";
    /// Bool, display and window: draw the cursor. Default true.
    pub const CURSOR: &str = "cursor";
    /// Int, display, window and camera: requested width in pixels (with `height`).
    pub const WIDTH: &str = "width";
    /// Int, display, window and camera: requested height in pixels (with `width`).
    pub const HEIGHT: &str = "height";
    /// Int, display, window and camera: requested frames per second.
    pub const FPS: &str = "fps";
}

/// What a source type captures.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceClass {
    /// Video from the capture port.
    Capture(TargetKind),
    /// Audio from the audio input port.
    Audio(EndpointKind),
}

/// The class of a built-in source type, or `None` for any other type.
pub fn class_of(type_id: &NodeTypeId) -> Option<SourceClass> {
    Some(match type_id.as_str() {
        DISPLAY_SOURCE => SourceClass::Capture(TargetKind::Display),
        WINDOW_SOURCE => SourceClass::Capture(TargetKind::Window),
        CAMERA_SOURCE => SourceClass::Capture(TargetKind::Camera),
        AUDIO_INPUT_SOURCE => SourceClass::Audio(EndpointKind::Input),
        AUDIO_OUTPUT_SOURCE => SourceClass::Audio(EndpointKind::OutputLoopback),
        APPLICATION_AUDIO_SOURCE => SourceClass::Audio(EndpointKind::Application),
        _ => return None,
    })
}
