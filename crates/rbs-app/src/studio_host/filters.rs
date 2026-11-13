//! Video filters and the sync offset in the host (ADR-0062): the rows the UI and the
//! control API show, and the edits both make.

use std::collections::BTreeMap;
use std::sync::Arc;

use rbs_api::request::FilterDomains;
use rbs_api::types as api;
use rbs_audio_filters::AudioFilterType;
use rbs_filters::{FilterType, Values};
use rbs_graph::node::NodeTypeId;
use rbs_params::{Curve, Field, FieldKind, Schema, Value, color_text, parse_color};
use rbs_scene::SceneCollection;
use rbs_scene::model::{Filter, FilterId, Settings, Source, SourceId};
use rbs_sources::{filter_settings, filter_values, filter_values_lenient};

use super::{Host, Refusal, SourceKind};
use crate::edit_history::EditKind;

/// A built-in filter type of either domain (ADR-0082): what the host edits, shows and
/// lists without caring whether the filter works on pictures or on sound.
#[derive(Clone, Copy)]
pub enum AnyFilter {
    /// A video filter.
    Video(&'static dyn FilterType),
    /// An audio filter.
    Audio(&'static dyn AudioFilterType),
}

impl AnyFilter {
    /// The built-in type with this id, from either catalogue.
    pub fn find(type_id: &str) -> Option<Self> {
        rbs_filters::find(type_id)
            .map(Self::Video)
            .or_else(|| rbs_audio_filters::find(type_id).map(Self::Audio))
    }

    /// The type id.
    pub fn type_id(self) -> NodeTypeId {
        match self {
            Self::Video(t) => t.type_id(),
            Self::Audio(t) => t.type_id(),
        }
    }

    /// The display name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Video(t) => t.name(),
            Self::Audio(t) => t.name(),
        }
    }

    /// Works on sound.
    pub fn is_audio(self) -> bool {
        matches!(self, Self::Audio(_))
    }
}

impl Schema for AnyFilter {
    fn fields(&self) -> &'static [Field] {
        match self {
            Self::Video(t) => FilterType::fields(*t),
            Self::Audio(t) => AudioFilterType::fields(*t),
        }
    }
}

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
    /// The live meter of a running audio filter that has one (a noise gate); read it
    /// only while an editor shows it.
    pub meter: Option<MeterHandle>,
}

/// A filter meter, equal to another only when it is the same meter (a row changes
/// when its filter's graph node is replaced).
#[derive(Debug, Clone)]
pub struct MeterHandle(pub Arc<rbs_audio_filters::FilterMeter>);

impl PartialEq for MeterHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl FilterRow {
    /// The built-in video type, if this build has it (`None` for an audio filter).
    pub fn ty(&self) -> Option<&'static dyn FilterType> {
        rbs_filters::find(&self.type_id)
    }

    /// The built-in type of either domain, if this build has it.
    pub fn any(&self) -> Option<AnyFilter> {
        AnyFilter::find(&self.type_id)
    }
}

/// The rows of a source's filters, in chain order.
pub fn rows(
    source: &Source,
    meter_of: impl Fn(FilterId) -> Option<Arc<rbs_audio_filters::FilterMeter>>,
) -> Vec<FilterRow> {
    let mut seen: BTreeMap<&str, u32> = BTreeMap::new();
    source
        .filters
        .iter()
        .map(|f| {
            let ty = AnyFilter::find(f.type_id.as_str());
            let base = ty.map_or_else(|| f.type_id.as_str().to_owned(), |t| t.name().to_owned());
            let n = seen.entry(f.type_id.as_str()).or_insert(0);
            *n += 1;
            let name = if *n == 1 { base } else { format!("{base} {n}") };
            FilterRow {
                id: f.id,
                type_id: f.type_id.as_str().to_owned(),
                name,
                enabled: f.enabled,
                values: ty.map(|t| filter_values_lenient(&t, &f.settings).0),
                stored: f.settings.clone(),
                meter: meter_of(f.id).map(MeterHandle),
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
        Value::Toggle(b) => serde_json::Value::Bool(b),
        Value::Number(n) => format!("{n}")
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Value::Color(c) => serde_json::Value::String(color_text(c)),
        Value::Source(id) => id.map_or(serde_json::Value::Null, serde_json::Value::from),
        // A choice needs its field to name the option: see `json_of_field`.
        #[allow(unreachable_patterns)]
        _ => serde_json::Value::Null,
    }
}

/// `v` as JSON for `field`: a choice is its option's key.
fn json_of_field(field: &Field, v: Value) -> serde_json::Value {
    match (field.kind, v) {
        (FieldKind::Choice { options, .. }, Value::Choice(i)) => options
            .get(usize::from(i))
            .map_or(serde_json::Value::Null, |(key, _)| {
                serde_json::Value::String((*key).to_owned())
            }),
        _ => json_of(v),
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
    let settings = match (row.any(), &row.values) {
        (Some(ty), Some(values)) => values
            .to_pairs(&ty)
            .into_iter()
            .zip(ty.fields())
            .map(|((k, v), field)| (k.to_owned(), json_of_field(field, v)))
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

/// One field of a type as the API lists it.
fn api_field(f: &Field) -> api::FilterField {
    let mut field = api::FilterField {
        key: f.key.to_owned(),
        label: f.label.to_owned(),
        kind: "unknown".to_owned(),
        help: (!f.help.is_empty()).then(|| f.help.to_owned()),
        curve: None,
        options: None,
        min: None,
        max: None,
        step: None,
        unit: None,
        default: serde_json::Value::Null,
    };
    match f.kind {
        FieldKind::Number {
            min,
            max,
            default,
            step,
            unit,
            curve,
            ..
        } => {
            field.kind = "number".to_owned();
            field.min = Some(min);
            field.max = Some(max);
            field.step = Some(step);
            field.unit = (!unit.is_empty()).then(|| unit.to_owned());
            field.curve = Some(
                match curve {
                    Curve::Log => "log",
                    _ => "linear",
                }
                .to_owned(),
            );
            field.default = json_of(Value::Number(default));
        }
        FieldKind::Color { default } => {
            field.kind = "color".to_owned();
            field.default = json_of(Value::Color(default));
        }
        FieldKind::Toggle { default } => {
            field.kind = "toggle".to_owned();
            field.default = json_of(Value::Toggle(default));
        }
        FieldKind::Source => {
            field.kind = "source".to_owned();
            field.default = json_of(Value::Source(None));
        }
        FieldKind::Choice { options, .. } => {
            field.kind = "choice".to_owned();
            field.options = Some(
                options
                    .iter()
                    .map(|(key, label)| api::FilterOption {
                        key: (*key).to_owned(),
                        label: (*label).to_owned(),
                    })
                    .collect(),
            );
            field.default = json_of_field(f, f.kind.default_value());
        }
        #[allow(unreachable_patterns)]
        _ => {}
    }
    field
}

/// `filter.types`' answer for `domains`: video types first, then audio types.
pub fn api_types(domains: FilterDomains) -> api::FilterTypeList {
    let video = matches!(domains, FilterDomains::Video | FilterDomains::All)
        .then(|| {
            rbs_filters::catalogue()
                .iter()
                .map(|t| AnyFilter::Video(*t))
        })
        .into_iter()
        .flatten();
    let audio = matches!(domains, FilterDomains::Audio | FilterDomains::All)
        .then(|| {
            rbs_audio_filters::catalogue()
                .iter()
                .map(|t| AnyFilter::Audio(*t))
        })
        .into_iter()
        .flatten();
    api::FilterTypeList {
        types: video
            .chain(audio)
            .map(|t| api::FilterTypeInfo {
                type_id: t.type_id().as_str().to_owned(),
                domain: if t.is_audio() { "audio" } else { "video" }.to_owned(),
                name: t.name().to_owned(),
                fields: t.fields().iter().map(api_field).collect(),
            })
            .collect(),
    }
}

/// `(key, value)` pairs from JSON settings, checked for kind only (ranges are checked
/// by `Values`).
fn pairs(
    ty: AnyFilter,
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
                (FieldKind::Toggle { .. }, serde_json::Value::Bool(b)) => Some(Value::Toggle(*b)),
                (FieldKind::Source, serde_json::Value::Null) => Some(Value::Source(None)),
                (FieldKind::Source, serde_json::Value::Number(n)) => n
                    .as_u64()
                    .and_then(|id| u32::try_from(id).ok())
                    .map(|id| Value::Source(Some(id))),
                (FieldKind::Choice { options, .. }, serde_json::Value::String(t)) => options
                    .iter()
                    .position(|(key, _)| key == t)
                    .and_then(|i| u8::try_from(i).ok())
                    .map(Value::Choice),
                _ => None,
            }
            .ok_or_else(|| {
                Refusal::Invalid(format!("{} has the wrong kind of value.", field.label))
            })?;
            Ok((field.key, value))
        })
        .collect()
}

fn invalid(e: &rbs_filters::ParamError) -> Refusal {
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

    /// Refuses a key source (a `Source` field, ADR-0095) that is the filter's own source,
    /// does not exist or has a picture: only sound sources can be a key.
    fn check_key_sources(
        &self,
        owner: SourceId,
        ty: &AnyFilter,
        values: &Values,
    ) -> Result<(), Refusal> {
        for (i, field) in ty.fields().iter().enumerate() {
            if !matches!(field.kind, FieldKind::Source) {
                continue;
            }
            let Some(id) = values.source(i) else { continue };
            let key = SourceId::from_raw(id);
            if key == owner {
                return Err(Refusal::Invalid(format!(
                    "{} cannot use its own sound as its {}.",
                    self.source_name(owner)?,
                    field.label.to_lowercase()
                )));
            }
            let source = self.collection.source(key).ok_or_else(|| {
                Refusal::NotFound(format!(
                    "There is no source {id} for the {}.",
                    field.label.to_lowercase()
                ))
            })?;
            if !SourceKind::of(&source.type_id).is_some_and(SourceKind::is_audio) {
                return Err(Refusal::Invalid(format!(
                    "{} has a picture; only a sound source can be the {}.",
                    source.name,
                    field.label.to_lowercase()
                )));
            }
        }
        Ok(())
    }

    /// Adds an enabled filter at the end of a source's chain (one undo step).
    pub(super) fn add_filter(
        &mut self,
        source: SourceId,
        type_id: &str,
        settings: &BTreeMap<String, serde_json::Value>,
    ) -> Result<FilterId, Refusal> {
        let name = self.source_name(source)?;
        let ty = AnyFilter::find(type_id)
            .ok_or_else(|| Refusal::Invalid(format!("There is no filter type {type_id:?}.")))?;
        let has_sound = self
            .collection
            .source(source)
            .and_then(|s| SourceKind::of(&s.type_id))
            .is_some_and(SourceKind::is_audio);
        // A source with a picture takes video filters, one with only sound takes audio
        // filters (ADR-0082); the wrong domain is refused in both directions.
        if has_sound != ty.is_audio() {
            return Err(Refusal::Invalid(if has_sound {
                format!("{name} has only sound; {} is a video filter.", ty.name())
            } else {
                format!("{name} has a picture; {} is an audio filter.", ty.name())
            }));
        }
        let values = Values::from_pairs(&ty, pairs(ty, settings)?).map_err(|e| invalid(&e))?;
        self.check_key_sources(source, &ty, &values)?;
        let (id, undo) = self
            .collection
            .add_filter(source, ty.type_id(), filter_settings(&ty, &values))
            .map_err(|e| Refusal::of(&e))?;
        self.history.record(EditKind::Filter, &name, 0, vec![undo]);
        Ok(id)
    }

    /// Applies a built-in look (ADR-0075) to a source and its item on the studio's
    /// canvas as one undo step: the item's style when the look has one, and each filter
    /// of the look replacing the source's filter of that type (set enabled with the
    /// look's values) or added at the end of the chain. `Ok(false)` when nothing
    /// changes.
    pub(super) fn apply_look(&mut self, source: SourceId, look: &str) -> Result<bool, Refusal> {
        let look = rbs_scene::look::find(look)
            .ok_or_else(|| Refusal::NotFound(format!("There is no look {look:?}.")))?;
        let name = self.source_name(source)?;
        let has_picture = self
            .collection
            .source(source)
            .and_then(|s| SourceKind::of(&s.type_id))
            .is_none_or(|k| !k.is_audio());
        if !has_picture {
            return Err(Refusal::Invalid(format!(
                "{name} has no picture to change."
            )));
        }
        // Everything is checked before anything is changed.
        let mut filters = Vec::new();
        for f in &look.filters {
            let ty = rbs_filters::find(f.type_id).ok_or_else(|| {
                Refusal::Invalid(format!("There is no filter type {:?}.", f.type_id))
            })?;
            let values = filter_values(ty, &f.settings).map_err(|e| invalid(&e))?;
            filters.push((ty, filter_settings(ty, &values)));
        }
        let mut records = Vec::new();
        let result = self.apply_look_steps(source, &look, filters, &mut records);
        match result {
            Ok(()) if records.is_empty() => Ok(false),
            Ok(()) => {
                self.history.record(EditKind::Look, &name, 0, records);
                Ok(true)
            }
            Err(e) => {
                self.roll_back(records);
                Err(e)
            }
        }
    }

    fn apply_look_steps(
        &mut self,
        source: SourceId,
        look: &rbs_scene::look::Look,
        filters: Vec<(&'static dyn FilterType, Settings)>,
        records: &mut Vec<rbs_scene::Undo>,
    ) -> Result<(), Refusal> {
        if let Some(style) = look.style {
            let (_, item) = self.item_of(source).map_err(Refusal::NotFound)?;
            let (item_id, mut layout) = (item.id, self.shown_layout(item));
            let own = item
                .overrides
                .contains_key(&self.canvas)
                .then_some(self.canvas);
            if layout.style != style {
                layout.style = style;
                let undo = self
                    .save_live_layout(item_id, own, layout)
                    .map_err(|e| Refusal::of(&e))?;
                records.push(undo);
            }
        }
        for (ty, settings) in filters {
            let existing = self.collection.source(source).and_then(|s| {
                s.filters
                    .iter()
                    .find(|f| f.type_id == ty.type_id())
                    .map(|f| (f.id, f.enabled, f.settings.clone()))
            });
            match existing {
                Some((id, enabled, old)) => {
                    if old != settings {
                        let undo = self
                            .collection
                            .set_filter_settings(source, id, settings)
                            .map_err(|e| Refusal::of(&e))?;
                        records.push(undo);
                    }
                    if !enabled {
                        let undo = self
                            .collection
                            .set_filter_enabled(source, id, true)
                            .map_err(|e| Refusal::of(&e))?;
                        records.push(undo);
                    }
                }
                None => {
                    let (_, undo) = self
                        .collection
                        .add_filter(source, ty.type_id(), settings)
                        .map_err(|e| Refusal::of(&e))?;
                    records.push(undo);
                }
            }
        }
        Ok(())
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
        let ty = AnyFilter::find(f.type_id.as_str()).ok_or_else(|| {
            Refusal::Invalid(format!(
                "This version has no filter type {:?}; it can only be moved, turned on or off, or removed.",
                f.type_id.as_str()
            ))
        })?;
        let (on_air, _) = filter_values_lenient(&ty, &f.settings);
        let values = on_air
            .with(&ty, pairs(ty, patch)?)
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
        let ty = AnyFilter::find(f.type_id.as_str()).ok_or_else(|| {
            Refusal::Invalid("This filter is not available in this version.".to_owned())
        })?;
        self.check_key_sources(source, &ty, &values)?;
        let (on_air, adjusted) = filter_values_lenient(&ty, &f.settings);
        let settings = filter_settings(&ty, &values);
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
