//! The answers that name what a call created: `source.add`, `scene.add`, `item.add`.

use serde::{Deserialize, Serialize};

/// `source.add`'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceAdded {
    /// The new source's id.
    pub source: u32,
}

/// `scene.add`'s answer (protocol 1.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneAdded {
    /// The new scene's id.
    pub scene: u32,
}

/// `item.add`'s answer (protocol 1.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemAdded {
    /// The new item's id.
    pub item: u32,
}
