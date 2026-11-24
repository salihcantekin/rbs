//! Selecting several sources in the Sources list: Ctrl/Cmd+click toggles a row and
//! Shift+click selects the range from the last plain-selected row. The set only
//! exists while it holds two or more sources; with one or none, the single selection
//! (`StudioUi::selected`) is the whole story. Runs on a click and when the rows are
//! made again, never per frame.

use std::collections::BTreeSet;

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
        self.refresh_marks();
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
