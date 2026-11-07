//! The user's undo history of scene edits (U4, control API "Undo"): one stack shared
//! by the app's own panels and every control API client.
//!
//! A step is one user action: adding a source, deleting one, moving it a layer up or
//! down, or placing its box. All the boxes of one drag on the picture panel are one step
//! (they carry the same gesture number), so Undo puts the source back where the drag
//! started. A step holds the scene collection's undo records in the order the edits
//! were made; undoing it applies them in reverse, and the records that come back form
//! the step that redoes it. A new edit clears the redo side. At most [`MAX_STEPS`] steps
//! are kept; the oldest go first.
//!
//! Generic over the record and the way it is applied, so the logic is tested without a
//! scene collection; the studio host uses `rbs_scene::Undo` and
//! `SceneCollection::undo`.

use std::collections::VecDeque;

/// Steps kept on the undo side (the same depth as the layout history, U21).
pub const MAX_STEPS: usize = 100;

/// What a step did, for "Undo moving Camera" and the like in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// A source was added.
    Add,
    /// A source was deleted (glossary: Delete source).
    Delete,
    /// A source moved a layer up or down.
    Order,
    /// A source's box was moved or resized.
    Place,
    /// A source's shape or border changed (ADR-0053).
    Style,
    /// A source's picture was cropped (ADR-0058).
    Crop,
    /// A source was shown or hidden in the scene.
    Visibility,
    /// A source was locked or unlocked (ADR-0059).
    Lock,
    /// A source was renamed (the name is the old one).
    RenameSource,
    /// A scene was added (the name is the scene's).
    AddScene,
    /// A scene was removed.
    RemoveScene,
    /// A scene was renamed (the name is the old one).
    RenameScene,
}

/// The step the next undo or redo applies: its kind and the name of the source or
/// scene it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepInfo {
    /// What it did.
    pub kind: EditKind,
    /// The source it did it to, as named then.
    pub name: String,
}

#[derive(Debug)]
struct Step<R> {
    info: StepInfo,
    /// Edits of one gesture join one step; 0 never joins.
    gesture: u64,
    /// In the order they apply.
    records: Vec<R>,
}

/// Why an undo or redo did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryError<E> {
    /// There is nothing to undo (or redo).
    Empty,
    /// A record was refused. The state is as it was before; the step stays.
    Refused(E),
    /// A record was refused and putting back what had been applied failed too: the
    /// state is not known, so the history was cleared.
    Lost(E),
}

/// Undo and redo stacks of steps made of records `R`.
#[derive(Debug)]
pub struct History<R> {
    undo: VecDeque<Step<R>>,
    redo: Vec<Step<R>>,
}

impl<R> Default for History<R> {
    fn default() -> Self {
        Self {
            undo: VecDeque::new(),
            redo: Vec::new(),
        }
    }
}

impl<R: Clone> History<R> {
    /// An empty history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an edit made of `records` (in the order they were applied). With a
    /// `gesture` other than 0 that equals the newest step's, the records join that
    /// step. Clears the redo side. Nothing is recorded for no records.
    pub fn record(&mut self, kind: EditKind, name: &str, gesture: u64, records: Vec<R>) {
        if records.is_empty() {
            return;
        }
        self.redo.clear();
        if gesture != 0
            && let Some(top) = self.undo.back_mut()
            && top.gesture == gesture
            && top.info.kind == kind
        {
            top.records.extend(records);
            return;
        }
        if self.undo.len() == MAX_STEPS {
            self.undo.pop_front();
        }
        self.undo.push_back(Step {
            info: StepInfo {
                kind,
                name: name.to_owned(),
            },
            gesture,
            records,
        });
    }

    /// Undoes the newest step with `apply` (which applies one record and returns the
    /// one that reverses it) and keeps it for redo. Returns what was undone.
    pub fn undo<E>(
        &mut self,
        apply: impl FnMut(R) -> Result<R, E>,
    ) -> Result<StepInfo, HistoryError<E>> {
        let step = self.undo.pop_back().ok_or(HistoryError::Empty)?;
        match revert(step, apply) {
            Ok(back) => {
                let info = back.info.clone();
                self.redo.push(back);
                Ok(info)
            }
            Err((Some(step), e)) => {
                self.undo.push_back(step);
                Err(HistoryError::Refused(e))
            }
            Err((None, e)) => {
                self.clear();
                Err(HistoryError::Lost(e))
            }
        }
    }

    /// Redoes the step undone last. Returns what was redone.
    pub fn redo<E>(
        &mut self,
        apply: impl FnMut(R) -> Result<R, E>,
    ) -> Result<StepInfo, HistoryError<E>> {
        let step = self.redo.pop().ok_or(HistoryError::Empty)?;
        match revert(step, apply) {
            Ok(back) => {
                let info = back.info.clone();
                // A redone step never joins a later gesture's edits.
                self.undo.push_back(Step { gesture: 0, ..back });
                Ok(info)
            }
            Err((Some(step), e)) => {
                self.redo.push(step);
                Err(HistoryError::Refused(e))
            }
            Err((None, e)) => {
                self.clear();
                Err(HistoryError::Lost(e))
            }
        }
    }

    /// What the next undo would undo.
    pub fn next_undo(&self) -> Option<&StepInfo> {
        self.undo.back().map(|s| &s.info)
    }

    /// What the next redo would redo.
    pub fn next_redo(&self) -> Option<&StepInfo> {
        self.redo.last().map(|s| &s.info)
    }

    /// Forgets every step (the records no longer match the state).
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

/// Applies a step's records in reverse and returns the step that reverses it. When a
/// record is refused, what was applied is reversed again and the error comes with the
/// step as it was, or with `None` when putting it back failed too.
#[allow(clippy::type_complexity)]
fn revert<R: Clone, E>(
    step: Step<R>,
    mut apply: impl FnMut(R) -> Result<R, E>,
) -> Result<Step<R>, (Option<Step<R>>, E)> {
    let mut back: Vec<R> = Vec::with_capacity(step.records.len());
    for r in step.records.iter().rev() {
        match apply(r.clone()) {
            Ok(b) => back.push(b),
            Err(e) => {
                // Newest first: `back` was filled in the order the records applied.
                while let Some(b) = back.pop() {
                    if apply(b).is_err() {
                        return Err((None, e));
                    }
                }
                return Err((Some(step), e));
            }
        }
    }
    // `back` is in the order its records were made, like any step's records.
    Ok(Step {
        records: back,
        ..step
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    /// A record of a fake document of numbered cells: cell `at` must hold `expect`
    /// and gets `set`, like the scene collection's records that carry the state they
    /// expect.
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Rec {
        at: usize,
        expect: i32,
        set: i32,
    }

    #[derive(Debug, PartialEq)]
    struct Conflict;

    struct Doc(Vec<i32>);

    impl Doc {
        /// Edits a cell and returns the record that undoes it.
        fn edit(&mut self, at: usize, to: i32) -> Rec {
            let was = self.0[at];
            self.0[at] = to;
            Rec {
                at,
                expect: to,
                set: was,
            }
        }

        fn apply(&mut self, r: Rec) -> Result<Rec, Conflict> {
            if self.0[r.at] != r.expect {
                return Err(Conflict);
            }
            self.0[r.at] = r.set;
            Ok(Rec {
                at: r.at,
                expect: r.set,
                set: r.expect,
            })
        }
    }

    #[test]
    fn steps_undo_and_redo_in_order() {
        let mut doc = Doc(vec![0, 0]);
        let mut h = History::new();
        let r = doc.edit(0, 1);
        h.record(EditKind::Add, "A", 0, vec![r]);
        let r = doc.edit(1, 2);
        h.record(EditKind::Order, "B", 0, vec![r]);
        assert_eq!(h.next_undo().unwrap().name, "B");

        let info = h.undo(|r| doc.apply(r)).unwrap();
        assert_eq!(info.kind, EditKind::Order);
        assert_eq!(doc.0, [1, 0]);
        assert_eq!(h.next_redo().unwrap().name, "B");
        h.undo(|r| doc.apply(r)).unwrap();
        assert_eq!(doc.0, [0, 0]);
        assert_eq!(h.undo(|r| doc.apply(r)), Err(HistoryError::Empty));

        h.redo(|r| doc.apply(r)).unwrap();
        h.redo(|r| doc.apply(r)).unwrap();
        assert_eq!(doc.0, [1, 2]);
        assert_eq!(h.redo(|r| doc.apply(r)), Err(HistoryError::Empty));
        h.undo(|r| doc.apply(r)).unwrap();
        assert_eq!(doc.0, [1, 0]);
    }

    #[test]
    fn a_gesture_is_one_step_and_a_new_edit_clears_redo() {
        let mut doc = Doc(vec![0, 0]);
        let mut h = History::new();
        for v in 1..=5 {
            let r = doc.edit(0, v);
            h.record(EditKind::Place, "A", 7, vec![r]);
        }
        // Another gesture is another step, even for the same source.
        let r = doc.edit(0, 9);
        h.record(EditKind::Place, "A", 8, vec![r]);
        // Gesture 0 never joins.
        let r = doc.edit(1, 1);
        h.record(EditKind::Place, "B", 0, vec![r]);
        let r = doc.edit(1, 2);
        h.record(EditKind::Place, "B", 0, vec![r]);

        h.undo(|r| doc.apply(r)).unwrap();
        assert_eq!(doc.0, [9, 1]);
        h.undo(|r| doc.apply(r)).unwrap();
        h.undo(|r| doc.apply(r)).unwrap();
        assert_eq!(doc.0, [5, 0]);
        h.undo(|r| doc.apply(r)).unwrap();
        assert_eq!(doc.0, [0, 0], "the drag goes back to its start at once");

        // Redo, then a new edit: nothing left to redo.
        h.redo(|r| doc.apply(r)).unwrap();
        assert_eq!(doc.0, [5, 0]);
        let r = doc.edit(1, 3);
        h.record(EditKind::Place, "B", 0, vec![r]);
        assert!(h.next_redo().is_none());
        // A redone step does not join a later edit with its old gesture.
        h.undo(|r| doc.apply(r)).unwrap();
        h.redo(|r| doc.apply(r)).unwrap();
        let r = doc.edit(0, 6);
        h.record(EditKind::Place, "A", 7, vec![r]);
        h.undo(|r| doc.apply(r)).unwrap();
        assert_eq!(doc.0, [5, 3]);
    }

    #[test]
    fn a_refused_record_leaves_the_state_and_the_step() {
        let mut doc = Doc(vec![0, 0]);
        let mut h = History::new();
        let a = doc.edit(0, 1);
        let b = doc.edit(1, 1);
        h.record(EditKind::Delete, "A", 0, vec![a, b]);
        // Someone else changed cell 0: the step's second record is refused after the
        // first one applied, and that one is put back.
        doc.0[0] = 5;
        assert_eq!(
            h.undo(|r| doc.apply(r)),
            Err(HistoryError::Refused(Conflict))
        );
        assert_eq!(doc.0, [5, 1]);
        assert!(h.next_undo().is_some());
        doc.0[0] = 1;
        h.undo(|r| doc.apply(r)).unwrap();
        assert_eq!(doc.0, [0, 0]);
    }

    #[test]
    fn a_failed_roll_back_clears_the_history() {
        let mut doc = Doc(vec![0, 0]);
        let mut h = History::new();
        let a = doc.edit(0, 1);
        let b = doc.edit(1, 1);
        h.record(EditKind::Delete, "A", 0, vec![a, b]);
        doc.0[0] = 5;
        let mut calls = 0;
        let got = h.undo(|r| {
            calls += 1;
            // The roll back (third call) is refused too.
            if calls == 3 {
                return Err(Conflict);
            }
            doc.apply(r)
        });
        assert_eq!(got, Err(HistoryError::Lost(Conflict)));
        assert!(h.next_undo().is_none() && h.next_redo().is_none());
    }

    #[test]
    fn the_oldest_steps_go_first() {
        let mut doc = Doc(vec![0]);
        let mut h = History::new();
        for v in 1..=(MAX_STEPS as i32 + 10) {
            let r = doc.edit(0, v);
            h.record(EditKind::Place, "A", 0, vec![r]);
        }
        let mut undone = 0;
        while h.undo(|r| doc.apply(r)).is_ok() {
            undone += 1;
        }
        assert_eq!(undone, MAX_STEPS);
        assert_eq!(doc.0, [10]);
    }

    #[test]
    fn nothing_is_recorded_without_records() {
        let mut h: History<Rec> = History::new();
        h.record(EditKind::Add, "A", 0, Vec::new());
        assert!(h.next_undo().is_none());
    }
}
