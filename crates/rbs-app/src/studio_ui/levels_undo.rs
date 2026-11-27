//! The undo toasts that follow the studio's history: the delete toast and the toast
//! after a scene went back to Shared levels (ADR-0091). Each one's Undo applies only
//! while its step is still the newest; another edit or an undo takes the toast away.

use rbs_api::Command as C;
use rbs_app::edit_history::{EditKind, StepInfo};
use rbs_app::studio_host::ApiReply;

use super::StudioUi;
use crate::ui::Studio;

/// The Shared levels toast: the scene it names while it shows.
#[derive(Debug, Default)]
pub struct LevelsUndo {
    /// The scene the confirm dialog put back in Shared levels, until the history
    /// shows that step.
    pending: Option<String>,
    /// The scene the toast names; empty while it does not show.
    shown: String,
    /// Bumps for each new undo toast (a delete's too), so its countdown starts again.
    pub serial: u64,
}

impl LevelsUndo {
    /// Whether `step` is the Shared levels (or Own levels) step of the scene `name`.
    fn names(step: Option<&StepInfo>, name: &str) -> bool {
        !name.is_empty() && step.is_some_and(|s| s.kind == EditKind::SceneLevels && s.name == name)
    }

    /// The newest undo step is now `newest`: the asked change shows its toast once it
    /// is in the history, and a toast whose step is no longer the newest goes.
    fn history(&mut self, newest: Option<&StepInfo>) {
        if let Some(name) = self.pending.take()
            && Self::names(newest, &name)
        {
            self.shown = name;
            self.serial += 1;
        } else if !Self::names(newest, &self.shown) {
            self.shown.clear();
        }
    }
}

impl StudioUi {
    /// The confirm dialog's "Use shared levels": the scene's own volumes and mutes go,
    /// one undo step, and a toast offers Undo.
    pub fn use_shared_levels(&mut self, scene: i32) {
        self.deleted.clear();
        self.levels_undo.shown.clear();
        self.levels_undo.pending = self.scene_name(scene);
        self.set_scene_own_levels(scene, false);
    }

    /// The name of scene `id`, as the host last listed it.
    pub fn scene_name(&self, id: i32) -> Option<String> {
        let list = self.scene_list.as_ref()?;
        let scene = list.scenes.iter().find(|s| s.id.get() as i32 == id)?;
        Some(scene.name.clone())
    }

    /// The Shared levels toast's answer: `undo` brings the scene's own levels back, if
    /// that step is still the newest; otherwise it was dismissed or timed out.
    pub fn shared_levels_toast(&mut self, undo: bool) {
        let ours = LevelsUndo::names(self.next_undo.as_ref(), &self.levels_undo.shown);
        self.levels_undo.shown.clear();
        if undo && ours {
            self.call_as(C::EditUndo, ApiReply::QuietIf(rbs_api::ErrorKind::Conflict));
        }
    }

    /// The host's history changed: what Undo and Redo apply now. The delete toast
    /// undoes only the delete it names: once another edit (also one from a control API
    /// client) or an undo changed what is newest, it goes; the Shared levels toast
    /// does the same.
    pub(super) fn history(&mut self, undo: Option<StepInfo>, redo: Option<StepInfo>) {
        let still = undo
            .as_ref()
            .is_some_and(|s| s.kind == EditKind::Delete && s.name == self.deleted);
        if !still {
            self.deleted.clear();
        }
        self.levels_undo.history(undo.as_ref());
        self.next_undo = undo;
        self.next_redo = redo;
    }

    /// The undo toasts' state for the window.
    pub(super) fn push_undo_toasts(&self, g: &Studio<'_>) {
        g.set_deleted(self.deleted.as_str().into());
        g.set_deleted_from_scene(self.deleted_from_scene);
        g.set_deleted_generation(self.levels_undo.serial as i32);
        g.set_levels_undo(self.levels_undo.shown.as_str().into());
        g.set_levels_undo_generation(self.levels_undo.serial as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(kind: EditKind, name: &str) -> StepInfo {
        StepInfo {
            kind,
            name: name.to_owned(),
        }
    }

    fn asked(name: &str) -> LevelsUndo {
        LevelsUndo {
            pending: Some(name.to_owned()),
            ..LevelsUndo::default()
        }
    }

    #[test]
    fn the_toast_shows_once_the_step_is_in_the_history() {
        let mut t = asked("Intro");
        t.history(Some(&step(EditKind::SceneLevels, "Intro")));
        assert_eq!(t.shown, "Intro");
        assert_eq!(t.serial, 1);
    }

    #[test]
    fn a_refused_change_shows_no_toast() {
        let mut t = asked("Intro");
        t.history(Some(&step(EditKind::Add, "Camera")));
        assert!(t.shown.is_empty());
        assert!(t.pending.is_none());
    }

    #[test]
    fn another_edit_takes_the_toast_away() {
        let mut t = asked("Intro");
        t.history(Some(&step(EditKind::SceneLevels, "Intro")));
        t.history(Some(&step(EditKind::Place, "Camera")));
        assert!(t.shown.is_empty());
    }

    #[test]
    fn an_undo_that_empties_the_history_takes_the_toast_away() {
        let mut t = asked("Intro");
        t.history(Some(&step(EditKind::SceneLevels, "Intro")));
        t.history(None);
        assert!(t.shown.is_empty());
    }

    #[test]
    fn another_scenes_levels_step_takes_the_toast_away() {
        let mut t = asked("Intro");
        t.history(Some(&step(EditKind::SceneLevels, "Intro")));
        t.history(Some(&step(EditKind::SceneLevels, "Outro")));
        assert!(t.shown.is_empty());
    }
}
