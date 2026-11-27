//! `source.add`'s parameters: a target, a text (protocol 1.75), or a shape, a file or
//! an address (protocol 1.100, ADR-0164 decision 6).

use serde::{Deserialize, Serialize};

use super::Stamp;
use crate::types::SourceKind;

/// `source.add`: a source of `kind` capturing one target, on top of the program scene.
/// A `text` source (protocol 1.75, ADR-0129 decision 8) has no target: `adapter` and
/// `target` may be left out and are ignored, and `text` is what it shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AddSource {
    /// Its kind.
    pub kind: SourceKind,
    /// The adapter that listed the target (empty for `text`).
    #[serde(default)]
    pub adapter: String,
    /// The target's id for that adapter (empty for `text`).
    #[serde(default)]
    pub target: String,
    /// Its name; `null` lets the server pick one.
    #[serde(default)]
    pub name: Option<String>,
    /// The scene it is placed in (protocol 1.9); absent: the program scene.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<u32>,
    /// Add it as a global source of the studio's canvas instead (protocol 1.10,
    /// ADR-0065): shown and heard in every scene; `scene` is then not allowed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub global: bool,
    /// Also add the sound that belongs to the picture (protocol 1.26, ADR-0096): a
    /// display's system sound without RBS's own sound, a window's application sound.
    /// Default false; the picture and its sound are one undo step.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub with_audio: bool,
    /// The text a `text` source shows (protocol 1.75); absent: "Text". Not allowed for
    /// other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The shape of a `color` source, centred (protocol 1.100). Not allowed for other
    /// kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stamp: Option<Stamp>,
    /// The file of an `image` or `media` source instead of a target (protocol 1.100):
    /// a local absolute path; any other path needs the irreversible-actions permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The address of a `browser` page (protocol 1.100); `""` adds a page that waits
    /// for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}
