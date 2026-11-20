//! Field edits applied in place (audit F-09): a volume, mute, monitor, sync offset or
//! master change, the live controls a fader drag or a mute click sends many times.
//!
//! Every other edit works on a copy of the collection and validates the copy
//! (`apply_with`). A field record cannot break the document shape or place the
//! stinger, and its new value is checked before it is applied, so it needs neither the
//! copy nor the walk. It is still all or nothing.

use super::{NextIds, Op, SceneCollection, SceneError, Undo};

impl SceneCollection {
    /// A live level edit whose new value is already in range. It cannot break names,
    /// nesting or cycles, so the full document walk adds nothing.
    pub(super) fn field_edit(op: &Op) -> bool {
        match op {
            Op::SetSourceAudio { new, .. } => new.is_valid(),
            Op::SetCanvasMaster { new, .. } => new.is_valid(),
            Op::Batch(ops) => !ops.is_empty() && ops.iter().all(Self::field_edit),
            _ => false,
        }
    }

    /// Commits a field edit (`field_edit` is true for `op`) without a copy.
    pub(super) fn apply_field(&mut self, op: Op, next: NextIds) -> Result<Undo, SceneError> {
        let inverse = self.perform_field(op)?;
        self.next = next;
        self.generation = self.generation.wrapping_add(1);
        Ok(Undo(inverse))
    }

    /// One field record fails only before it writes (unknown id, `UndoConflict`). A
    /// batch that fails part-way performs the inverses it already has, newest first,
    /// so a refused edit leaves `self` as it was.
    fn perform_field(&mut self, op: Op) -> Result<Op, SceneError> {
        let Op::Batch(ops) = op else {
            return self.perform(op);
        };
        let mut inverse = Vec::with_capacity(ops.len());
        for op in ops {
            match self.perform_field(op) {
                Ok(undo) => inverse.push(undo),
                Err(error) => {
                    for undo in inverse.into_iter().rev() {
                        // Each inverse expects exactly the value its record just wrote
                        // to a slot that still exists, so it cannot be refused.
                        let restored = self.perform(undo);
                        debug_assert!(restored.is_ok(), "field rollback refused");
                    }
                    return Err(error);
                }
            }
        }
        inverse.reverse();
        Ok(Op::Batch(inverse))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use super::*;
    use crate::model::{MasterAudio, Settings, SourceAudio, SourceId};
    use rbs_core::media::Size;
    use rbs_core::timing::FrameRate;
    use rbs_graph::node::NodeTypeId;

    fn studio() -> (SceneCollection, SourceId, SourceId) {
        let mut col = SceneCollection::new();
        col.add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_60)
            .unwrap();
        let ty = || NodeTypeId::new("org.rbs.source.camera");
        let (a, _) = col.add_source("A", ty(), Settings::new()).unwrap();
        let (b, _) = col.add_source("B", ty(), Settings::new()).unwrap();
        (col, a, b)
    }

    fn set(col: &SceneCollection, source: SourceId, volume: f32) -> Op {
        let expected = col.get_source(source).unwrap().audio;
        Op::SetSourceAudio {
            source,
            expected,
            new: SourceAudio { volume, ..expected },
        }
    }

    #[test]
    fn a_batch_refused_part_way_changes_nothing() {
        let (mut col, a, b) = studio();
        let mut stale = set(&col, b, 0.3);
        if let Op::SetSourceAudio { expected, .. } = &mut stale {
            expected.muted = !expected.muted;
        }
        // The refusal is two levels deep, after two records have been written.
        let canvas = col.canvases().next().unwrap().id;
        let master = Op::SetCanvasMaster {
            canvas,
            expected: col.canvases().next().unwrap().master,
            new: MasterAudio::new(0.5, true),
        };
        let op = Op::Batch(vec![set(&col, a, 0.2), Op::Batch(vec![master, stale])]);
        assert!(SceneCollection::field_edit(&op));
        let (before, generation) = (col.clone(), col.generation());
        let next = col.next.clone();
        assert_eq!(col.apply_field(op, next), Err(SceneError::UndoConflict));
        assert_eq!(col, before);
        assert_eq!(col.generation(), generation);
    }

    #[test]
    fn a_field_batch_undoes_in_place() {
        let (mut col, a, b) = studio();
        let before = col.clone();
        let op = Op::Batch(vec![set(&col, a, 0.2), set(&col, b, 0.7)]);
        let next = col.next.clone();
        let undo = col.apply_field(op, next).unwrap();
        assert_eq!(col.get_source(b).unwrap().audio.volume, 0.7);
        let redo = col.undo(undo).unwrap();
        assert_eq!(col, before);
        col.undo(redo).unwrap();
        assert_eq!(col.get_source(a).unwrap().audio.volume, 0.2);
    }
}
