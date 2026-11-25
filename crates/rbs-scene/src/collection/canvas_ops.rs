//! Canvas insertion and removal, including their scene-owned state.

use super::{Op, SceneCollection, SceneError};
use crate::model::{
    Canvas, CanvasId, GroupId, GroupScroll, ItemId, Layout, Scene, SceneId, SceneScope,
};

impl SceneCollection {
    pub(super) fn perform_insert_canvas(
        &mut self,
        canvas: Canvas,
        overrides: Vec<(ItemId, Layout)>,
        group_scrolls: Vec<(SceneId, GroupId, GroupScroll)>,
        globals: Option<Scene>,
    ) -> Result<Op, SceneError> {
        let id = canvas.id;
        Self::restorable(self.next.canvas, id.0, self.canvases.contains_key(&id))?;
        self.canvases.insert(id, canvas);
        if let Some(scene) = globals {
            let sid = scene.id;
            Self::restorable(self.next.scene, sid.0, self.scenes.contains_key(&sid))?;
            for item in &scene.items {
                Self::restorable(self.next.item, item.id.0, self.item(item.id).is_some())?;
            }
            self.index_scene(&scene);
            self.scenes.insert(sid, scene);
        }
        for (item, layout) in overrides {
            let item = self.item_mut(item)?;
            item.overrides.insert(id, layout);
        }
        for (scene, group, scroll) in group_scrolls {
            let group = self
                .scenes
                .get_mut(&scene)
                .ok_or(SceneError::UnknownScene(scene))?
                .groups
                .iter_mut()
                .find(|g| g.id == group)
                .ok_or(SceneError::UnknownGroup(group))?;
            if group.scrolls.insert(id, scroll).is_some() {
                return Err(SceneError::UndoConflict);
            }
        }
        Ok(Op::RemoveCanvas(id))
    }

    pub(super) fn perform_remove_canvas(&mut self, id: CanvasId) -> Result<Op, SceneError> {
        let canvas = self
            .canvases
            .remove(&id)
            .ok_or(SceneError::UnknownCanvas(id))?;
        // Its global scene goes with it (ADR-0065): the user never sees it as
        // a scene of its own. Validation keeps it nested nowhere.
        let globals = canvas.globals.and_then(|g| self.scenes.remove(&g));
        if let Some(scene) = &globals {
            self.unindex_scene(scene);
        }
        if self
            .scenes
            .values()
            .any(|s| s.scope == SceneScope::Canvas(id))
        {
            return Err(SceneError::CanvasHasScenes(id));
        }
        let mut overrides = Vec::new();
        let mut group_scrolls = Vec::new();
        for scene in self.scenes.values_mut() {
            for item in scene.items.iter_mut() {
                if let Some(layout) = item.overrides.remove(&id) {
                    overrides.push((item.id, layout));
                }
            }
            for group in &mut scene.groups {
                if let Some(scroll) = group.scrolls.remove(&id) {
                    group_scrolls.push((scene.id, group.id, scroll));
                }
            }
        }
        Ok(Op::InsertCanvas {
            canvas,
            overrides,
            group_scrolls,
            globals,
        })
    }
}
