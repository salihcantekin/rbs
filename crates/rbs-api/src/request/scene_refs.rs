//! The scene requests that name a scene: `scene.remove`, `scene.switch`, `scene.rename`.

use serde::{Deserialize, Serialize};

/// `scene.audio.reset_source`: a scene and one of its sources (ADR-0091).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SceneSourceRef {
    /// The scene's id.
    pub scene: u32,
    /// The source's id.
    pub source: u32,
}

/// `scene.audio.set`: at least one of `volume` and `muted` (ADR-0091).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetSceneLevel {
    /// The scene's id.
    pub scene: u32,
    /// The source's id.
    pub source: u32,
    /// A linear factor from 0 to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    /// Muted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
}

/// One entry of a scene's own levels (ADR-0091); an absent field uses the source value.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SceneLevelInfo {
    /// The source's id.
    pub source: u32,
    /// The scene's volume for it, a linear factor from 0 to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    /// The scene's mute for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
}

/// `scene.audio.get`'s answer (protocol 1.48, ADR-0091).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SceneLevels {
    /// The scene's id.
    pub scene: u32,
    /// Own levels (`true`) or Shared levels.
    pub own: bool,
    /// Its entries by source id; empty in Shared levels.
    #[serde(default)]
    pub levels: Vec<SceneLevelInfo>,
}

/// `scene.audio.set`'s answer (protocol 1.48).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SceneLevelSet {
    /// The scene was in Shared levels and this call turned it to Own levels.
    #[serde(default)]
    pub turned_own: bool,
}

/// `scene.remove` and `scene.switch` (protocol 1.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SceneRef {
    /// The scene's id (`scene.list`).
    #[cfg_attr(feature = "schema", schemars(range(max = 4294967295_u32)))]
    pub scene: u32,
}

/// `scene.set_active` (protocol 1.112, ADR-0195).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetSceneActive {
    /// The scene's id.
    pub scene: u32,
    /// Whether next, previous and slot navigation may reach it.
    pub active: bool,
}

/// `scene.step` (protocol 1.112, ADR-0195): the next or previous active scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct StepScene {
    /// 1 for next, -1 for previous.
    pub step: i32,
}

/// `scene.position` (protocol 1.112, ADR-0195): the scene in a slot of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ScenePosition {
    /// The 1-based slot in the full scene list.
    pub n: u32,
}

/// `scene.rename` (protocol 1.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RenameScene {
    /// The scene's id.
    #[cfg_attr(feature = "schema", schemars(range(max = 4294967295_u32)))]
    pub scene: u32,
    /// The new name, unique among scenes.
    pub name: String,
}
