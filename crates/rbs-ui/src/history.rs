//! Layout undo history, separate from the engine command log (UI rule U21).
//!
//! It stores whole workspace snapshots: a workspace is a few hundred bytes and
//! changes only on user action, so snapshots are simpler than inverse
//! operations and cannot drift from them.

use std::collections::VecDeque;

use crate::workspace::Workspace;

/// Default number of undo steps kept.
pub const DEFAULT_LIMIT: usize = 100;

/// Undo and redo stacks of workspace states.
#[derive(Clone, Debug)]
pub struct LayoutHistory {
    undo: VecDeque<Entry>,
    redo: Vec<Workspace>,
    limit: usize,
}

#[derive(Clone, Debug)]
struct Entry {
    before: Workspace,
    group: Option<u64>,
}

impl Default for LayoutHistory {
    fn default() -> Self {
        LayoutHistory::new(DEFAULT_LIMIT)
    }
}

impl LayoutHistory {
    /// An empty history keeping at most `limit` undo steps (at least one).
    pub fn new(limit: usize) -> Self {
        LayoutHistory {
            undo: VecDeque::new(),
            redo: Vec::new(),
            limit: limit.max(1),
        }
    }

    /// Record the state `before` a change. A change with the same `group` as the
    /// previous one joins its step (for example the many moves of one splitter
    /// drag); call [`LayoutHistory::seal`] to end a group. Clears redo.
    pub fn record(&mut self, before: &Workspace, group: Option<u64>) {
        self.redo.clear();
        if group.is_some() && self.undo.back().is_some_and(|e| e.group == group) {
            return;
        }
        if self.undo.len() == self.limit {
            self.undo.pop_front();
        }
        self.undo.push_back(Entry {
            before: before.clone(),
            group,
        });
    }

    /// End the current group, so the next change starts a new step.
    pub fn seal(&mut self) {
        if let Some(e) = self.undo.back_mut() {
            e.group = None;
        }
    }

    /// Undo one step: `current` becomes the state before it. Returns false when
    /// there is nothing to undo.
    pub fn undo(&mut self, current: &mut Workspace) -> bool {
        let Some(e) = self.undo.pop_back() else {
            return false;
        };
        self.redo.push(std::mem::replace(current, e.before));
        true
    }

    /// Redo one undone step. Returns false when there is nothing to redo.
    pub fn redo(&mut self, current: &mut Workspace) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        let before = std::mem::replace(current, next);
        if self.undo.len() == self.limit {
            self.undo.pop_front();
        }
        self.undo.push_back(Entry {
            before,
            group: None,
        });
        true
    }

    /// Is there a step to undo?
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Is there a step to redo?
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::builtin;
    use crate::panel::id;

    #[test]
    fn undo_redo_and_groups() {
        let mut h = LayoutHistory::new(2);
        let mut w = builtin::edit();
        let original = w.clone();

        h.record(&w, None);
        w.close(&id("rbs.stats")).unwrap();
        let closed = w.clone();

        // Three changes in one group are one step.
        for _ in 0..3 {
            h.record(&w, Some(7));
            w.locked = !w.locked;
        }
        h.seal();
        assert!(h.undo(&mut w));
        assert_eq!(w, closed);
        assert!(h.undo(&mut w));
        assert_eq!(w, original);
        assert!(!h.undo(&mut w));
        assert!(h.redo(&mut w));
        assert_eq!(w, closed);
        assert!(h.can_redo());
        // A new change drops the redo stack.
        h.record(&w, None);
        assert!(!h.can_redo());
    }

    #[test]
    fn limit_drops_oldest() {
        let mut h = LayoutHistory::new(2);
        let mut w = builtin::edit();
        for _ in 0..5 {
            h.record(&w, None);
            w.locked = !w.locked;
        }
        assert!(h.undo(&mut w));
        assert!(h.undo(&mut w));
        assert!(!h.undo(&mut w));
    }
}
