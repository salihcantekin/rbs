//! The meters' accessible values (U16), kept apart from the Mixer rows.
//!
//! Slint redraws the window when a model row it draws from changes, even when every
//! drawn part stays the same. The accessible value changes about once a second while
//! a source makes sound, so as a field of `MeterRow` it cost one full redraw a second
//! with the bars on the native layer (ADR-0035). Here it is a model of its own,
//! `Studio.meter-values` (and `master-meter-values`), read only by the meters'
//! accessible elements, so writing it redraws nothing.
//!
//! `rows` is aligned with the Mixer rows (`Studio.meters`): row `i` of one is row `i`
//! of the other. `index` maps a source id to that row.

use std::collections::HashMap;
use std::rc::Rc;

use slint::{Model, ModelRc, SharedString, VecModel};

use crate::ui::{MeterRow, Studio};

/// The value of a meter that has heard nothing yet.
const SILENT: &str = "silent";

#[derive(Default)]
pub(super) struct MeterValues {
    /// Source id to its Mixer row. Rebuilt when that row set changes, not on a tick.
    pub(super) index: HashMap<i32, usize>,
    rows: Rc<VecModel<SharedString>>,
    /// The master's value: one row while the master strip shows, else none.
    master: Rc<VecModel<SharedString>>,
}

impl MeterValues {
    /// Gives a window's `Studio` global the shared models.
    pub(super) fn attach(&self, g: &Studio<'_>) {
        g.set_meter_values(ModelRc::from(self.rows.clone()));
        g.set_master_meter_values(ModelRc::from(self.master.clone()));
    }

    /// The values for a new set of Mixer rows, to pass to [`Self::set_rows`] after the
    /// index is rebuilt: a running source keeps its value, any other reads silent.
    pub(super) fn carried(&self, rows: &[MeterRow]) -> Vec<SharedString> {
        rows.iter()
            .map(|row| {
                row.live
                    .then(|| self.index.get(&row.id))
                    .flatten()
                    .and_then(|&i| self.rows.row_data(i))
                    .unwrap_or_else(|| SILENT.into())
            })
            .collect()
    }

    /// The values aligned with the Mixer rows just set.
    pub(super) fn set_rows(&self, values: Vec<SharedString>) {
        super::set_rows(&self.rows, values);
    }

    /// Writes the value of the Mixer row `index` when it changed.
    pub(super) fn set(&self, index: usize, text: &str) {
        write(&self.rows, index, text);
    }

    /// Writes the master's value when it changed and the master strip shows.
    pub(super) fn set_master(&self, text: &str) {
        write(&self.master, 0, text);
    }

    /// The master strip shows (one value) or not (none).
    pub(super) fn show_master(&self, shown: bool) {
        match (shown, self.master.row_count()) {
            (true, 0) => self.master.set_vec(vec![SharedString::from(SILENT)]),
            (false, n) if n > 0 => self.master.set_vec(Vec::new()),
            _ => {}
        }
    }

    /// No Mixer rows and no master.
    pub(super) fn clear(&mut self) {
        self.index.clear();
        self.rows.set_vec(Vec::new());
        self.master.set_vec(Vec::new());
    }
}

fn write(model: &VecModel<SharedString>, index: usize, text: &str) {
    if model.row_data(index).is_some_and(|old| old != text) {
        model.set_row_data(index, text.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i32, live: bool) -> MeterRow {
        MeterRow {
            id,
            live,
            ..MeterRow::default()
        }
    }

    #[test]
    fn a_value_is_written_apart_from_the_rows_and_only_when_it_changes() {
        let values = MeterValues {
            index: HashMap::from([(4, 0), (9, 1)]),
            ..MeterValues::default()
        };
        let carried = values.carried(&[row(4, true), row(9, true)]);
        assert_eq!(carried, ["silent", "silent"], "nothing heard yet");
        values.set_rows(carried);
        values.set(1, "-12 dBFS");
        assert_eq!(values.rows.row_data(1).as_deref(), Some("-12 dBFS"));
        values.set(5, "-3 dBFS");
        assert_eq!(
            values.rows.row_count(),
            2,
            "a row that is not there is not made"
        );
    }

    #[test]
    fn a_running_source_keeps_its_value_when_the_rows_change() {
        let values = MeterValues {
            index: HashMap::from([(4, 0), (9, 1)]),
            ..MeterValues::default()
        };
        values.set_rows(vec!["-6 dBFS".into(), "-20 dBFS".into()]);
        // 9 moves first, 4 stops running, 2 is new.
        let next = values.carried(&[row(9, true), row(4, false), row(2, true)]);
        assert_eq!(next, ["-20 dBFS", "silent", "silent"]);
    }

    #[test]
    fn the_master_has_a_value_only_while_its_strip_shows() {
        let mut values = MeterValues::default();
        values.set_master("-1 dBFS");
        assert_eq!(values.master.row_count(), 0);
        values.show_master(true);
        values.set_master("-1 dBFS");
        assert_eq!(values.master.row_data(0).as_deref(), Some("-1 dBFS"));
        values.show_master(true);
        assert_eq!(
            values.master.row_data(0).as_deref(),
            Some("-1 dBFS"),
            "showing it again keeps the value"
        );
        values.clear();
        assert_eq!((values.master.row_count(), values.rows.row_count()), (0, 0));
    }
}
