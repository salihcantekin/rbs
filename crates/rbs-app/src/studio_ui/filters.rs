//! The Filters and Audio tabs of Properties (ADR-0062): the rows the UI shows and the
//! commands its controls send. A slider drag shows on the picture through
//! [`Command::PreviewFilter`] and becomes one undo step on release.

use std::collections::BTreeMap;

use rbs_app::studio_host::filters::{AnyFilter, FilterRow};
use rbs_app::studio_host::{Command, SourceRow};
use rbs_filters::Values;
use rbs_params::{Curve, FieldKind, Schema, Value, color_text, parse_color};
use rbs_scene::model::{FilterId, SourceId};
use slint::{ModelRc, VecModel};

use super::StudioUi;
use crate::ui::{FilterData, FilterFieldData};

/// What the Filters tab remembers between updates.
#[derive(Debug, Default)]
pub struct FilterUi {
    /// The selected filter of each source (by id), while it is in the chain.
    selected: BTreeMap<i32, i32>,
    /// The filters each source had at the last update: a new one is selected.
    known: BTreeMap<i32, Vec<i32>>,
    /// The values a slider drag shows on the picture, until it is released.
    drag: Option<(FilterId, Values)>,
}

/// Whether a number's slider is log-scaled: only for a log curve over a positive range.
fn log_scaled(min: f32, curve: Curve) -> bool {
    matches!(curve, Curve::Log) && min > 0.0
}

/// A number's slider position (0 to 1): linear, or log between `min` and `max`.
fn position_of(n: f32, min: f32, max: f32, log: bool) -> f32 {
    if log {
        ((n / min).ln() / (max / min).ln()).clamp(0.0, 1.0)
    } else {
        ((n - min) / (max - min).max(f32::EPSILON)).clamp(0.0, 1.0)
    }
}

/// Decimals a value on `step` needs.
fn decimals(step: f32) -> i32 {
    if step >= 1.0 {
        0
    } else if step >= 0.1 {
        1
    } else {
        2
    }
}

/// The unit as shown after a value: "°" directly, others after a space.
fn suffix(unit: &str) -> String {
    if unit.is_empty() || unit == "°" {
        unit.to_owned()
    } else {
        format!(" {unit}")
    }
}

/// A number with as many decimals as its step needs, and its unit.
pub fn number_text(v: f32, step: f32, unit: &str) -> String {
    let d = decimals(step);
    let v = if v.abs() < 0.5 * 10f32.powi(-d) {
        0.0
    } else {
        v
    };
    format!("{v:.prec$}{}", suffix(unit), prec = d as usize)
}

fn field_data(ty: AnyFilter, values: &Values) -> Vec<FilterFieldData> {
    ty.fields()
        .iter()
        .zip(values.as_slice())
        .map(|(f, v)| match (f.kind, *v) {
            (
                FieldKind::Number {
                    min,
                    max,
                    step,
                    unit,
                    curve,
                    ..
                },
                Value::Number(n),
            ) => {
                let log = log_scaled(min, curve);
                let span = (max - min).max(f32::EPSILON);
                FilterFieldData {
                    label: f.label.into(),
                    help: f.help.into(),
                    log,
                    kind: 0,
                    position: position_of(n, min, max, log),
                    step: if log { 0.02 } else { step / span },
                    value_text: number_text(n, step, unit).into(),
                    min,
                    max,
                    snap: step,
                    decimals: decimals(step),
                    suffix: suffix(unit).into(),
                    ..FilterFieldData::default()
                }
            }
            (FieldKind::Color { .. }, Value::Color(c)) => FilterFieldData {
                label: f.label.into(),
                help: f.help.into(),
                kind: 1,
                colour: color_text(c).into(),
                swatch: slint::Color::from_argb_u8(c[3], c[0], c[1], c[2]),
                ..FilterFieldData::default()
            },
            _ => FilterFieldData {
                label: f.label.into(),
                help: f.help.into(),
                kind: 2,
                ..FilterFieldData::default()
            },
        })
        .collect()
}

impl FilterUi {
    /// The Filters tab's rows of `row` and its selected filter (-1 none).
    pub fn data(&mut self, row: &SourceRow) -> (ModelRc<FilterData>, i32) {
        let source = row.id.get() as i32;
        let ids: Vec<i32> = row.filters.iter().map(|f| f.id.get() as i32).collect();
        // A filter that was not there before was just added: select it.
        if let Some(old) = self.known.get(&source)
            && let Some(new) = ids.iter().rev().find(|id| !old.contains(id))
        {
            self.selected.insert(source, *new);
        }
        self.known.insert(source, ids.clone());
        let selected = match self.selected.get(&source) {
            Some(id) if ids.contains(id) => *id,
            _ => ids.first().copied().unwrap_or(-1),
        };
        let last = row.filters.len().saturating_sub(1);
        let data: Vec<FilterData> = row
            .filters
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let fields = match (f.any(), &f.values) {
                    (Some(ty), Some(values)) => field_data(ty, values),
                    _ => Vec::new(),
                };
                FilterData {
                    id: f.id.get() as i32,
                    name: f.name.as_str().into(),
                    enabled: f.enabled,
                    known: f.any().is_some(),
                    can_up: i > 0,
                    can_down: i < last,
                    fields: ModelRc::new(VecModel::from(fields)),
                }
            })
            .collect();
        (ModelRc::new(VecModel::from(data)), selected)
    }

    /// Forgets the selection and the chains of sources that are gone.
    pub fn retain(&mut self, rows: &[SourceRow]) {
        let ids: Vec<i32> = rows.iter().map(|r| r.id.get() as i32).collect();
        self.selected.retain(|s, _| ids.contains(s));
        self.known.retain(|s, _| ids.contains(s));
    }
}

impl StudioUi {
    fn filter_row(&self, filter: i32) -> Option<&FilterRow> {
        self.rows
            .iter()
            .flat_map(|r| r.filters.iter())
            .find(|f| f.id.get() as i32 == filter)
    }

    /// The type and the values the filter's controls start from: a drag's, else the
    /// values on air.
    fn filter_values(&self, filter: i32) -> Option<(AnyFilter, Values)> {
        let row = self.filter_row(filter)?;
        let ty = row.any()?;
        let values = match &self.filter_ui.drag {
            Some((id, v)) if id.get() as i32 == filter => *v,
            _ => row.values?,
        };
        Some((ty, values))
    }

    /// The number of field `field` at slider position `position`, on its step.
    fn number_at(ty: AnyFilter, field: usize, position: f32) -> Option<(&'static str, f32)> {
        let f = ty.fields().get(field)?;
        let FieldKind::Number {
            min,
            max,
            step,
            curve,
            ..
        } = f.kind
        else {
            return None;
        };
        let p = position.clamp(0.0, 1.0);
        let raw = if log_scaled(min, curve) {
            min * (max / min).powf(p)
        } else {
            min + p * (max - min)
        };
        let v = if step > 0.0 {
            (min + ((raw - min) / step).round() * step).clamp(min, max)
        } else {
            raw
        };
        Some((f.key, v))
    }

    pub fn set_source_sync_offset(&mut self, id: i32, ms: i32) {
        self.send(Command::SetSourceSyncOffset {
            id: SourceId::from_raw(id as u32),
            offset_ms: ms.clamp(-200, 200),
        });
    }

    pub fn add_filter(&mut self, source: i32, type_index: i32) {
        // A source with a picture takes video filters, one with only sound audio filters
        // (ADR-0082); the menu the user picked from follows the same rule.
        let sound_only = self
            .rows
            .iter()
            .find(|r| r.id.get() as i32 == source)
            .is_some_and(|r| r.picture.is_none() && r.kind.is_some_and(|k| k.is_audio()));
        let type_id = if sound_only {
            rbs_audio_filters::catalogue()
                .get(type_index as usize)
                .map(|t| t.type_id())
        } else {
            rbs_filters::catalogue()
                .get(type_index as usize)
                .map(|t| t.type_id())
        };
        let Some(type_id) = type_id else {
            return;
        };
        self.deleted.clear();
        self.send(Command::AddFilter {
            source: SourceId::from_raw(source as u32),
            type_id: type_id.as_str().to_owned(),
        });
    }

    pub fn select_filter(&mut self, source: i32, filter: i32) {
        if self.filter_ui.selected.get(&source) != Some(&filter) {
            self.filter_ui.selected.insert(source, filter);
            self.refresh_rows();
        }
    }

    pub fn remove_filter(&mut self, filter: i32) {
        self.deleted.clear();
        self.send(Command::RemoveFilter(FilterId::from_raw(filter as u32)));
    }

    pub fn move_filter(&mut self, filter: i32, index: i32) {
        if index < 0 {
            return;
        }
        self.deleted.clear();
        self.send(Command::MoveFilter {
            filter: FilterId::from_raw(filter as u32),
            index: index as usize,
        });
    }

    pub fn set_filter_enabled(&mut self, filter: i32, enabled: bool) {
        self.deleted.clear();
        self.send(Command::SetFilterEnabled {
            filter: FilterId::from_raw(filter as u32),
            enabled,
        });
    }

    /// A slider moved: the picture shows the new value; nothing is stored yet.
    pub fn preview_filter(&mut self, filter: i32, field: i32, position: f32) {
        let Some((ty, values)) = self.filter_values(filter) else {
            return;
        };
        let Some((key, v)) = Self::number_at(ty, field as usize, position) else {
            return;
        };
        let Ok(values) = values.with(&ty, [(key, Value::Number(v))]) else {
            return;
        };
        let id = FilterId::from_raw(filter as u32);
        self.filter_ui.drag = Some((id, values));
        self.send(Command::PreviewFilter { filter: id, values });
    }

    /// The slider was let go: what it showed becomes one undo step.
    pub fn commit_filter(&mut self, filter: i32) {
        match self.filter_ui.drag.take() {
            Some((id, values)) if id.get() as i32 == filter => {
                self.deleted.clear();
                self.send(Command::SetFilterValues { filter: id, values });
            }
            _ => {}
        }
    }

    /// The drag was cancelled: the stored values go back on the picture.
    pub fn cancel_filter(&mut self, filter: i32) {
        self.filter_ui.drag = None;
        self.send(Command::RestoreFilter(FilterId::from_raw(filter as u32)));
    }

    /// A colour picked or typed (`#rrggbb` or `#rrggbbaa`; anything else changes
    /// nothing).
    pub fn set_filter_colour(&mut self, filter: i32, field: i32, text: &str) {
        let Some(colour) = parse_color(text.trim()) else {
            return;
        };
        let Some((ty, values)) = self.filter_values(filter) else {
            return;
        };
        let Some(f) = ty.fields().get(field as usize) else {
            return;
        };
        if let Ok(values) = values.with(&ty, [(f.key, Value::Color(colour))]) {
            self.deleted.clear();
            self.send(Command::SetFilterValues {
                filter: FilterId::from_raw(filter as u32),
                values,
            });
        }
    }

    pub fn reset_filter(&mut self, filter: i32) {
        let Some(ty) = self.filter_row(filter).and_then(FilterRow::any) else {
            return;
        };
        self.deleted.clear();
        self.send(Command::SetFilterValues {
            filter: FilterId::from_raw(filter as u32),
            values: Values::defaults(&ty),
        });
    }

    /// The video filter type names for Add filter.
    pub fn filter_type_names() -> Vec<String> {
        rbs_filters::catalogue()
            .iter()
            .map(|t| t.name().to_owned())
            .collect()
    }

    /// The audio filter type names for Add filter on a source with only sound.
    pub fn audio_filter_type_names() -> Vec<String> {
        rbs_audio_filters::catalogue()
            .iter()
            .map(|t| t.name().to_owned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_show_the_decimals_their_step_needs() {
        assert_eq!(number_text(12.0, 1.0, "px"), "12 px");
        assert_eq!(number_text(-45.0, 1.0, "°"), "-45°");
        assert_eq!(number_text(0.3, 0.01, ""), "0.30");
        assert_eq!(number_text(-0.001, 0.01, ""), "0.00");
    }

    #[test]
    fn log_sliders_are_even_in_ratio() {
        // 0.1 to 100 ms: the middle of the slider is the geometric mean, 3.16 ms.
        let mid = position_of(10f32.powf(0.5), 0.1, 100.0, true);
        assert!((mid - 0.5).abs() < 1e-5, "{mid}");
        assert_eq!(position_of(0.1, 0.1, 100.0, true), 0.0);
        assert_eq!(position_of(100.0, 0.1, 100.0, true), 1.0);
        assert!((position_of(5.0, 0.0, 10.0, false) - 0.5).abs() < 1e-6);
        assert!(!log_scaled(0.0, Curve::Log), "a range from 0 cannot be log");
    }
}
