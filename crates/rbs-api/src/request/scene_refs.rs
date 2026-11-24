//! The scene requests that name a scene: `scene.remove`, `scene.switch`, `scene.rename`.

use serde::{Deserialize, Serialize};

/// `scene.remove` and `scene.switch` (protocol 1.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneRef {
    /// The scene's id (`scene.list`).
    pub scene: u32,
}

/// `scene.rename` (protocol 1.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameScene {
    /// The scene's id.
    pub scene: u32,
    /// The new name, unique among scenes.
    pub name: String,
}
