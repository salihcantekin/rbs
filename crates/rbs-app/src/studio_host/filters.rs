//! Video filters and the sync offset in the host (ADR-0062): the rows the UI and the
//! control API show, and the edits both make.

use std::collections::BTreeMap;

use rbs_api::types as api;
use rbs_filters::{FieldKind, FilterType, Value, Values, color_text, parse_color};
use rbs_scene::SceneCollection;
use rbs_scene::model::{Filter, FilterId, Settings, Source, SourceId};
use rbs_sources::{filter_settings, filter_values_lenient};

use super::{Host, Refusal, SourceKind};
use crate::edit_history::EditKind;

/// One filter of a source, as the Filters tab and the API show it.
#[derive(Debug, Clone, PartialEq)]
pub struct FilterRow {
    /// Its id in the collection.
    pub id: FilterId,
    /// Its type id.
    pub type_id: String,
    /// Display name, numbered from the second filter of the same type ("Blur 2").
    pub name: String,
    /// Applied or not.
    pub enabled: bool,
    /// The values it runs on air; `None` for a type this build does not have.
    pub values: Option<Values>,
    /// Its stored settings (shown as they are for an unknown type).
    pub stored: Settings,
}

impl FilterRow {
    /// The built-in type, if this build has it.
    pub fn ty(&self) -> Option<&'static dyn FilterType> {
        rbs_filters::find(&self.type_id)
    }
}

/// The rows of a source's filters, in chain order.
pub fn rows(source: &Source) -> Vec<FilterRow> {
    let mut seen: BTreeMap<&str, u32> = BTreeMap::new();
    source
        .filters
        .iter()
        .map(|f| {
            let ty = rbs_filters::find(f.type_id.as_str());
            let base = ty.map_or_else(|| f.type_id.as_str().to_owned(), |t| t.name().to_owned());
            let n = seen.entry(f.type_id.as_str()).or_insert(0);
            *n += 1;
            let name = if *n == 1 { base } else { format!("{base} {n}") };
            FilterRow {
                id: f.id,
                type_id: f.type_id.as_str().to_owned(),
                name,
                enabled: f.enabled,
                values: ty.map(|t| filter_values_lenient(t, &f.settings).0),
                stored: f.settings.clone(),
            }
        })
        .collect()
}

/// The sync offset of every source with one, in nanoseconds, for the studio's start
/// (ADR-0062 decision 10: the collection is the one source of truth).
pub fn audio_offsets(collection: &SceneCollection) -> BTreeMap<SourceId, i64> {
    collection
        .sources()
        .filter(|s| s.audio.sync_offset_ms != 0)
        .map(|s| (s.id, i64::from(s.audio.sync_offset_ms) * 1_000_000))
        .collect()
}

fn json_of(v: Value) -> serde_json::Value {
    match v {
        Value::Number(n) => format!("{n}")
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Value::Color(c) => serde_json::Value::String(color_text(c)),
        #[allow(unreachable_patterns)]
        _ => serde_json::Value::Null,
    }
}

fn json_of_stored(v: &rbs_scene::model::SettingValue) -> serde_json::Value {
    use rbs_scene::model::SettingValue as S;
    match v {
        S::Bool(b) => serde_json::Value::Bool(*b),
        S::Int(i) => serde_json::Value::from(*i),
        S::Float(f) => serde_json::Number::from_f64(*f).map_or(serde_json::Value::Null, Into::into),
        S::Text(t) => serde_json::Value::String(t.clone()),
        #[allow(unreachable_patterns)]
        _ => serde_json::Value::Null,
    }
}

/// The API's row of a filter: the values on air, or the stored settings for an
/// unknown type.
pub fn api_row(row: &FilterRow) -> api::FilterInfo {
    let settings = match (row.ty(), &row.values) {
        (Some(ty), Some(values)) => values
            .to_pairs(ty)
            .into_iter()
            .map(|(k, v)| (k.to_owned(), json_of(v)))
            .collect(),
        _ => row
            .stored
            .iter()
            .map(|(k, v)| (k.clone(), json_of_stored(v)))
            .collect(),
    };
    api::FilterInfo {
        id: row.id.get(),
        type_id: row.type_id.clone(),
        enabled: row.enabled,
        settings,
    }
}

/// `filter.types`' answer.
pub fn api_types() -> api::FilterTypeList {
    api::FilterTypeList {
        types: rbs_filters::catalogue()
            .iter()
            .map(|t| api::FilterTypeInfo {
                type_id: t.type_id().as_str().to_owned(),
                name: t.name().to_owned(),
                fields: t
                    .fields()
                    .iter()
                    .map(|f| match f.kind {
                        FieldKind::Number {
                            min,
                            max,
                            default,
                            step,
                            unit,
                        } => api::FilterField {
                            key: f.key.to_owned(),
                            label: f.label.to_owned(),
                            kind: "number".to_owned(),
                            min: Some(min),
                            max: Some(max),
                            step: Some(step),
                            unit: (!unit.is_empty()).then(|| unit.to_owned()),
                            default: json_of(Value::Number(default)),
                        },
                        FieldKind::Color { default } => api::FilterField {
                            key: f.key.to_owned(),
                            label: f.label.to_owned(),
                            kind: "color".to_owned(),
                            min: None,
                            max: None,
                            step: None,
                            unit: None,
                            default: json_of(Value::Color(default)),
                        },
                        #[allow(unreachable_patterns)]
                        _ => api::FilterField {
                            key: f.key.to_owned(),
                            label: f.label.to_owned(),
                            kind: "unknown".to_owned(),
                            min: None,
                            max: None,
                            step: None,
                            unit: None,
                            default: serde_json::Value::Null,
                        },
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// `(key, value)` pairs from JSON settings, checked for kind only (ranges are checked
/// by `Values`).
fn pairs(
    ty: &dyn FilterType,
    settings: &BTreeMap<String, serde_json::Value>,
) -> Result<Vec<(&'static str, Value)>, Refusal> {
    settings
        .iter()
        .map(|(key, v)| {
            let field = ty.fields().iter().find(|f| f.key == key).ok_or_else(|| {
                Refusal::Invalid(format!("{} has no setting {key:?}.", ty.name()))
            })?;
            let value = match (field.kind, v) {
                (FieldKind::Number { .. }, serde_json::Value::Number(n)) => {
                    n.as_f64().map(|f| Value::Number(f as f32))
                }
                (FieldKind::Color { .. }, serde_json::Value::String(t)) => {
                    parse_color(t).map(Value::Color)
                }
                _ => None,
            }
            .ok_or_else(|| {
                Refusal::Invalid(format!("{} has the wrong kind of value.", field.label))
            })?;
            Ok((field.key, value))
        })
        .collect()
}

fn invalid(e: &rbs_filters::FilterError) -> Refusal {
    Refusal::Invalid(format!("{e}."))
}

impl Host {
    /// The source that holds `filter`, and the filter.
    fn filter_owner(&self, filter: FilterId) -> Result<(SourceId, &Filter), Refusal> {
        self.collection
            .sources()
            .find_map(|s| s.filters.iter().find(|f| f.id == filter).map(|f| (s.id, f)))
            .ok_or_else(|| Refusal::NotFound(format!("There is no filter {}.", filter.get())))
    }

    fn source_name(&self, id: SourceId) -> Result<String, Refusal> {
        self.collection
            .source(id)
            .map(|s| s.name.clone())
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))
    }

    /// Adds an enabled filter at the end of a source's chain (one undo step).
    pub(super) fn add_filter(
        &mut self,
        source: SourceId,
        type_id: &str,
        settings: &BTreeMap<String, serde_json::Value>,
    ) -> Result<FilterId, Refusal> {
        let name = self.source_name(source)?;
        let has_picture = self
            .collection
            .source(source)
            .and_then(|s| SourceKind::of(&s.type_id))
            .is_some_and(|k| !k.is_audio());
        if !has_picture {
            return Err(Refusal::Invalid(format!(
                "{name} has no picture to filter."
            )));
        }
        let ty = rbs_filters::find(type_id)
            .ok_or_else(|| Refusal::Invalid(format!("There is no filter type {type_id:?}.")))?;
        let values = Values::from_pairs(ty, pairs(ty, settings)?).map_err(|e| invalid(&e))?;
        let (id, undo) = self
            .collection
            .add_filter(source, ty.type_id(), filter_settings(ty, &values))
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Filter, &name, 0, vec![undo]);
        Ok(id)
    }

    /// Removes a filter (one undo step).
    pub(super) fn remove_filter(&mut self, filter: FilterId) -> Result<(), Refusal> {
        let (source, _) = self.filter_owner(filter)?;
        let name = self.source_name(source)?;
        let undo = self
            .collection
            .remove_filter(source, filter)
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Filter, &name, 0, vec![undo]);
        Ok(())
    }

    /// Moves a filter to `index` in its chain; `Ok(false)` when it is there already.
    pub(super) fn move_filter(&mut self, filter: FilterId, index: usize) -> Result<bool, Refusal> {
        let (source, _) = self.filter_owner(filter)?;
        let name = self.source_name(source)?;
        let chain = &self
            .collection
            .source(source)
            .map(|s| s.filters.clone())
            .unwrap_or_default();
        if index >= chain.len() {
            return Err(Refusal::Invalid(format!(
                "The chain has {} filters; position {index} is past its end.",
                chain.len()
            )));
        }
        if chain.get(index).is_some_and(|f| f.id == filter) {
            return Ok(false);
        }
        let undo = self
            .collection
            .move_filter(source, filter, index)
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Filter, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Turns a filter on or off; `Ok(false)` when it already is.
    pub(super) fn set_filter_enabled(
        &mut self,
        filter: FilterId,
        enabled: bool,
    ) -> Result<bool, Refusal> {
        let (source, f) = self.filter_owner(filter)?;
        if f.enabled == enabled {
            return Ok(false);
        }
        let name = self.source_name(source)?;
        let undo = self
            .collection
            .set_filter_enabled(source, filter, enabled)
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Filter, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Patches a filter's settings on top of the values on air (ADR-0062 decision 8):
    /// refused only for what the patch sends; the full set is written. `Ok(false)` when
    /// nothing changes and the stored settings needed no repair.
    pub(super) fn patch_filter(
        &mut self,
        filter: FilterId,
        patch: &BTreeMap<String, serde_json::Value>,
    ) -> Result<bool, Refusal> {
        let (_, f) = self.filter_owner(filter)?;
        let ty = rbs_filters::find(f.type_id.as_str()).ok_or_else(|| {
            Refusal::Invalid(format!(
                "This version has no filter type {:?}; it can only be moved, turned on or off, or removed.",
                f.type_id.as_str()
            ))
        })?;
        let (on_air, _) = filter_values_lenient(ty, &f.settings);
        let values = on_air
            .with(ty, pairs(ty, patch)?)
            .map_err(|e| invalid(&e))?;
        self.set_filter_values(filter, values)
    }

    /// Writes a filter's full validated values (a slider release, a typed value, Reset
    /// to defaults); one undo step. `Ok(false)` when the values on air already are
    /// these and the stored settings need no repair.
    pub(super) fn set_filter_values(
        &mut self,
        filter: FilterId,
        values: Values,
    ) -> Result<bool, Refusal> {
        let (source, f) = self.filter_owner(filter)?;
        let ty = rbs_filters::find(f.type_id.as_str()).ok_or_else(|| {
            Refusal::Invalid("This filter is not available in this version.".to_owned())
        })?;
        let (on_air, adjusted) = filter_values_lenient(ty, &f.settings);
        let settings = filter_settings(ty, &values);
        if on_air == values && adjusted.is_empty() && settings == f.settings {
            return Ok(false);
        }
        let name = self.source_name(source)?;
        let undo = self
            .collection
            .set_filter_settings(source, filter, settings)
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Filter, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Sets the sync offset of a source's sound in milliseconds: a live control, not
    /// an undo step (ADR-0062 decision 10), sent to the running input at once.
    /// `Ok(false)` when it already is.
    pub(super) fn set_source_sync_offset(
        &mut self,
        source: SourceId,
        offset_ms: i32,
    ) -> Result<bool, Refusal> {
        let s = self
            .collection
            .source(source)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", source.get())))?;
        let sound = SourceKind::of(&s.type_id).is_some_and(SourceKind::is_audio);
        if !sound {
            return Err(Refusal::Invalid(format!(
                "{} has no sound to delay.",
                s.name
            )));
        }
        if s.audio.sync_offset_ms == offset_ms {
            return Ok(false);
        }
        self.collection
            .set_source_sync_offset(source, offset_ms)
            .map_err(|e| Refusal::of(&e))?;
        self.studio
            .set_audio_offset(source, i64::from(offset_ms) * 1_000_000);
        Ok(true)
    }
}
