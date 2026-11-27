//! Built-in source classes exposed by the control API.

use serde::{Deserialize, Serialize};

/// A kind of source the user can add.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A whole display.
    Screen,
    /// One window.
    Window,
    /// A camera or capture card.
    Camera,
    /// A microphone or line input.
    AudioInput,
    /// What the computer plays.
    SystemAudio,
    /// What one application plays.
    ApplicationAudio,
    /// A picture: a file or a built-in sample (protocol 1.34, ADR-0075).
    Image,
    /// A solid colour (protocol 1.34, ADR-0075).
    Color,
    /// A local media file (protocol TBD, ADR-0108).
    Media,
    /// A browser page from the helper process (protocol TBD, ADR-0109).
    Browser,
    /// A text drawn by the server's text renderer (protocol 1.75, ADR-0129).
    Text,
    /// The pictures of one folder shown one after another (protocol 1.102, ADR-0171).
    Slideshow,
}
