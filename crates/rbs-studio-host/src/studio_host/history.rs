//! Undo and redo of scene edits and of recorded text boxes.

use super::*;

impl Host {
    /// Undoes (`back`) or redoes one step of the history and runs the result. A
    /// refused step is said in [`Update::Error`]; with nothing to do, nothing happens.
    pub(super) fn step(&mut self, back: bool, notify: &dyn Fn(Update)) {
        // Nothing to undo is not a problem: the buttons are disabled then, and a key
        // press says nothing.
        match self.undo_or_redo(back) {
            Ok(()) => self.apply(notify),
            Err((true, _)) => {}
            Err((false, why)) => {
                notify(Update::Error(why));
                // A refused step left the scene as it was, but a lost one may not
                // have: run what is there.
                self.apply(notify);
            }
        }
    }

    /// One step of the history; the error says whether there was nothing to do
    /// (`true`) and why it failed.
    pub(super) fn undo_or_redo(&mut self, back: bool) -> Result<(), (bool, String)> {
        let program = self.scene;
        let preview = self.preview();
        let offsets = filters::audio_offsets(&self.collection);
        let result = self.undo_or_redo_records(back);
        self.resync_audio_offsets(&offsets);
        // Undo and redo never change what is on air (ADR-0059) or the preview scene
        // (ADR-0061): a record that put a scene back on air or in preview is
        // overruled, and a removed program is replaced.
        let now = self.collection.canvas(self.canvas).and_then(|c| c.program);
        if now != Some(program) && self.collection.scene(program).is_some() {
            let _ = self.collection.set_program(self.canvas, Some(program));
        }
        self.keep_program();
        let kept = preview.filter(|p| self.collection.scene(*p).is_some());
        let restored = match (preview, kept) {
            (None, _) => None,
            (Some(_), Some(p)) => Some(p),
            // The preview scene went away: studio mode stays, with the program.
            (Some(_), None) => Some(self.scene),
        };
        if self.preview() != restored {
            let _ = self.collection.set_preview(self.canvas, restored);
        }
        result
    }

    /// The box of every text source a running take writes a separate picture track
    /// of; empty while no take records one.
    pub(super) fn recorded_text_boxes(
        &self,
    ) -> Vec<(SourceId, Option<SettingValue>, Option<SettingValue>)> {
        self.recorded_text_boxes_in(&self.collection)
    }

    fn recorded_text_boxes_in(
        &self,
        collection: &SceneCollection,
    ) -> Vec<(SourceId, Option<SettingValue>, Option<SettingValue>)> {
        if !self.studio.iso_video_running() {
            return Vec::new();
        }
        collection
            .sources()
            .filter(|s| {
                rbs_sources::types::class_of(&s.type_id)
                    == Some(rbs_sources::types::SourceClass::Text)
                    && self.studio.iso_video_records(s.id)
            })
            .map(|s| {
                let get = |k: &str| s.settings.get(k).cloned();
                (
                    s.id,
                    get(rbs_sources::types::keys::WIDTH),
                    get(rbs_sources::types::keys::HEIGHT),
                )
            })
            .collect()
    }

    pub(super) fn undo_or_redo_records(&mut self, back: bool) -> Result<(), (bool, String)> {
        let mut collection = self.collection.clone();
        let mut history = self.edits.history.fork();
        let apply = |r: Undo| collection.undo(r);
        let done = if back {
            history.undo(apply)
        } else {
            history.redo(apply)
        };
        match done {
            Ok(_) => {
                // Undoing an activation, or redoing a deactivation, may not take a
                // scene off the air (ADR-0195 decision 3).
                let shown = self.shown_scenes();
                if let Some(id) = shown
                    .iter()
                    .find(|id| collection.scene(**id).is_some_and(|s| !s.active))
                {
                    let name = collection.scene(*id).map_or("", |s| s.name.as_str());
                    return Err((
                        false,
                        format!("The scene {name:?} is on air, so it cannot be made inactive."),
                    ));
                }
                if self.recorded_text_boxes_in(&collection) != self.recorded_text_boxes() {
                    return Err((false, STOP_TO_CHANGE_BOX.to_owned()));
                }
                // A bus in use is never removed, and output settings follow a rename
                // (ADR-0170 decision 9).
                let renames = self
                    .bus_step_check(&collection)
                    .map_err(|why| (false, why))?;
                self.collection = collection;
                self.edits.history = history;
                self.bus_step_done(&renames);
                Ok(())
            }
            Err(HistoryError::Empty) => Err((
                true,
                if back {
                    "nothing to undo"
                } else {
                    "nothing to redo"
                }
                .to_owned(),
            )),
            Err(HistoryError::Refused(e)) => Err((
                false,
                format!("The scene changed in a way this step cannot be applied to ({e:?})."),
            )),
            Err(HistoryError::Lost(e)) => {
                self.edits.history = history;
                Err((
                    false,
                    format!(
                        "The scene changed in a way this step cannot be applied to ({e:?}), and part of it may have been applied; the undo history was cleared."
                    ),
                ))
            }
        }
    }

    /// The scene that holds an item.
    pub(super) fn scene_of(&self, item: ItemId) -> Option<SceneId> {
        self.collection.item(item).map(|(s, _)| s)
    }
}

/// The undo history and the steps the window shows.
pub(super) struct EditHistory {
    /// The user's undo history of scene edits, shared with control API clients.
    pub(super) history: History<Undo>,
    /// The next undo and redo as last sent.
    pub(super) history_shown: (Option<StepInfo>, Option<StepInfo>),
}

impl EditHistory {
    pub(super) fn new() -> Self {
        Self {
            history: History::new(),
            history_shown: (None, None),
        }
    }
}
