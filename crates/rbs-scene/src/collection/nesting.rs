//! Preflight for the retained, unsupported scene-item variant (ADR-0188).

use super::{Op, SceneCollection, SceneError};
use crate::model::{ItemContent, Scene, SceneItem};

impl SceneCollection {
    /// Rejects unsupported scene placements before serializing a collection.
    pub fn validate_no_nested_scenes(&self) -> Result<(), SceneError> {
        for scene in self.scenes.values() {
            check_items(&scene.items)?;
        }
        Ok(())
    }
}

pub(super) fn check_items(items: &[SceneItem]) -> Result<(), SceneError> {
    if items
        .iter()
        .any(|item| matches!(item.content, ItemContent::Scene(_)))
    {
        Err(SceneError::NestingUnsupported)
    } else {
        Ok(())
    }
}

pub(super) fn check_scene_items(scenes: &[Scene]) -> Result<(), SceneError> {
    for scene in scenes {
        check_items(&scene.items)?;
    }
    Ok(())
}

pub(super) fn check_op(op: &Op) -> Result<(), SceneError> {
    match op {
        Op::InsertCanvas {
            globals: Some(scene),
            ..
        } => check_items(&scene.items),
        Op::InsertScene {
            scene, placements, ..
        } => {
            check_items(&scene.items)?;
            for (_, _, item) in placements {
                check_items(std::slice::from_ref(item))?;
            }
            Ok(())
        }
        Op::InsertSource { placements, .. } => {
            for (_, _, item) in placements {
                check_items(std::slice::from_ref(item))?;
            }
            Ok(())
        }
        Op::InsertItem { item, .. } | Op::ReplaceItem { new: item, .. } => {
            check_items(std::slice::from_ref(item))
        }
        Op::ReplaceItems(items) => {
            for (_, new) in items {
                check_items(std::slice::from_ref(new))?;
            }
            Ok(())
        }
        Op::SetSceneItems { new, .. } => check_items(new),
        Op::Batch(ops) => {
            for op in ops {
                check_op(op)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
