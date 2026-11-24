use serde::{Deserialize, Serialize};

/// The canvas an output records or streams (protocol 1.29, ADR-0093 decision 6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OutputCanvas {
    /// The studio's main canvas.
    #[default]
    Main,
    /// The vertical canvas (`canvas.add_vertical`).
    Vertical,
}

impl OutputCanvas {
    /// Every canvas an output can use, main first.
    pub const ALL: [OutputCanvas; 2] = [OutputCanvas::Main, OutputCanvas::Vertical];

    /// Its number in the studio's set of canvases: the object id of its
    /// `record.canvas_state` and `stream.canvas_state` keys.
    pub const fn object(self) -> u32 {
        match self {
            OutputCanvas::Main => 0,
            OutputCanvas::Vertical => 1,
        }
    }
}

/// The canvases `record.start`, `record.stop`, `stream.start` and `stream.stop` act on:
/// one canvas, or every canvas the studio runs, main first (`all`, protocol 1.42,
/// ADR-0111 decision 3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OutputTarget {
    /// The studio's main canvas.
    #[default]
    Main,
    /// The vertical canvas.
    Vertical,
    /// Every canvas the studio runs, main first (protocol 1.42).
    All,
}

impl OutputTarget {
    /// The one canvas asked for; `None` for [`OutputTarget::All`].
    pub const fn single(self) -> Option<OutputCanvas> {
        match self {
            OutputTarget::Main => Some(OutputCanvas::Main),
            OutputTarget::Vertical => Some(OutputCanvas::Vertical),
            OutputTarget::All => None,
        }
    }
}

impl From<OutputCanvas> for OutputTarget {
    fn from(c: OutputCanvas) -> Self {
        match c {
            OutputCanvas::Main => OutputTarget::Main,
            OutputCanvas::Vertical => OutputTarget::Vertical,
        }
    }
}
