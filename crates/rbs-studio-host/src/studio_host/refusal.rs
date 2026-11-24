//! Why the studio host refused a command or a step: the kind the control API answers
//! with and the words the UI shows (quality plan WP-25, studio host spec "Errors").

use rbs_scene::SceneError;

use super::{CropRefusal, edit_error};

/// Why a scene, item or source edit was refused (ADR-0059): words for the UI, and the
/// control API's error kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The object does not exist (any more).
    NotFound(String),
    /// The request is not valid (an empty name, a volume out of range).
    Invalid(String),
    /// It conflicts with the state (a taken name, the last scene).
    Conflict(String),
    /// Something else went wrong.
    Failed(String),
}

impl Refusal {
    /// In words.
    pub fn text(&self) -> &str {
        match self {
            Self::NotFound(s) | Self::Invalid(s) | Self::Conflict(s) | Self::Failed(s) => s,
        }
    }

    /// In words, for the UI edge ([`Update::Error`]).
    pub fn into_text(self) -> String {
        match self {
            Self::NotFound(s) | Self::Invalid(s) | Self::Conflict(s) | Self::Failed(s) => s,
        }
    }

    pub(super) fn of(e: &SceneError) -> Self {
        match e {
            SceneError::UnknownSource(_)
            | SceneError::UnknownScene(_)
            | SceneError::UnknownItem(_)
            | SceneError::UnknownFilter(_)
            | SceneError::UnknownGroup(_) => Self::NotFound(edit_error(e)),
            SceneError::DuplicateName(_) | SceneError::StingerPlaced(_) => {
                Self::Conflict(edit_error(e))
            }
            SceneError::EmptyName
            | SceneError::InvalidVolume
            | SceneError::InvalidSyncOffset
            | SceneError::InvalidShowFor
            | SceneError::InvalidActivity
            | SceneError::InvalidTransition
            | SceneError::ChromeNotDisplay
            | SceneError::GroupTooSmall(_)
            | SceneError::GroupScope
            | SceneError::GroupNotContiguous(_)
            | SceneError::IndexOutOfRange(_) => Self::Invalid(edit_error(e)),
            _ => Self::Failed(edit_error(e)),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.text())
    }
}

impl std::error::Error for Refusal {}

impl From<CropRefusal> for Refusal {
    fn from(e: CropRefusal) -> Self {
        let text = e.text();
        match e {
            CropRefusal::NotFound(_) => Refusal::NotFound(text),
            CropRefusal::Invalid(_) => Refusal::Invalid(text),
            CropRefusal::NoPicture | CropRefusal::Conflict(_) => Refusal::Conflict(text),
            CropRefusal::Failed(_) => Refusal::Failed(text),
        }
    }
}
