//! The `media.*` methods' parameters and answer (protocol 1.83, ADR-0108 decision 10;
//! slide shows in 1.102, ADR-0171 decision 9).

use serde::{Deserialize, Serialize};

/// `media.play`, `media.pause`, `media.restart` and `media.state` (protocol 1.83,
/// ADR-0108 decision 10): one media-file source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaSource {
    /// The source's id.
    pub source: u32,
}

/// `media.seek` (protocol 1.83, ADR-0108 decision 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaSeek {
    /// The source's id.
    pub source: u32,
    /// The media time to move to, in milliseconds; past the file's stated duration it
    /// is clamped to it.
    pub position_ms: u64,
}

/// The play state of a media-file source (ADR-0108 decision 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaPlayState {
    /// Picture and sound advance.
    Playing,
    /// Stopped at the cursor.
    Paused,
    /// Only a preview scene draws it: it plays when a program or hold scene does.
    Cued,
    /// The end of the file without `loop`: the last picture holds.
    Ended,
    /// Asleep with no stored pause or end.
    Standby,
    /// A fault stopped playback.
    Unavailable,
    /// A state this client does not know (a newer server).
    #[serde(other)]
    Unknown,
}

/// The answer of every `media.*` method (protocol 1.83).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaStateInfo {
    /// The source's id.
    pub source: u32,
    /// The play state after the command.
    pub state: MediaPlayState,
    /// The media cursor in milliseconds: where a pause stopped, else the picture
    /// shown last.
    pub cursor_ms: u64,
    /// The file's stated duration in milliseconds, once it was opened. For a slide
    /// show, the interval; absent with `advance = manual`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// A slide show's shown picture, 0-based (protocol 1.102, ADR-0171 decision 9);
    /// absent for a media file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    /// A slide show's picture count (protocol 1.102); absent for a media file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}
