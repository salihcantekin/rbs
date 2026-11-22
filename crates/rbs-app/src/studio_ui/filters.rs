//! The Filters and Audio tabs of Properties (ADR-0062): the rows the UI shows and the
//! commands its controls send. A slider drag shows on the picture through
//! [`Command::PreviewFilter`] and becomes one undo step on release.

use std::collections::BTreeMap;
use std::rc::Rc;

use rbs_app::studio_host::filters::{AnyFilter, FilterRow};
use rbs_app::studio_host::{Command, SourceRow};
use rbs_filters::Values;
use rbs_params::{Curve, FieldKind, Schema, Value, color_text, parse_color};
use rbs_scene::model::{FilterId, SourceId};
use rbs_sources::permissions::PermissionKind;
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
    /// The live meters of the shown filters that have one, by filter id.
    meters: BTreeMap<i32, std::sync::Arc<rbs_audio_filters::FilterMeter>>,
    /// Each source's Filters list and each filter's fields, kept across updates and
    /// changed row by row: a new model per update would make every Sources row look
    /// changed and rewrite them all (audit U-08).
    chains: Models<FilterData>,
    fields: Models<FilterFieldData>,
}

/// List models kept by id (`VecModel` has no `Debug`, so this shows only the ids).
struct Models<T>(BTreeMap<i32, Rc<VecModel<T>>>);

impl<T> Default for Models<T> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<T> std::fmt::Debug for Models<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.0.keys()).finish()
    }
}

/// The level bar of a noise gate spans this range, the range of its thresholds (dB).
const BAR_MIN_DB: f32 = -80.0;

/// A level in dB as a position on the bar, 0 to 1.
fn bar_position(db: f32) -> f32 {
    ((db - BAR_MIN_DB) / -BAR_MIN_DB).clamp(0.0, 1.0)
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

/// The unit as shown after a value: "°" and a ratio's ":1" directly, others after a
/// space.
fn suffix(unit: &str) -> String {
    if unit.is_empty() || unit == "°" || unit.starts_with(':') {
        unit.to_owned()
    } else {
        format!(" {unit}")
    }
}

/// The Add filter menu's value for the voice chain: noise suppression, equaliser (low
/// cut on), compressor and limiter in one undo step.
pub const VOICE_CHAIN: &str = "voice-chain";

/// The equaliser's response as path commands in a 100 x 48 view box (x: 20 Hz to
/// 20 kHz on a log scale, y: +24 dB at 0, the top, to -24 dB at 48, the bottom), and in words
/// for screen readers (U16). Drawn for 48 kHz: below 20 kHz the rate barely moves it.
fn eq_curve(values: &Values) -> (String, String) {
    use std::fmt::Write as _;
    let setup = rbs_audio_filters::DspSetup::new(
        rbs_core::media::AudioFormat::STEREO_48K,
        rbs_core::media::DEFAULT_BLOCK_FRAMES,
    );
    let eq = rbs_audio_filters::equaliser::EqualiserDsp::new(setup, values);
    const POINTS: u32 = 100;
    let mut path = String::new();
    let (mut low, mut high) = (0.0f32, 0.0f32);
    for i in 0..=POINTS {
        let x = i as f32 * 100.0 / POINTS as f32;
        let freq = 20.0 * 1000f32.powf(x / 100.0);
        let db = eq.response_db(freq);
        low = low.min(db);
        high = high.max(db);
        let y = 24.0 - db.clamp(-24.0, 24.0);
        let _ = write!(path, "{}{x:.2} {y:.2} ", if i == 0 { "M " } else { "L " });
    }
    let words = if low > -0.05 && high < 0.05 {
        "flat".to_owned()
    } else {
        format!("from {low:.1} dB to {high:+.1} dB")
    };
    (path, words)
}

/// `commands` (only `M` and `L` with x y pairs) from a `vb_w` x `vb_h` view box to
/// `w` x `h` pixels: Slint's Path keeps the view box's aspect, and a curve must fill a
/// box of any width.
pub fn fit_path(commands: &str, vb_w: f32, vb_h: f32, w: f32, h: f32) -> String {
    use std::fmt::Write as _;
    if vb_w <= 0.0 || vb_h <= 0.0 {
        return String::new();
    }
    let (sx, sy) = (w / vb_w, h / vb_h);
    let mut out = String::with_capacity(commands.len());
    let mut words = commands.split_whitespace();
    while let Some(word) = words.next() {
        match word {
            "M" | "L" => {
                let (Some(x), Some(y)) = (
                    words.next().and_then(|v| v.parse::<f32>().ok()),
                    words.next().and_then(|v| v.parse::<f32>().ok()),
                ) else {
                    return String::new();
                };
                let _ = write!(out, "{word} {:.1} {:.1} ", x * sx, y * sy);
            }
            _ => return String::new(),
        }
    }
    out
}

/// Milliseconds from ns: whole when they are, else one decimal ("10 ms", "19.3 ms").
fn ms_text(ns: i64) -> String {
    let tenths = (ns + 50_000) / 100_000;
    if tenths % 10 == 0 {
        format!("{} ms", tenths / 10)
    } else {
        format!("{:.1} ms", tenths as f64 / 10.0)
    }
}

/// The editor's line for a filter that adds `latency_ns` on a source whose input left
/// `uncompensated_ns` of its chain's latency (ADR-0130 decision 10): empty when the
/// filter adds none; a warning when part of it was not taken back.
pub fn latency_line(latency_ns: i64, uncompensated_ns: i64) -> (String, bool) {
    if latency_ns <= 0 {
        return (String::new(), false);
    }
    if uncompensated_ns >= 50_000 {
        (
            format!(
                "Adds {}; {} could not be compensated.",
                ms_text(latency_ns),
                ms_text(uncompensated_ns)
            ),
            true,
        )
    } else {
        (format!("Adds {}, compensated.", ms_text(latency_ns)), false)
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
    // From 1 kHz up, in kHz, so "20.0 kHz" fits the fixed-width readout beside the
    // slider (`ValueReadout`); the editor's `text-at` does the same during a drag.
    if unit == "Hz" && v.abs() >= 1000.0 {
        let prec = if v.abs() < 10_000.0 { 2 } else { 1 };
        return format!("{:.prec$} kHz", v / 1000.0);
    }
    format!("{v:.prec$}{}", suffix(unit), prec = d as usize)
}

/// The sound sources a ducker's key can be, in row order: `(source id, name)`.
pub fn sound_sources(rows: &[SourceRow]) -> Vec<(u32, String)> {
    rows.iter()
        .filter(|r| {
            r.kind
                .is_some_and(rbs_app::studio_host::SourceKind::is_audio)
        })
        .map(|r| (r.id.get(), r.name.clone()))
        .collect()
}

fn field_data(
    ty: AnyFilter,
    values: &Values,
    own: SourceId,
    sounds: &[(u32, String)],
) -> Vec<FilterFieldData> {
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
                    default,
                    ..
                },
                Value::Number(n),
            ) => {
                let log = log_scaled(min, curve);
                let span = (max - min).max(f32::EPSILON);
                // Compared as shown, so a value on the default's step reads as the default.
                let shown = number_text(n, step, unit);
                let default_text = number_text(default, step, unit);
                FilterFieldData {
                    changed: shown != default_text,
                    default_text: default_text.into(),
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
            (FieldKind::Color { default }, Value::Color(c)) => FilterFieldData {
                changed: c != default,
                default_text: color_text(default).into(),
                label: f.label.into(),
                help: f.help.into(),
                kind: 1,
                colour: color_text(c).into(),
                swatch: slint::Color::from_argb_u8(c[3], c[0], c[1], c[2]),
                ..FilterFieldData::default()
            },
            // A source picker (ADR-0095); the editor lists the collection's sound sources.
            (FieldKind::Source, Value::Source(id)) => {
                // "None", then every other sound source; a key that is not among them
                // (not in this scene) shows no choice.
                let others: Vec<&(u32, String)> =
                    sounds.iter().filter(|s| s.0 != own.get()).collect();
                let choice = match id {
                    None => 0,
                    Some(id) => others
                        .iter()
                        .position(|s| s.0 == id)
                        .map_or(-1, |i| i as i32 + 1),
                };
                let options: Vec<slint::SharedString> =
                    std::iter::once(slint::SharedString::from("None"))
                        .chain(others.iter().map(|s| s.1.as_str().into()))
                        .collect();
                FilterFieldData {
                    changed: id.is_some(),
                    default_text: "None".into(),
                    label: f.label.into(),
                    help: f.help.into(),
                    kind: 3,
                    options: ModelRc::new(VecModel::from(options)),
                    choice,
                    ..FilterFieldData::default()
                }
            }
            // One of a fixed list (ADR-0130): a segmented control or a dropdown.
            (FieldKind::Choice { options, .. }, Value::Choice(i)) => {
                let default = f.kind.default_value();
                let labels: Vec<slint::SharedString> =
                    options.iter().map(|(_, label)| (*label).into()).collect();
                let default_text = match default {
                    Value::Choice(d) => options.get(usize::from(d)).map_or("", |o| o.1),
                    _ => "",
                };
                FilterFieldData {
                    changed: Value::Choice(i) != default,
                    default_text: default_text.into(),
                    label: f.label.into(),
                    help: f.help.into(),
                    kind: 4,
                    options: ModelRc::new(VecModel::from(labels)),
                    choice: i32::from(i),
                    ..FilterFieldData::default()
                }
            }
            // An on/off switch (ADR-0130).
            (FieldKind::Toggle { default }, Value::Toggle(on)) => FilterFieldData {
                changed: on != default,
                default_text: if default { "on" } else { "off" }.into(),
                label: f.label.into(),
                help: f.help.into(),
                kind: 5,
                choice: i32::from(on),
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
    pub fn data(
        &mut self,
        row: &SourceRow,
        sounds: &[(u32, String)],
        permissions: &(
            Vec<(PermissionKind, rbs_sources::permissions::PermissionInfo)>,
            Option<PermissionKind>,
        ),
    ) -> (ModelRc<FilterData>, i32) {
        // Why pointer filters cannot see clicks, if they cannot (ADR-0165 decision 7).
        let pointer_note = {
            let (statuses, waiting) = permissions;
            let kind = PermissionKind::InputMonitoring;
            let info = statuses.iter().find(|(k, _)| *k == kind).map(|(_, i)| *i);
            rbs_app::permission_text::pointer_note(info, *waiting == Some(kind))
        };
        let pointer_note = pointer_note.as_ref();
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
        // Pointer filters stay before the others (ADR-0165 decision 3), so Move up
        // and Move down stop at that line, from the declared need, never a type id.
        let pointer: Vec<bool> = row
            .filters
            .iter()
            .map(|f| {
                rbs_filters::find(f.type_id.as_str())
                    .is_some_and(|t| t.needs().contains(rbs_filters::Needs::POINTER))
            })
            .collect();
        let data: Vec<FilterData> = row
            .filters
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let fields = match (f.any(), &f.values) {
                    (Some(ty), Some(values)) => field_data(ty, values, row.id, sounds),
                    _ => Vec::new(),
                };
                let id = f.id.get() as i32;
                match &f.meter {
                    Some(m) => {
                        self.meters.insert(id, std::sync::Arc::clone(&m.0));
                    }
                    None => {
                        self.meters.remove(&id);
                    }
                }
                // A filter with a live meter is a noise gate: its first two numbers are
                // the open and the close threshold.
                let (open_pos, close_pos, bar_label) = match (&f.meter, &f.values) {
                    (Some(_), Some(v)) if f.type_id == rbs_audio_filters::ducker::DUCKER => (
                        bar_position(v.number(1)),
                        // The ducker's hysteresis: 3 dB under its threshold.
                        bar_position(v.number(1) - 3.0),
                        "Key level",
                    ),
                    (Some(_), Some(v)) => (
                        bar_position(v.number(0)),
                        bar_position(v.number(1)),
                        "Level",
                    ),
                    _ => (0.0, 0.0, ""),
                };
                let (latency_text, latency_warn) =
                    latency_line(f.latency_ns, row.latency.map_or(0, |l| l.uncompensated_ns));
                let (curve, curve_text) = match (&f.values, f.type_id.as_str()) {
                    (Some(v), rbs_audio_filters::equaliser::EQUALISER) => eq_curve(v),
                    _ => (String::new(), String::new()),
                };
                FilterData {
                    curve: curve.into(),
                    curve_text: curve_text.into(),
                    latency_text: latency_text.into(),
                    latency_warn,
                    gate: f.meter.is_some(),
                    bar_label: bar_label.into(),
                    open_pos,
                    close_pos,
                    id,
                    name: f.name.as_str().into(),
                    enabled: f.enabled,
                    known: f.any().is_some(),
                    can_up: room(&pointer, i).0,
                    can_down: room(&pointer, i).1,
                    perm_text: match pointer_note {
                        Some((text, _)) if pointer[i] => text.as_str().into(),
                        _ => "".into(),
                    },
                    perm_action: match pointer_note {
                        Some((_, action)) if pointer[i] => *action,
                        _ => 0,
                    },
                    fields: {
                        let model = self.fields.0.entry(id).or_default();
                        super::set_rows(model, fields);
                        ModelRc::from(Rc::clone(model))
                    },
                }
            })
            .collect();
        let chain = self.chains.0.entry(source).or_default();
        super::set_rows(chain, data);
        (ModelRc::from(Rc::clone(chain)), selected)
    }

    /// The live levels of a filter's meter as bar positions (input peak, gain
    /// reduction); zeros when it has none. Read by the editor's timer, only while shown.
    pub fn levels(&self, filter: i32) -> (f32, f32) {
        self.meters.get(&filter).map_or((0.0, 0.0), |m| {
            (
                bar_position(m.input_peak_db()),
                (m.gain_reduction_db() / -BAR_MIN_DB).clamp(0.0, 1.0),
            )
        })
    }

    /// Forgets the selection and the chains of sources that are gone.
    pub fn retain(&mut self, rows: &[SourceRow]) {
        let ids: Vec<i32> = rows.iter().map(|r| r.id.get() as i32).collect();
        self.selected.retain(|s, _| ids.contains(s));
        self.known.retain(|s, _| ids.contains(s));
        self.chains.0.retain(|s, _| ids.contains(s));
        let filters: Vec<i32> = rows
            .iter()
            .flat_map(|r| r.filters.iter().map(|f| f.id.get() as i32))
            .collect();
        self.fields.0.retain(|f, _| filters.contains(f));
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

    /// Add filter: the filter type `type_id` on `source`. The catalogue it comes from
    /// (picture filters, or audio filters on the Audio tab) is the caller's; an id that
    /// is in neither does nothing.
    pub fn add_filter_type(&mut self, source: i32, type_id: &str) {
        if type_id == VOICE_CHAIN {
            self.deleted.clear();
            self.call(rbs_api::Command::FilterAddVoiceChain(
                rbs_api::request::SourceRef {
                    source: source as u32,
                },
            ));
            return;
        }
        if filter_types(true)
            .iter()
            .chain(&filter_types(false))
            .all(|(_, id)| id.as_str() != type_id)
        {
            return;
        }
        self.deleted.clear();
        self.call(rbs_api::Command::FilterAdd(rbs_api::request::AddFilter {
            source: source as u32,
            type_id: type_id.to_owned(),
            settings: None,
        }));
    }

    pub fn select_filter(&mut self, source: i32, filter: i32) {
        if self.filter_ui.selected.get(&source) != Some(&filter) {
            self.filter_ui.selected.insert(source, filter);
            self.refresh_rows();
        }
    }

    pub fn remove_filter(&mut self, filter: i32) {
        self.deleted.clear();
        self.call(rbs_api::Command::FilterRemove(
            rbs_api::request::FilterRef {
                filter: filter as u32,
            },
        ));
    }

    pub fn move_filter(&mut self, filter: i32, index: i32) {
        if index < 0 {
            return;
        }
        self.deleted.clear();
        self.call(rbs_api::Command::FilterMove(rbs_api::request::MoveFilter {
            filter: filter as u32,
            index: index as u32,
        }));
    }

    pub fn set_filter_enabled(&mut self, filter: i32, enabled: bool) {
        self.deleted.clear();
        self.call(rbs_api::Command::FilterSetEnabled(
            rbs_api::request::SetFilterEnabled {
                filter: filter as u32,
                enabled,
            },
        ));
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

    /// A filter's full values as `filter.set_settings` (one undo step): an unchanged
    /// release puts the stored values back on the running filter (ADR-0168).
    fn send_filter_values(&self, filter: FilterId, values: &Values) {
        let Some(ty) = self
            .filter_row(filter.get() as i32)
            .and_then(FilterRow::any)
        else {
            return;
        };
        self.call(rbs_api::Command::FilterSetSettings(
            rbs_api::request::SetFilterSettings {
                filter: filter.get(),
                settings: rbs_app::studio_host::filters::api_settings(&ty, values),
            },
        ));
    }

    /// The slider was let go: what it showed becomes one undo step.
    pub fn commit_filter(&mut self, filter: i32) {
        match self.filter_ui.drag.take() {
            Some((id, values)) if id.get() as i32 == filter => {
                self.deleted.clear();
                self.send_filter_values(id, &values);
            }
            _ => {}
        }
    }

    /// The drag was cancelled: the stored values go back on the picture.
    pub fn cancel_filter(&mut self, filter: i32) {
        self.filter_ui.drag = None;
        self.send(Command::RestoreFilter(FilterId::from_raw(filter as u32)));
    }

    /// A key source picked in a source field's list: option 0 is none, then the other
    /// sound sources in row order (the order `field_data` listed them in).
    pub fn set_filter_source(&mut self, filter: i32, field: i32, option: i32) {
        let Some((ty, values)) = self.filter_values(filter) else {
            return;
        };
        let Some(owner) = self
            .rows
            .iter()
            .find(|r| r.filters.iter().any(|f| f.id.get() as i32 == filter))
            .map(|r| r.id)
        else {
            return;
        };
        let Some(f) = ty.fields().get(field as usize) else {
            return;
        };
        let id = if option <= 0 {
            None
        } else {
            let others: Vec<u32> = sound_sources(&self.rows)
                .into_iter()
                .map(|s| s.0)
                .filter(|id| *id != owner.get())
                .collect();
            match others.get(option as usize - 1) {
                Some(id) => Some(*id),
                None => return,
            }
        };
        let Ok(values) = values.with(&ty, [(f.key, Value::Source(id))]) else {
            return;
        };
        self.deleted.clear();
        self.send_filter_values(FilterId::from_raw(filter as u32), &values);
    }

    /// An option picked in a choice field, or a switch flipped (`option` 0 or 1): one
    /// undo step.
    pub fn set_filter_option(&mut self, filter: i32, field: i32, option: i32) {
        let Some((ty, values)) = self.filter_values(filter) else {
            return;
        };
        let Some(f) = ty.fields().get(field as usize) else {
            return;
        };
        let value = match f.kind {
            FieldKind::Choice { options, .. } => match u8::try_from(option) {
                Ok(i) if usize::from(i) < options.len() => Value::Choice(i),
                _ => return,
            },
            FieldKind::Toggle { .. } => Value::Toggle(option != 0),
            _ => return,
        };
        let Ok(values) = values.with(&ty, [(f.key, value)]) else {
            return;
        };
        self.deleted.clear();
        self.send_filter_values(FilterId::from_raw(filter as u32), &values);
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
            self.send_filter_values(FilterId::from_raw(filter as u32), &values);
        }
    }

    /// Puts one field of a filter back to its schema default (one change).
    pub fn reset_filter_field(&mut self, filter: i32, field: i32) {
        let Some((ty, values)) = self.filter_values(filter) else {
            return;
        };
        let Some(f) = ty.fields().get(field as usize) else {
            return;
        };
        if let Ok(values) = values.with(&ty, [(f.key, f.kind.default_value())]) {
            self.deleted.clear();
            self.send_filter_values(FilterId::from_raw(filter as u32), &values);
        }
    }

    pub fn reset_filter(&mut self, filter: i32) {
        let Some(ty) = self.filter_row(filter).and_then(FilterRow::any) else {
            return;
        };
        self.deleted.clear();
        self.send_filter_values(FilterId::from_raw(filter as u32), &Values::defaults(&ty));
    }
}

/// Whether the platform's input serves the pointer to filters, set once from the
/// adapters (spec `video-filters.md`, "Pointer types").
/// Whether the filter at `i` can move up and down, given which filters of the chain
/// are pointer filters: not past either end, nor across the line after the pointer
/// filters (ADR-0165 decision 3; the host refuses that move).
fn room(pointer: &[bool], i: usize) -> (bool, bool) {
    let same = |j: usize| pointer.get(j) == pointer.get(i);
    (i > 0 && same(i - 1), i + 1 < pointer.len() && same(i + 1))
}

static POINTER_SERVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Sets what the platform serves to filters; Add filter then offers only the picture
/// types it can run.
pub fn set_served(serves: rbs_filters::Needs) {
    let pointer = serves.contains(rbs_filters::Needs::POINTER);
    POINTER_SERVED.store(pointer, std::sync::atomic::Ordering::Relaxed);
}

fn served() -> rbs_filters::Needs {
    if POINTER_SERVED.load(std::sync::atomic::Ordering::Relaxed) {
        rbs_filters::Needs::POINTER
    } else {
        rbs_filters::Needs::NONE
    }
}

/// The filter types Add filter offers, `(name, type id)`: the audio catalogue for the
/// Audio tab (`audio`), else the picture types the platform can run (ADR-0082).
pub fn filter_types(audio: bool) -> Vec<(String, String)> {
    if audio {
        // The voice chain first: four filters in one step (ADR-0130 decision 10).
        std::iter::once(("Voice chain (4 filters)".to_owned(), VOICE_CHAIN.to_owned()))
            .chain(
                rbs_audio_filters::catalogue()
                    .iter()
                    .map(|t| (t.name().to_owned(), t.type_id().as_str().to_owned())),
            )
            .collect()
    } else {
        rbs_filters::offered(served())
            .map(|t| (t.name().to_owned(), t.type_id().as_str().to_owned()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_stop_at_the_ends_and_at_the_pointer_line() {
        let chain = [true, true, false, false];
        assert_eq!(room(&chain, 0), (false, true));
        assert_eq!(room(&chain, 1), (true, false), "last pointer filter");
        assert_eq!(room(&chain, 2), (false, true), "first other filter");
        assert_eq!(room(&chain, 3), (true, false));
        assert_eq!(room(&[false], 0), (false, false));
    }

    #[test]
    #[allow(clippy::unwrap_used)] // Tests fail by panicking.
    fn add_filter_offers_each_catalogue_by_stable_type_id() {
        use rbs_ui::command::{CommandId, MAX_CONTEXT_ITEMS};
        for audio in [false, true] {
            let types = filter_types(audio);
            assert!(!types.is_empty());
            // The menu shows them all, and each id can be run by id.
            assert!(
                types.len() < MAX_CONTEXT_ITEMS,
                "group the filter types before they overflow the menu"
            );
            for (name, id) in &types {
                assert!(!name.is_empty());
                let run = CommandId::with_params(
                    "filter.add",
                    &[
                        ("source", "4294967295"),
                        ("collection", "A long collection name here"),
                        ("type", id),
                    ],
                )
                .unwrap();
                assert!(run.to_string().len() <= 128, "{run}");
            }
        }
        // The two catalogues share no type id.
        assert!(
            filter_types(true)
                .iter()
                .all(|(_, a)| filter_types(false).iter().all(|(_, b)| a != b))
        );
    }

    #[test]
    fn pointer_types_are_offered_only_where_the_platform_serves_the_pointer() {
        // Nothing in this test binary sets what the platform serves.
        let ids: Vec<String> = filter_types(false).into_iter().map(|(_, id)| id).collect();
        assert!(!ids.iter().any(|id| id == rbs_filters::CLICK_RINGS));
        assert!(!ids.iter().any(|id| id == rbs_filters::POINTER_HALO));
    }

    #[test]
    fn a_path_is_stretched_to_its_box() {
        assert_eq!(
            fit_path("M 0 24 L 100 0", 100.0, 48.0, 200.0, 72.0),
            "M 0.0 36.0 L 200.0 0.0 "
        );
        assert_eq!(fit_path("C 1 2", 100.0, 48.0, 200.0, 72.0), "");
    }

    #[test]
    fn a_flat_equaliser_is_a_line_through_the_middle() {
        let (path, words) = eq_curve(&Values::defaults(
            &rbs_audio_filters::equaliser::Equaliser as &dyn rbs_audio_filters::AudioFilterType,
        ));
        assert_eq!(words, "flat");
        assert!(path.starts_with("M 0.00 24.00 L "), "{path}");
    }

    #[test]
    fn a_latent_filter_says_what_it_adds() {
        assert_eq!(latency_line(0, 0), (String::new(), false));
        assert_eq!(
            latency_line(10_000_000, 0),
            ("Adds 10 ms, compensated.".to_owned(), false)
        );
        assert_eq!(
            latency_line(19_333_333, 0),
            ("Adds 19.3 ms, compensated.".to_owned(), false)
        );
        assert_eq!(
            latency_line(12_000_000, 4_000_000),
            (
                "Adds 12 ms; 4 ms could not be compensated.".to_owned(),
                true
            )
        );
    }

    #[test]
    fn numbers_show_the_decimals_their_step_needs() {
        assert_eq!(number_text(12.0, 1.0, "px"), "12 px");
        assert_eq!(number_text(-45.0, 1.0, "°"), "-45°");
        assert_eq!(number_text(0.3, 0.01, ""), "0.30");
        assert_eq!(number_text(-0.001, 0.01, ""), "0.00");
        assert_eq!(number_text(4.0, 0.1, ":1"), "4.0:1");
        assert_eq!(number_text(400.0, 1.0, "Hz"), "400 Hz");
        assert_eq!(number_text(3000.0, 1.0, "Hz"), "3.00 kHz");
        assert_eq!(number_text(20000.0, 1.0, "Hz"), "20.0 kHz");
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
