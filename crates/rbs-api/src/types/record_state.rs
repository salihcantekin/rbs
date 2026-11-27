//! What a recording is doing (`record.status`, topic `record.state`).

use serde::{Deserialize, Serialize};

use super::*;

/// What a recording is doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RecordState {
    /// Nothing records.
    Idle {
        /// How the last recording of this server run ended, if one ran.
        last: Option<RecordingSummary>,
    },
    /// A recording runs.
    Recording {
        /// The profile it uses, or `null` for an explicit file.
        profile: Option<String>,
        /// The video encoder's name.
        video_encoder: String,
        /// The audio encoder's name, when audio is recorded.
        audio_encoder: Option<String>,
        /// The separate files written next to the master (protocol 1.46, ADR-0086);
        /// absent when there are none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tracks: Vec<RecordTrack>,
        /// Marked sources whose file was not written (protocol 1.46); absent when
        /// none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skipped: Vec<SkippedTrack>,
        /// Paused, or a pause asked for has not begun yet (protocol 1.61,
        /// ADR-0124). `state` stays `recording` while paused.
        #[serde(default)]
        paused: bool,
        /// Time left out by pauses so far, in milliseconds, as of this state (an
        /// open pause counted up to the change) (protocol 1.61).
        #[serde(default)]
        paused_ms: u64,
        /// Time written to the file so far, in milliseconds, as of this state: the
        /// time since the start less `paused_ms` (protocol 1.61).
        #[serde(default)]
        recorded_ms: u64,
        /// The audio tracks of a recording with more than one, track 1 first
        /// (protocol 1.113, ADR-0170 decision 11); absent for one track, which
        /// `audio_encoder` names.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        audio_tracks: Vec<AudioTrack>,
    },
}

/// One audio track of a multi-track recording (protocol 1.113): the bus it records,
/// which is also the track's name in the file, and its encoder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioTrack {
    /// The bus, by name (`main` or a bus of the main canvas).
    pub bus: String,
    /// The audio encoder's name.
    pub encoder: String,
}

/// A marked source that has no file in a running recording (protocol 1.46).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SkippedTrack {
    /// The source (`source.list`).
    pub source: u32,
    /// The feed that was not written.
    pub role: TrackRole,
    /// Why.
    pub reason: SkipReason,
}

/// The feeds of a source marked to record separately (protocol 1.52).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RecordTracks {
    /// Its picture.
    #[serde(default, skip_serializing_if = "is_false")]
    pub video: bool,
    /// Its sound.
    #[serde(default, skip_serializing_if = "is_false")]
    pub audio: bool,
    /// The picture's form; `auto` when absent.
    #[serde(default, skip_serializing_if = "PictureForm::is_auto")]
    pub video_as: PictureForm,
}

/// The form of a marked picture in a source row (protocol 1.52). The request side is
/// `request::VideoAs`; this one reads a later server's value as `other`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PictureForm {
    /// Images for a still source, a video file otherwise.
    #[default]
    Auto,
    /// A video file.
    Video,
    /// PNG images.
    Image,
    /// A form this client does not know (a later minor).
    #[serde(other)]
    Other,
}

impl PictureForm {
    fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

/// Why a marked source has no file (protocol 1.46).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The program mix does not hear the source when the recording starts.
    NotHeard,
    /// More video files than this version writes beside the recording (protocol
    /// 1.52; a fixed number until the ADR-0087 estimate).
    Limit,
    /// The source has no picture when the recording starts (protocol 1.52).
    NoPicture,
    /// The recording has no picture (WAV or FLAC), so a video file has no encoder
    /// settings to match (protocol 1.52).
    AudioOnly,
    /// The file could not be opened (protocol 1.52).
    OpenFailed,
    /// A reason this client does not know (a later minor).
    #[serde(other)]
    Other,
}
