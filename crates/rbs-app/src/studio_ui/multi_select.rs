//! Selecting several sources in the Sources list: Ctrl/Cmd+click toggles a row and
//! Shift+click selects the range from the last plain-selected row. The set only
//! exists while it holds two or more sources; with one or none, the single selection
//! (`StudioUi::selected`) is the whole story. Runs on a click and when the rows are
//! made again, never per frame.

use std::collections::BTreeSet;

use rbs_ui::transform::{DragKeys, Mark};
use slint::Model as _;

use super::{OutputCanvas, SourceData, sound_nest};
use crate::preview_edit::{Effect, Placed};

/// How a row was clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Click {
    /// No modifier: that row alone.
    Plain,
    /// Ctrl (Cmd on macOS): add or remove the row.
    Toggle,
    /// Shift: every row from the primary row to this one.
    Range,
}

impl Click {
    /// The click for the list's `(ctrl, shift)` modifier flags.
    pub(super) fn from_keys(ctrl: bool, shift: bool) -> Click {
        if ctrl {
            Click::Toggle
        } else if shift {
            Click::Range
        } else {
            Click::Plain
        }
    }
}

/// The selected sources when there are several (the primary one included).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Picked(BTreeSet<i32>);

impl Picked {
    /// Whether `id` is one of several selected sources.
    pub(super) fn has(&self, id: i32) -> bool {
        self.0.contains(&id)
    }

    /// The selected ids: the set, else the single `selected` source (empty for -1).
    pub(super) fn ids(&self, selected: i32) -> Vec<i32> {
        if self.0.len() > 1 {
            self.0.iter().copied().collect()
        } else if selected >= 0 {
            vec![selected]
        } else {
            Vec::new()
        }
    }

    /// Applies a click on `id`. `order` is the rows' ids as listed; `selected` is the
    /// primary selection and is updated.
    pub(super) fn click(&mut self, order: &[i32], selected: &mut i32, id: i32, how: Click) {
        match how {
            Click::Plain => {
                self.0.clear();
                *selected = id;
            }
            Click::Toggle => {
                let mut set: BTreeSet<i32> = self.ids(*selected).into_iter().collect();
                if !set.remove(&id) {
                    set.insert(id);
                    *selected = id;
                } else if *selected == id {
                    *selected = set.iter().next_back().copied().unwrap_or(-1);
                }
                self.0 = set;
                self.settle();
            }
            Click::Range => {
                let (a, b) = (
                    order.iter().position(|x| *x == *selected),
                    order.iter().position(|x| *x == id),
                );
                match (a, b) {
                    (Some(a), Some(b)) => {
                        let (lo, hi) = (a.min(b), a.max(b));
                        self.0 = order[lo..=hi].iter().copied().collect();
                        self.settle();
                    }
                    _ => {
                        self.0.clear();
                        *selected = id;
                    }
                }
            }
        }
    }

    /// A set of fewer than two is not a multi-selection.
    fn settle(&mut self) {
        if self.0.len() < 2 {
            self.0.clear();
        }
    }

    /// Drops sources that are no longer listed.
    pub(super) fn retain(&mut self, order: &[i32]) {
        self.0.retain(|id| order.contains(id));
        self.settle();
    }

    /// Forgets the set (a plain selection from elsewhere).
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }
}

impl super::StudioUi {
    /// A Sources row was clicked with Ctrl/Cmd or Shift held.
    pub fn select_with(&mut self, id: i32, ctrl: bool, shift: bool) {
        let order: Vec<i32> = self.rows.iter().map(|r| r.id.get() as i32).collect();
        self.picked.click(
            &order,
            &mut self.selected,
            id,
            Click::from_keys(ctrl, shift),
        );
        self.reveal_sound();
        self.mark_picked();
        self.refresh_marks();
    }

    /// The sources selected together (two or more) when the main picture shows the
    /// marks; empty for a single selection.
    fn group_ids(&self) -> Vec<i32> {
        if self.marks_on() != OutputCanvas::Main {
            return Vec::new();
        }
        let ids = self.picked.ids(self.selected);
        if ids.len() > 1 { ids } else { Vec::new() }
    }

    /// A press on the main picture: a drag of the covering box while several sources
    /// are selected and it hits one, else the single-source press.
    pub(super) fn press_edit(
        &mut self,
        at: [f32; 2],
        placed: &[Placed],
        selected: i32,
        keys: DragKeys,
    ) -> Effect {
        let several = self.group_ids();
        let grouped = (!several.is_empty())
            .then(|| {
                self.edit
                    .press_group(at, self.canvas_px(), placed, &several, keys)
            })
            .flatten();
        grouped.unwrap_or_else(|| {
            self.edit
                .press(at, self.canvas_px(), placed, selected, keys)
        })
    }

    /// Arrow keys: all selected sources move together, else the selected one.
    pub(super) fn nudge_edit(&mut self, placed: &[Placed], dx: f32, dy: f32) -> Effect {
        let several = self.group_ids();
        if several.is_empty() {
            self.edit.nudge(placed, self.selected, dx, dy)
        } else {
            self.edit.nudge_group(placed, &several, dx, dy)
        }
    }

    /// The selection's marks: one covering box for several sources, else the source's.
    pub(super) fn marks_edit(&self, placed: &[Placed], buf: &mut Vec<Mark>) {
        let several = self.group_ids();
        if several.is_empty() {
            self.edit
                .marks(self.canvas_px(), placed, self.selected, buf);
        } else {
            self.edit
                .group_marks(self.canvas_px(), placed, &several, buf);
        }
    }

    /// Marks the Sources rows: companion nesting and which rows are part of a
    /// multi-selection (sources that no longer exist leave the set).
    pub(super) fn mark_rows(&mut self, data: &mut [SourceData], keys: &[sound_nest::Key]) {
        let order: Vec<i32> = data.iter().map(|d| d.id).collect();
        self.picked.retain(&order);
        self.nest.mark(data, keys, self.selected);
        for d in data {
            d.picked = self.picked.has(d.id);
        }
    }

    /// A plain selection of one source (or none, -1): ends any multi-selection.
    pub(super) fn select_one(&mut self, id: i32) {
        self.clear_picked();
        self.selected = id;
    }

    /// Forgets several selected sources (a plain selection) and unmarks their rows.
    pub(crate) fn clear_picked(&mut self) {
        if self.picked.0.is_empty() {
            return;
        }
        self.picked.clear();
        self.mark_picked();
    }

    /// Brings each Sources row's `picked` flag in line with the set. The rows are
    /// only made again when the sources change, so a click must update them here.
    pub(crate) fn mark_picked(&self) {
        for i in 0..self.sources.row_count() {
            if let Some(mut row) = self.sources.row_data(i) {
                let picked = self.picked.has(row.id);
                if row.picked != picked {
                    row.picked = picked;
                    self.sources.set_row_data(i, row);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDER: [i32; 5] = [1, 2, 3, 4, 5];

    #[test]
    fn toggle_adds_and_removes_and_one_left_is_a_single_selection() {
        let (mut p, mut sel) = (Picked::default(), 2);
        p.click(&ORDER, &mut sel, 4, Click::Toggle);
        assert_eq!((p.ids(sel), sel), (vec![2, 4], 4));
        p.click(&ORDER, &mut sel, 4, Click::Toggle);
        assert_eq!((p.ids(sel), sel), (vec![2], 2));
        assert!(!p.has(2), "one source is not a set");
    }

    #[test]
    fn toggle_from_nothing_selects_the_row() {
        let (mut p, mut sel) = (Picked::default(), -1);
        p.click(&ORDER, &mut sel, 3, Click::Toggle);
        assert_eq!((p.ids(sel), sel), (vec![3], 3));
    }

    #[test]
    fn range_runs_from_the_primary_row_in_either_direction() {
        let (mut p, mut sel) = (Picked::default(), 4);
        p.click(&ORDER, &mut sel, 2, Click::Range);
        assert_eq!(p.ids(sel), vec![2, 3, 4]);
        assert_eq!(sel, 4, "the anchor stays");
    }

    #[test]
    fn plain_click_clears_the_set() {
        let (mut p, mut sel) = (Picked::default(), 1);
        p.click(&ORDER, &mut sel, 3, Click::Toggle);
        p.click(&ORDER, &mut sel, 5, Click::Plain);
        assert_eq!((p.ids(sel), sel), (vec![5], 5));
    }

    #[test]
    fn removed_sources_leave_the_set() {
        let (mut p, mut sel) = (Picked::default(), 1);
        p.click(&ORDER, &mut sel, 2, Click::Toggle);
        p.retain(&[1, 3]);
        assert!(!p.has(1) && !p.has(2));
    }
}
