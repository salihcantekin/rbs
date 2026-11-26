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

/// A picture file or a built-in sample picture (ADR-0074).
pub const IMAGE_SOURCE: &str = "org.rbs.source.image";
/// A solid colour or a two-colour gradient (ADR-0074).
pub const COLOR_SOURCE: &str = "org.rbs.source.color";
/// One local video, GIF, APNG, or animated WebP file (ADR-0108).
pub const MEDIA_SOURCE: &str = "org.rbs.source.media";
/// A page rendered by a helper tree (ADR-0109). Network-capable, including `file`.
pub const BROWSER_SOURCE: &str = "org.rbs.source.browser";
/// A text block, a clock or a timer drawn by a text renderer (ADR-0129). Installed only
/// when a renderer is registered.
pub const TEXT_SOURCE: &str = "org.rbs.source.text";
/// A folder of PNG and JPEG pictures shown in turn (ADR-0171).
pub const SLIDESHOW_SOURCE: &str = "org.rbs.source.slideshow";

/// Every built-in source type, in a fixed order.
pub const ALL: [&str; 12] = [
    DISPLAY_SOURCE,
    WINDOW_SOURCE,
    CAMERA_SOURCE,
    AUDIO_INPUT_SOURCE,
    AUDIO_OUTPUT_SOURCE,
    APPLICATION_AUDIO_SOURCE,
    IMAGE_SOURCE,
    COLOR_SOURCE,
    MEDIA_SOURCE,
    BROWSER_SOURCE,
    TEXT_SOURCE,
    SLIDESHOW_SOURCE,
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
    /// Bool, system audio (output loopback): leave RBS's own sound out of the capture
    /// (ADR-0096). Default false; honoured only where the adapter reports
    /// `audio.loopback.exclude_self`. The "Capture audio too" companion of a display sets it.
    pub const EXCLUDE_OWN_PROCESS: &str = "exclude_own_process";
    /// Int, display, window and camera: requested width in pixels (with `height`).
    pub const WIDTH: &str = "width";
    /// Int, display, window and camera: requested height in pixels (with `width`).
    pub const HEIGHT: &str = "height";
    /// Int, display, window and camera: requested frames per second.
    pub const FPS: &str = "fps";
    /// Text, camera: the colour matrix of its Y'CbCr frames, `bt709` or `bt601`.
    /// Absent: Automatic, what the device reports (ADR-0175).
    pub const COLOR_MATRIX: &str = "color_matrix";
    /// Text, camera: the code range of its Y'CbCr frames, `limited` or `full`.
    /// Absent: Automatic, what the device reports (ADR-0175).
    pub const COLOR_RANGE: &str = "color_range";
    /// Int, color: the item width Reset transform gives the source, in canvas pixels
    /// (with `box_height`). A Library shape or plain frame stores the box it was added
    /// with, since its picture is only a 2 x 2 fill that the item's box stretches.
    pub const BOX_WIDTH: &str = "box_width";
    /// Int, color: the item height Reset transform gives the source (with `box_width`).
    pub const BOX_HEIGHT: &str = "box_height";
    /// Text, image: path of a PNG file. Wins over `sample`.
    pub const PATH: &str = "path";
    /// Text, image: id of a built-in sample picture (`still::SAMPLES`).
    pub const SAMPLE: &str = "sample";
    /// Text, color: `#rrggbb` or `#rrggbbaa`. Default `#000000`.
    pub const COLOR: &str = "color";
    /// Text, color: second colour; present makes a gradient.
    pub const COLOR2: &str = "color2";
    /// Int, color: gradient angle in degrees, 0 = top to bottom. Default 0.
    pub const ANGLE: &str = "angle";
    /// Bool, color: draw the fill. Absent or true fills the shape; false leaves the
    /// inside empty (the stored colour stays, so turning it back on restores it).
    pub const FILLED: &str = "filled";
    /// Bool, media: play again at the end. Default true. Does not by itself require a
    /// new node; the binder still replaces on any settings edit until a parameter
    /// channel exists (ADR-0108 decision 7).
    pub const LOOP: &str = "loop";
    /// Bool, media: seek to the start on the next wake. Default true. Read on wake,
    /// not by replacing the node.
    pub const RESTART_WHEN_SHOWN: &str = "restart_when_shown";
    /// Int, media: where the play range starts, in milliseconds of media time.
    /// Default 0 (ADR-0108 decision 11).
    pub const START_MS: &str = "start_ms";
    /// Int, media: where the play range ends, in milliseconds of media time; absent
    /// for the end of the file. Greater than `start_ms` (ADR-0108 decision 11).
    pub const END_MS: &str = "end_ms";
    /// Text, browser: `http`, `https`, or `file` URL. Network-capable even for `file`.
    pub const URL: &str = "url";
    /// Bool, browser: keep straight alpha. Default false, which forces opaque alpha.
    pub const TRANSPARENT: &str = "transparent";
    /// Text, browser: custom CSS, at most 64 KiB (ADR-0132 decision 4). Default empty.
    pub const CSS: &str = "css";
    /// Bool, browser: reload a `file` page when its directory changes (ADR-0132 decision 5).
    pub const RELOAD_ON_FILE_CHANGE: &str = "reload_on_file_change";
    /// Bool, browser: keep this source's sign-in in its own profile (ADR-0132 decision 10).
    pub const KEEP_SIGNED_IN: &str = "keep_signed_in";
    /// Text, browser: 32 lowercase hex digits naming the kept profile; set by the
    /// server only, never by a client (ADR-0132 decision 10).
    pub const PROFILE_ID: &str = "profile_id";
    /// Int, browser: reload every N seconds, 0 or 10..=86400 (ADR-0149 decision 2).
    pub const RELOAD_EVERY_S: &str = "reload_every_s";
    /// Text, browser: the user's own script, at most 32 768 bytes (ADR-0149 decision 3).
    pub const SCRIPT: &str = "script";
    /// Bool, browser: run `script` after each load (ADR-0149 decision 3).
    pub const SCRIPT_ON_LOAD: &str = "script_on_load";
    /// Int, browser: run `script` every N seconds, 0 or 1..=86400 (ADR-0149 decision 3).
    pub const SCRIPT_EVERY_S: &str = "script_every_s";
    /// Int, browser: page zoom in percent, 25..=500. Default 100 (ADR-0149 decision 4).
    pub const ZOOM_PERCENT: &str = "zoom_percent";
    /// Bool, browser: stay Active while placed anywhere (ADR-0149 decision 5).
    pub const KEEP_RUNNING: &str = "keep_running";
    /// Bool, browser: keep error pages off air and retry (ADR-0149 decision 6). Default true.
    pub const RETRY_ON_ERROR: &str = "retry_on_error";

    // Text source (ADR-0129 decisions 3 and 11; ranges in `rbs_text`). `width` and
    // `height` are the box, `color`, `color2` and `angle` the fill, `target` a
    // countdown's time of day.
    /// Text, text: `text`, `clock`, `countdown`, `stopwatch`, `recording_time` or
    /// `stream_time`. Default `text`.
    pub const KIND: &str = "kind";
    /// Text, text: the text; `\n` breaks a line. At most 10 000 characters.
    pub const TEXT: &str = "text";
    /// Text, text: a local file whose content is shown instead of `text`.
    pub const FILE: &str = "file";
    /// Int, text: 0 = the whole file, N = its last N lines.
    pub const FILE_LINES: &str = "file_lines";
    /// Text, text: font family. Absent: the system UI font.
    pub const FONT: &str = "font";
    /// Float, text: font size in pixels, 1 to 1024. Default 72.
    pub const SIZE: &str = "size";
    /// Int, text: font weight, 100 to 900. Default 400.
    pub const WEIGHT: &str = "weight";
    /// Bool, text: italic.
    pub const ITALIC: &str = "italic";
    /// Text, text: `left`, `center` or `right`. Default `center`.
    pub const ALIGN: &str = "align";
    /// Text, text: `top`, `middle` or `bottom`. Default `middle`.
    pub const VALIGN: &str = "valign";
    /// Bool, text: break lines at the box width. Default true.
    pub const WRAP: &str = "wrap";
    /// Bool, text: one line whose picture is as wide as the text (ADR-0150 decision
    /// 10). Default false.
    pub const SINGLE_LINE: &str = "single_line";
    /// Text, text: `fit`, `clip` or `ellipsis`. Default `fit`.
    pub const OVERFLOW: &str = "overflow";
    /// Float, text: line height multiple, 0.5 to 3. Default 1.
    pub const LINE_SPACING: &str = "line_spacing";
    /// Float, text: extra space between letters in pixels, -50 to 200.
    pub const LETTER_SPACING: &str = "letter_spacing";
    /// Float, text: outline width in pixels, 0 to 64; 0 = none.
    pub const OUTLINE_WIDTH: &str = "outline_width";
    /// Text, text: outline colour. Default `#000000`.
    pub const OUTLINE_COLOR: &str = "outline_color";
    /// Text, text: shadow colour; absent = no shadow.
    pub const SHADOW_COLOR: &str = "shadow_color";
    /// Float, text: shadow offset right, -256 to 256.
    pub const SHADOW_X: &str = "shadow_x";
    /// Float, text: shadow offset down, -256 to 256.
    pub const SHADOW_Y: &str = "shadow_y";
    /// Float, text: shadow blur radius, 0 to 64.
    pub const SHADOW_BLUR: &str = "shadow_blur";
    /// Text, text: background colour of the box; absent = transparent.
    pub const BACKGROUND: &str = "background";
    /// Float, text: space inside the box edge, 0 to 1024.
    pub const PADDING: &str = "padding";
    /// Bool, text: `{time}`, `{date}`, `{scene}` ... fields in `text`. Default false.
    pub const FIELDS: &str = "fields";
    /// Text, text: the pattern a clock or timer is shown with.
    pub const FORMAT: &str = "format";
    /// Int, text: a countdown's length in milliseconds.
    pub const DURATION_MS: &str = "duration_ms";
    /// Text, text: shown when a countdown ends or a recording or stream is stopped.
    pub const END_TEXT: &str = "end_text";
}

/// What a source type captures.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceClass {
    /// Video from the capture port.
    Capture(TargetKind),
    /// Audio from the audio input port.
    Audio(EndpointKind),
    /// A picture that does not change on its own (image, colour, gradient).
    Still,
    /// A local media file (ADR-0108). Always a video pin and an audio pin.
    Media,
    /// A browser page (ADR-0109). Network-capable. Video pin only.
    Browser,
    /// A text, clock or timer (ADR-0129). Video pin only; still or not depends on
    /// its settings ([`crate::is_still`]).
    Text,
    /// A folder of pictures shown in turn (ADR-0171). Video pin only; never still.
    Slideshow,
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
        IMAGE_SOURCE | COLOR_SOURCE => SourceClass::Still,
        MEDIA_SOURCE => SourceClass::Media,
        BROWSER_SOURCE => SourceClass::Browser,
        TEXT_SOURCE => SourceClass::Text,
        SLIDESHOW_SOURCE => SourceClass::Slideshow,
        _ => return None,
    })
}

/// The categories of the Sources list filter (ADR-0195 decision 5): stable ids, in the
/// order the filter lists them. "other" holds any type without a category.
pub const SOURCE_CATEGORIES: [&str; 10] = [
    "screen",
    "window",
    "camera",
    "audio",
    "picture",
    "media",
    "browser",
    "text",
    "slideshow",
    "other",
];

/// The filter category of a source type. Interim: the type registry (ADR-0162) will
/// carry it, and this is the only place that lists the built-in types for it.
pub fn source_category(type_id: &str) -> &'static str {
    match type_id {
        DISPLAY_SOURCE => "screen",
        WINDOW_SOURCE => "window",
        CAMERA_SOURCE => "camera",
        AUDIO_INPUT_SOURCE | AUDIO_OUTPUT_SOURCE | APPLICATION_AUDIO_SOURCE => "audio",
        IMAGE_SOURCE | COLOR_SOURCE => "picture",
        MEDIA_SOURCE => "media",
        BROWSER_SOURCE => "browser",
        TEXT_SOURCE => "text",
        SLIDESHOW_SOURCE => "slideshow",
        _ => "other",
    }
}

#[cfg(test)]
mod category_tests {
    use super::*;

    #[test]
    fn built_in_types_have_a_listed_category() {
        for id in [
            DISPLAY_SOURCE,
            TEXT_SOURCE,
            COLOR_SOURCE,
            AUDIO_OUTPUT_SOURCE,
        ] {
            assert!(SOURCE_CATEGORIES.contains(&source_category(id)));
        }
        assert_eq!(source_category("com.example.unknown"), "other");
    }
}
