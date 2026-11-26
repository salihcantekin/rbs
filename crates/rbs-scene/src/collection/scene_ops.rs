//! Scene copies and a scene's own levels (ADR-0065, ADR-0091), kept apart from the
//! main edit code so that file stays within its size budget.

use super::groups::copy_groups;
use super::{Op, SceneCollection, SceneError, Undo, issue};
use crate::model::{ItemId, Scene, SceneAudio, SceneId, SceneLevel, SourceId};

impl SceneCollection {
    /// Copies a scene: new scene id and item ids, the same sources,
    /// same layouts. Does not put it on air. Refused for a canvas's global scene
    /// (ADR-0065).
    pub fn duplicate_scene(
        &mut self,
        id: SceneId,
        name: &str,
    ) -> Result<(SceneId, Undo), SceneError> {
        if self.canvases.values().any(|c| c.globals == Some(id)) {
            return Err(SceneError::GlobalScene(id));
        }
        let source = self.get_scene(id)?.clone();
        super::check_items(&source.items)?;
        let mut next = self.next.clone();
        let new_id = SceneId(issue(&mut next.scene)?);
        let mut items = Vec::with_capacity(source.items.len());
        for item in &source.items {
            let mut copy = item.clone();
            copy.id = ItemId(issue(&mut next.item)?);
            items.push(copy);
        }
        let groups = copy_groups(&mut next, &source.groups, &mut items)?;
        let scene = Scene {
            id: new_id,
            name: name.trim().to_owned(),
            scope: source.scope,
            items,
            audio: source.audio.clone(),
            transition_in: source.transition_in,
            groups,
            active: source.active,
        };
        let undo = self.apply_with(
            Op::InsertScene {
                scene,
                placements: Vec::new(),
                shown: Vec::new(),
                globals: Vec::new(),
            },
            next,
        )?;
        Ok((new_id, undo))
    }

    /// Turns a scene's Own levels on (ADR-0091), with these entries: the caller copies
    /// the effective levels of the scene's sources so nothing jumps
    /// ([`SceneCollection::heard_levels`]). Refused for a global scene. One undo step;
    /// the scene in Own levels already changes nothing but its entries.
    pub fn set_scene_own_levels(
        &mut self,
        id: SceneId,
        audio: SceneAudio,
    ) -> Result<Undo, SceneError> {
        if self.is_global_scene(id) {
            return Err(SceneError::GlobalScene(id));
        }
        let expected_own = self.get_scene(id)?.audio.is_some();
        self.apply(Op::SetSceneAudio {
            scene: id,
            expected_own,
            new: Some(audio),
        })
    }

    /// Puts a scene back in Shared levels, discarding its entries (ADR-0091). One undo
    /// step, whose undo restores the entries.
    pub fn set_scene_shared_levels(&mut self, id: SceneId) -> Result<Undo, SceneError> {
        let expected_own = self.get_scene(id)?.audio.is_some();
        self.apply(Op::SetSceneAudio {
            scene: id,
            expected_own,
            new: None,
        })
    }

    /// Sets, changes or (with `None`) removes a scene's entry for a source (ADR-0091).
    /// `SharedLevels` while the scene is in Shared levels; `InvalidVolume` for a volume
    /// outside `0.0..=1.0`. A live edit: the record covers only this entry.
    pub fn set_scene_level(
        &mut self,
        scene: SceneId,
        source: SourceId,
        level: Option<SceneLevel>,
    ) -> Result<Undo, SceneError> {
        self.get_source(source)?;
        let audio = self
            .get_scene(scene)?
            .audio
            .as_ref()
            .ok_or(SceneError::SharedLevels(scene))?;
        if level.is_some_and(|l| !l.is_valid()) {
            return Err(SceneError::InvalidVolume);
        }
        let expected = audio.level(source).copied();
        self.apply(Op::SetSceneLevel {
            scene,
            source,
            expected,
            new: level,
        })
    }

    /// Performs `ops` in order and returns the inverse that undoes them in reverse.
    pub(super) fn perform_batch(&mut self, ops: Vec<Op>) -> Result<Op, SceneError> {
        let mut inverse = Vec::with_capacity(ops.len());
        for op in ops {
            inverse.push(self.perform(op)?);
        }
        inverse.reverse();
        Ok(Op::Batch(inverse))
    }
}
