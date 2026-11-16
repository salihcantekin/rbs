//! Duplicate source, Duplicate scene with new sources, Reset source and Copy / Paste
//! attributes (ADR-0133): host compositions of ordinary collection edits, checked and
//! applied all or nothing as one undo step, plus the live audio writes after it.

use rbs_api::types::OutputCanvas;
use rbs_graph::node::NodeTypeId;
use rbs_params::{FieldKind, Schema};
use rbs_scene::model::{
    Crop, ItemId, ItemStyle, Layout, SceneId, SettingValue, Settings, Shape, Source, SourceAudio,
    SourceId, SourceMonitor,
};
use rbs_scene::{AttributeGroup, SourceCopy, Undo, recrop_keeping, visible_size};

use super::filters::AnyFilter;
use super::{CAMERA_SIZE, Host, Refusal, SourceKind, unique_scene_name, whole_fps};
use crate::edit_history::EditKind;

/// What Copy attributes took: every group's value of one item and its source, at copy
/// time (ADR-0133 decision 6). Later edits of the original do not change it.
#[derive(Debug, Clone)]
pub(super) struct AttributeSnapshot {
    /// The source copied.
    pub source: SourceId,
    /// Its state when copied.
    pub state: Source,
    /// Its item's layout on the copy call's canvas, in that canvas's pixels.
    pub layout: Option<Layout>,
    /// Its picture size when known.
    pub picture: Option<(u32, u32)>,
    /// Groups whose copied value is not the default.
    pub groups: Vec<AttributeGroup>,
}

/// How pasted filters meet the target's own (ADR-0133 decision 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilterPaste {
    /// Replace the target's filters of that kind.
    #[default]
    Replace,
    /// Add after them.
    Append,
}

/// Why a group was not written for a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// The group does not apply to the target.
    NotApplicable,
    /// It already has that value.
    Unchanged,
    /// The item is locked.
    Locked,
    /// The picture size is not known in this session.
    NoPictureSize,
    /// The region does not fit the target's picture.
    OutOfPicture,
}

impl Skip {
    /// The reason as the control API names it.
    pub fn id(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::Unchanged => "unchanged",
            Self::Locked => "locked",
            Self::NoPictureSize => "no_picture_size",
            Self::OutOfPicture => "out_of_picture",
        }
    }
}

/// What a reset or paste did to one target.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetReport {
    /// The source.
    pub source: SourceId,
    /// The item used, if any.
    pub item: Option<ItemId>,
    /// Groups written in the undo step.
    pub changed: Vec<AttributeGroup>,
    /// Live groups written outside it.
    pub live: Vec<AttributeGroup>,
    /// Groups not written, and why.
    pub skipped: Vec<(AttributeGroup, Skip)>,
}

/// What Duplicate source made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Duplicate {
    /// The copy.
    pub source: SourceId,
    /// Its item.
    pub item: ItemId,
    /// It starts active (false for a camera copy in standby).
    pub active: bool,
    /// Copies of its companions, `(source, item)`.
    pub companions: Vec<(SourceId, ItemId)>,
}

/// The order groups are written in: crop before transform (ADR-0133 decision 1).
const WRITE_ORDER: [AttributeGroup; 12] = [
    AttributeGroup::Crop,
    AttributeGroup::Transform,
    AttributeGroup::Opacity,
    AttributeGroup::Style,
    AttributeGroup::VideoFilters,
    AttributeGroup::AudioFilters,
    AttributeGroup::Options,
    AttributeGroup::Region,
    AttributeGroup::Glide,
    AttributeGroup::RecordSeparately,
    AttributeGroup::Sound,
    AttributeGroup::SyncOffset,
];

/// Whether a source type shows a picture and has sound.
fn kinds(type_id: &NodeTypeId) -> (bool, bool) {
    match SourceKind::of(type_id) {
        Some(SourceKind::Media) => (true, true),
        Some(k) if k.is_audio() => (false, true),
        Some(_) => (true, false),
        // Plugin types: treated as pictures (ADR-0133 decision 1, `SourceKinds`).
        None => (true, false),
    }
}

/// True for a filter that works on sound.
fn is_audio_filter(type_id: &NodeTypeId) -> bool {
    AnyFilter::find(type_id.as_str()).is_some_and(AnyFilter::is_audio)
}

/// The filter setting keys that name a source (`FieldKind::Source`, ADR-0095).
fn source_fields(type_id: &NodeTypeId) -> Vec<String> {
    AnyFilter::find(type_id.as_str())
        .map(|f| {
            f.fields()
                .iter()
                .filter(|field| matches!(field.kind, FieldKind::Source))
                .map(|field| field.key.to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// A free source name from `base`, avoiding `also` (names chosen in the same step).
fn free_name(host: &Host, base: &str, also: &[String]) -> String {
    let taken = |n: &str| host.collection.sources().any(|s| s.name == n) || also.iter().any(|a| a == n);
    let base = base.trim();
    (2..)
        .map(|i| format!("{base} {i}"))
        .find(|n| !taken(n))
        .unwrap_or_else(|| base.to_owned())
}

/// The keys of `settings` a group writes for this source: its option keys, without
/// `exclude_own_process` on a companion (ADR-0133 decision 2).
fn writable_options(source: &Source) -> Vec<&'static str> {
    rbs_sources::option_keys(&source.type_id)
        .iter()
        .copied()
        .filter(|k| {
            !(source.companion_of().is_some() && *k == rbs_sources::types::keys::EXCLUDE_OWN_PROCESS)
        })
        .collect()
}

fn default_style() -> ItemStyle {
    ItemStyle::new(Shape::Rectangle { radius: 0.0 })
}

impl TargetReport {
    fn new(source: SourceId, item: Option<ItemId>) -> Self {
        Self {
            source,
            item,
            changed: Vec::new(),
            live: Vec::new(),
            skipped: Vec::new(),
        }
    }
}

impl Host {
    // ----- duplicate -----

    /// Duplicate source (ADR-0133 decision 3): an independent copy above the source's
    /// item in the scene being edited (or its global item), with its companion copied
    /// muted. A camera copy starts in standby. One undo step.
    pub(super) fn duplicate_source(&mut self, id: SourceId) -> Result<Duplicate, Refusal> {
        let (_, item) = self.item_of(id).map_err(Refusal::NotFound)?;
        let item = item.id;
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?
            .clone();
        let camera = SourceKind::of(&source.type_id) == Some(SourceKind::Camera);
        let standby = source.standby() || camera;
        let mut names = vec![free_name(self, &source.name, &[])];
        let mut copies = vec![SourceCopy::new(id, &names[0], standby)];
        if source.companion_of().is_none() {
            let companions: Vec<Source> = self
                .collection
                .sources()
                .filter(|s| s.companion_of() == Some(id))
                .cloned()
                .collect();
            for c in companions {
                let name = free_name(self, &c.name, &names);
                copies.push(SourceCopy::new(c.id, &name, standby).with_muted(true));
                names.push(name);
            }
        }
        let (made, undo) = self
            .collection
            .duplicate_source(item, &copies, &source_fields)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::Duplicate, &source.name, 0, vec![undo]);
        let mut items = made.items.into_iter();
        let (copy, copy_item) = items
            .next()
            .ok_or_else(|| Refusal::Failed("the copy was not placed".into()))?;
        Ok(Duplicate {
            source: copy,
            item: copy_item,
            active: !standby,
            companions: items.collect(),
        })
    }

    /// Duplicate scene with new sources (ADR-0133 decision 4): the scene copied with
    /// copies of the sources placed directly in it and their companions (global sources
    /// and nested scenes stay references). One undo step.
    pub(super) fn duplicate_scene_with_copies(
        &mut self,
        id: SceneId,
    ) -> Result<(SceneId, Vec<(SourceId, SourceId)>), Refusal> {
        let scene = self
            .collection
            .scene(id)
            .filter(|_| self.listed(id))
            .ok_or_else(|| Refusal::NotFound(format!("There is no scene {}.", id.get())))?
            .clone();
        let mut set: Vec<SourceId> = Vec::new();
        for it in &scene.items {
            if let rbs_scene::model::ItemContent::Source(s) = it.content
                && !set.contains(&s)
            {
                set.push(s);
            }
        }
        let companions: Vec<SourceId> = self
            .collection
            .sources()
            .filter(|s| s.companion_of().is_some_and(|p| set.contains(&p)))
            .map(|s| s.id)
            .collect();
        for c in companions {
            if !set.contains(&c) {
                set.push(c);
            }
        }
        let mut names = Vec::new();
        let mut copies = Vec::new();
        for s in set {
            let Some(src) = self.collection.source(s) else {
                continue;
            };
            let picture_standby = |src: &Source| {
                src.standby() || SourceKind::of(&src.type_id) == Some(SourceKind::Camera)
            };
            let standby = match src.companion_of().and_then(|p| self.collection.source(p)) {
                Some(p) => picture_standby(p),
                None => picture_standby(src),
            };
            let name = free_name(self, &src.name, &names);
            copies.push(SourceCopy::new(s, &name, standby));
            names.push(name);
        }
        let name = unique_scene_name(&self.collection, &scene.name);
        let (new_id, made, undo) = self
            .collection
            .duplicate_scene_with_copies(id, &name, &copies, &source_fields)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::AddScene, &name, 0, vec![undo]);
        Ok((new_id, made.sources))
    }

    // ----- option defaults -----

    /// The option values a new source of `type_id` gets on the studio's canvas
    /// (ADR-0133 decision 2); keys left out mean the type's own default. The Add source
    /// path uses the same values.
    pub(super) fn option_defaults(&self, type_id: &NodeTypeId) -> Settings {
        use rbs_sources::types::keys;
        let mut settings = Settings::new();
        let Some(canvas) = self.collection.canvas(self.canvas) else {
            return settings;
        };
        match SourceKind::of(type_id) {
            Some(SourceKind::Screen | SourceKind::Window) => {
                settings.insert(
                    keys::FPS.into(),
                    SettingValue::Int(whole_fps(canvas.frame_rate)),
                );
            }
            Some(SourceKind::Camera) => {
                let (w, h, fps) = CAMERA_SIZE;
                settings.insert(keys::WIDTH.into(), SettingValue::Int(w));
                settings.insert(keys::HEIGHT.into(), SettingValue::Int(h));
                settings.insert(keys::FPS.into(), SettingValue::Int(fps));
            }
            Some(SourceKind::Browser) => {
                let (w, h) = browser_size(canvas.size.width, canvas.size.height);
                settings.insert(keys::WIDTH.into(), SettingValue::Int(w));
                settings.insert(keys::HEIGHT.into(), SettingValue::Int(h));
            }
            _ => {}
        }
        settings
    }

    // ----- copy -----

    /// Copy attributes (ADR-0133 decision 6): takes the snapshot of a source and its
    /// item on `on`. Replaces any earlier snapshot.
    pub(super) fn copy_attributes(
        &mut self,
        id: SourceId,
        on: OutputCanvas,
    ) -> Result<AttributeSnapshot, Refusal> {
        let (_, item) = self.item_of(id).map_err(Refusal::NotFound)?;
        let item = item.id;
        let state = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?
            .clone();
        let layout = self.crop_target_on(item, on).ok().map(|(_, l, _)| l);
        let picture = self.sizes.get(&id).copied();
        let mut snapshot = AttributeSnapshot {
            source: id,
            state,
            layout,
            picture,
            groups: Vec::new(),
        };
        snapshot.groups = AttributeGroup::ALL
            .into_iter()
            .filter(|g| self.snapshot_differs(&snapshot, *g))
            .collect();
        self.attribute_clipboard = Some(snapshot.clone());
        Ok(snapshot)
    }

    /// True when the snapshot holds a value other than the default for `group`.
    fn snapshot_differs(&self, s: &AttributeSnapshot, group: AttributeGroup) -> bool {
        let (picture, sound) = kinds(&s.state.type_id);
        let audio_filters = |audio: bool| {
            s.state
                .filters
                .iter()
                .any(|f| is_audio_filter(&f.type_id) == audio)
        };
        match group {
            AttributeGroup::Transform => picture && s.layout.is_some(),
            AttributeGroup::Crop => s.layout.is_some_and(|l| l.crop != Crop::new(0, 0, 0, 0)),
            AttributeGroup::Opacity => s.layout.is_some_and(|l| l.opacity != 1.0),
            AttributeGroup::Style => s.layout.is_some_and(|l| l.style != default_style()),
            AttributeGroup::VideoFilters => audio_filters(false),
            AttributeGroup::AudioFilters => audio_filters(true),
            AttributeGroup::Options => {
                let defaults = self.option_defaults(&s.state.type_id);
                writable_options(&s.state)
                    .iter()
                    .any(|k| s.state.settings.get(*k) != defaults.get(*k))
            }
            AttributeGroup::Region => s.state.region.is_some() || s.state.chrome.is_some(),
            AttributeGroup::Glide => s.state.glide.is_some(),
            AttributeGroup::RecordSeparately => {
                s.state.record_separately != rbs_scene::model::RecordSeparately::default()
            }
            AttributeGroup::Sound => {
                let a = s.state.audio;
                sound
                    && (a.volume != SourceAudio::default().volume
                        || a.muted
                        || a.monitor != SourceMonitor::Off)
            }
            AttributeGroup::SyncOffset => sound && s.state.audio.sync_offset_ms != 0,
        }
    }

    // ----- reset and paste -----

    /// Reset source (ADR-0133 decision 5): `groups` back to their defaults for a source
    /// and its item on `on`. One undo step (none when nothing changes), then the live
    /// groups.
    pub(super) fn reset_source(
        &mut self,
        id: SourceId,
        groups: &[AttributeGroup],
        on: OutputCanvas,
    ) -> Result<TargetReport, Refusal> {
        let name = self.name_of(id);
        self.collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let item = self.item_of(id).ok().map(|(_, i)| i.id);
        let mut records = Vec::new();
        let mut report = TargetReport::new(id, item);
        let mut live = Vec::new();
        let done = self.write_groups(id, item, groups, None, FilterPaste::Replace, on, &mut records, &mut report, &mut live);
        self.finish(done, EditKind::Reset, &name, records, live, &mut report)?;
        Ok(report)
    }

    /// Paste attributes (ADR-0133 decision 6): the chosen groups of the snapshot onto
    /// every item, one undo step for all of them, then the live groups. A refusal of
    /// any target rolls every target back.
    pub(super) fn paste_attributes(
        &mut self,
        items: &[ItemId],
        groups: &[AttributeGroup],
        filters: FilterPaste,
        on: OutputCanvas,
    ) -> Result<Vec<TargetReport>, Refusal> {
        let snapshot = self
            .attribute_clipboard
            .clone()
            .ok_or_else(|| Refusal::Conflict("Copy a source's attributes first.".into()))?;
        let mut targets = Vec::with_capacity(items.len());
        for item in items {
            match self.collection.item(*item).map(|(_, i)| i.content) {
                Some(rbs_scene::model::ItemContent::Source(s)) => targets.push((*item, s)),
                Some(_) => {
                    return Err(Refusal::Invalid(format!(
                        "Item {} places a scene, not a source.",
                        item.get()
                    )));
                }
                None => return Err(Refusal::NotFound(format!("There is no item {}.", item.get()))),
            }
        }
        let mut records = Vec::new();
        let mut reports = Vec::new();
        let mut live = Vec::new();
        let mut sources_done: Vec<SourceId> = Vec::new();
        let mut failed = None;
        for (item, source) in targets {
            let mut report = TargetReport::new(source, Some(item));
            // A source group named by two items of the same source is applied once.
            let chosen: Vec<AttributeGroup> = groups
                .iter()
                .copied()
                .filter(|g| g.on_item() || !sources_done.contains(&source))
                .collect();
            sources_done.push(source);
            if let Err(e) = self.write_groups(
                source,
                Some(item),
                &chosen,
                Some(&snapshot),
                filters,
                on,
                &mut records,
                &mut report,
                &mut live,
            ) {
                failed = Some(e);
                break;
            }
            reports.push(report);
        }
        let name = snapshot.state.name.clone();
        let mut summary = TargetReport::new(snapshot.source, None);
        let done = failed.map_or(Ok(()), Err);
        self.finish(done, EditKind::PasteAttributes, &name, records, live, &mut summary)?;
        // The live writes are reported per target.
        for r in &mut reports {
            r.live.retain(|g| summary.live.contains(g));
        }
        Ok(reports)
    }

    /// Records the step (or rolls it back), then writes the live groups.
    fn finish(
        &mut self,
        done: Result<(), Refusal>,
        kind: EditKind,
        name: &str,
        records: Vec<Undo>,
        live: Vec<LiveWrite>,
        report: &mut TargetReport,
    ) -> Result<(), Refusal> {
        if let Err(e) = done {
            self.roll_back(records);
            return Err(e);
        }
        if !records.is_empty() {
            self.history.record(kind, name, 0, records);
        }
        for w in live {
            let group = w.group();
            if self.write_live(w)? && !report.live.contains(&group) {
                report.live.push(group);
            }
        }
        Ok(())
    }

    fn write_live(&mut self, w: LiveWrite) -> Result<bool, Refusal> {
        match w {
            LiveWrite::Sound(id, audio) => {
                let a = self.set_source_audio(id, Some(audio.volume), Some(audio.muted))?;
                let b = self.set_source_monitor(id, audio.monitor)?;
                Ok(a || b)
            }
            LiveWrite::SyncOffset(id, ms) => self.set_source_sync_offset(id, ms),
        }
    }

    /// Writes each chosen group for one target, from `snapshot` or the defaults.
    #[allow(clippy::too_many_arguments)]
    fn write_groups(
        &mut self,
        id: SourceId,
        item: Option<ItemId>,
        groups: &[AttributeGroup],
        snapshot: Option<&AttributeSnapshot>,
        filters: FilterPaste,
        on: OutputCanvas,
        records: &mut Vec<Undo>,
        report: &mut TargetReport,
        live: &mut Vec<LiveWrite>,
    ) -> Result<(), Refusal> {
        for group in WRITE_ORDER.into_iter().filter(|g| groups.contains(g)) {
            let outcome = if group.on_item() {
                self.write_item_group(id, item, group, snapshot, on, records)?
            } else {
                self.write_source_group(id, group, snapshot, filters, records, live)?
            };
            match outcome {
                Ok(()) if !group.is_live() => report.changed.push(group),
                Ok(()) => {}
                Err(skip) => report.skipped.push((group, skip)),
            }
        }
        Ok(())
    }

    /// One item group. `Ok(Err(skip))` when it is not written.
    fn write_item_group(
        &mut self,
        id: SourceId,
        item: Option<ItemId>,
        group: AttributeGroup,
        snapshot: Option<&AttributeSnapshot>,
        on: OutputCanvas,
        records: &mut Vec<Undo>,
    ) -> Result<Result<(), Skip>, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let (picture, _) = kinds(&source.type_id);
        let region = source.region;
        let Some(item) = item.filter(|_| picture) else {
            return Ok(Err(Skip::NotApplicable));
        };
        if self.collection.item(item).is_some_and(|(_, i)| i.locked) {
            return Ok(Err(Skip::Locked));
        }
        let Ok((_, old, own)) = self.crop_target_on(item, on) else {
            return Ok(Err(Skip::NotApplicable));
        };
        let from = match snapshot {
            Some(s) => match s.layout {
                Some(l) if kinds(&s.state.type_id).0 => Some(l),
                _ => return Ok(Err(Skip::NotApplicable)),
            },
            None => None,
        };
        let size = self.sizes.get(&id).copied();
        let visible = |crop: Crop| size.and_then(|p| visible_size(p, region, crop));
        let new = match group {
            AttributeGroup::Crop => {
                let crop = match (from, snapshot) {
                    (None, _) => Crop::new(0, 0, 0, 0),
                    (Some(l), Some(s)) => match (s.picture, size) {
                        (Some(sp), Some(tp)) if sp != tp => scale_crop(l.crop, sp, tp),
                        _ => l.crop,
                    },
                    (Some(l), None) => l.crop,
                };
                if crop == old.crop {
                    return Ok(Err(Skip::Unchanged));
                }
                match size {
                    Some(p) => match recrop_keeping(old, p, region, crop) {
                        Some(l) => l,
                        None => return Ok(Err(Skip::OutOfPicture)),
                    },
                    None if from.is_none() => old.with_crop(crop),
                    None => return Ok(Err(Skip::NoPictureSize)),
                }
            }
            AttributeGroup::Transform => {
                let Some(shown) = visible(old.crop) else {
                    return Ok(Err(Skip::NoPictureSize));
                };
                match from {
                    None => match old.transform_reset(shown) {
                        Some(l) => l,
                        None => return Ok(Err(Skip::NoPictureSize)),
                    },
                    Some(l) => {
                        let width = l.size[0].abs();
                        let height = width * shown.1 as f32 / shown.0.max(1) as f32;
                        let mut out = old;
                        out.position = l.position;
                        out.rotation = l.rotation;
                        out.size = [width.copysign(l.size[0]), height.copysign(l.size[1])];
                        out
                    }
                }
            }
            AttributeGroup::Opacity => {
                let mut out = old;
                out.opacity = from.map_or(1.0, |l| l.opacity);
                out
            }
            AttributeGroup::Style => {
                let mut out = old;
                out.style = from.map_or_else(default_style, |l| l.style);
                out
            }
            _ => return Ok(Err(Skip::NotApplicable)),
        };
        if new == old {
            return Ok(Err(Skip::Unchanged));
        }
        let undo = self
            .save_live_layout(item, own, new)
            .map_err(|e| Refusal::of(&e))?;
        records.push(undo);
        Ok(Ok(()))
    }

    /// One source group. `Ok(Err(skip))` when it is not written; a live group is queued
    /// in `live` and written after the step.
    fn write_source_group(
        &mut self,
        id: SourceId,
        group: AttributeGroup,
        snapshot: Option<&AttributeSnapshot>,
        filters: FilterPaste,
        records: &mut Vec<Undo>,
        live: &mut Vec<LiveWrite>,
    ) -> Result<Result<(), Skip>, Refusal> {
        let target = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?
            .clone();
        let (picture, sound) = kinds(&target.type_id);
        let from = snapshot.map(|s| &s.state);
        let same_type = from.is_none_or(|f| f.type_id == target.type_id);
        let applies = match group {
            AttributeGroup::VideoFilters | AttributeGroup::Glide => picture,
            AttributeGroup::AudioFilters | AttributeGroup::Sound | AttributeGroup::SyncOffset => {
                sound && from.is_none_or(|f| kinds(&f.type_id).1)
            }
            AttributeGroup::Options => same_type && !writable_options(&target).is_empty(),
            AttributeGroup::Region => {
                same_type
                    && matches!(
                        SourceKind::of(&target.type_id),
                        Some(SourceKind::Screen | SourceKind::Window)
                    )
            }
            AttributeGroup::RecordSeparately => true,
            _ => false,
        };
        if !applies {
            return Ok(Err(Skip::NotApplicable));
        }
        match group {
            AttributeGroup::VideoFilters | AttributeGroup::AudioFilters => {
                let audio = group == AttributeGroup::AudioFilters;
                let chain: Vec<_> = from
                    .map(|f| {
                        f.filters
                            .iter()
                            .filter(|x| is_audio_filter(&x.type_id) == audio)
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                let mode = if from.is_none() { FilterPaste::Replace } else { filters };
                self.write_filters(&target, audio, &chain, mode, records)
            }
            AttributeGroup::Options => {
                let defaults = self.option_defaults(&target.type_id);
                let mut settings = target.settings.clone();
                for key in writable_options(&target) {
                    let value = match from {
                        Some(f) => f.settings.get(key).cloned(),
                        None => defaults.get(key).cloned(),
                    };
                    match value {
                        Some(v) => settings.insert(key.to_owned(), v),
                        None => settings.remove(key),
                    };
                }
                if settings == target.settings {
                    return Ok(Err(Skip::Unchanged));
                }
                rbs_sources::validate_settings(&target.type_id, &settings)
                    .map_err(|e| Refusal::Invalid(e.to_string()))?;
                records.push(
                    self.collection
                        .set_source_settings(id, settings)
                        .map_err(|e| Refusal::of(&e))?,
                );
                Ok(Ok(()))
            }
            AttributeGroup::Region => {
                let (region, chrome) = from.map_or((None, None), |f| (f.region, f.chrome));
                if (region, chrome) == (target.region, target.chrome) {
                    return Ok(Err(Skip::Unchanged));
                }
                if let (Some(r), Some(p)) = (region, self.sizes.get(&id).copied())
                    && r.size(p).is_none()
                {
                    return Ok(Err(Skip::OutOfPicture));
                }
                let display = SourceKind::of(&target.type_id) == Some(SourceKind::Screen);
                if display && (chrome.is_some() || target.chrome.is_some()) {
                    records.push(
                        self.collection
                            .set_display_chrome(id, chrome)
                            .map_err(|e| Refusal::of(&e))?,
                    );
                }
                if chrome.is_none() && self.collection.source(id).is_some_and(|s| s.region != region) {
                    records.push(
                        self.collection
                            .set_source_region(id, region)
                            .map_err(|e| Refusal::of(&e))?,
                    );
                }
                Ok(Ok(()))
            }
            AttributeGroup::Glide => {
                let glide = from.and_then(|f| f.glide);
                if glide == target.glide {
                    return Ok(Err(Skip::Unchanged));
                }
                records.push(
                    self.collection
                        .set_source_glide(id, glide)
                        .map_err(|e| Refusal::of(&e))?,
                );
                Ok(Ok(()))
            }
            AttributeGroup::RecordSeparately => {
                let mut r = target.record_separately;
                match from {
                    None => r = rbs_scene::model::RecordSeparately::default(),
                    Some(f) => {
                        let s = f.record_separately;
                        if picture {
                            r.video = s.video;
                            r.video_as = s.video_as;
                        }
                        if sound {
                            r.audio = s.audio;
                        }
                    }
                }
                if r == target.record_separately {
                    return Ok(Err(Skip::Unchanged));
                }
                records.push(
                    self.collection
                        .set_source_record_separately(id, r)
                        .map_err(|e| Refusal::of(&e))?,
                );
                Ok(Ok(()))
            }
            AttributeGroup::Sound => {
                let mut audio = target.audio;
                let want = from.map_or_else(SourceAudio::default, |f| f.audio);
                audio.volume = want.volume;
                audio.muted = want.muted;
                audio.monitor = want.monitor;
                if audio == target.audio {
                    return Ok(Err(Skip::Unchanged));
                }
                live.push(LiveWrite::Sound(id, audio));
                Ok(Ok(()))
            }
            AttributeGroup::SyncOffset => {
                let ms = from.map_or(0, |f| f.audio.sync_offset_ms);
                if ms == target.audio.sync_offset_ms {
                    return Ok(Err(Skip::Unchanged));
                }
                live.push(LiveWrite::SyncOffset(id, ms));
                Ok(Ok(()))
            }
            _ => Ok(Err(Skip::NotApplicable)),
        }
    }

    /// Writes one kind of filters: the same type sequence keeps the target's ids and
    /// writes only `enabled` and settings (no rebuild); otherwise the kind's filters
    /// are removed and the new ones added (ADR-0133 decision 6).
    fn write_filters(
        &mut self,
        target: &Source,
        audio: bool,
        chain: &[rbs_scene::model::Filter],
        mode: FilterPaste,
        records: &mut Vec<Undo>,
    ) -> Result<Result<(), Skip>, Refusal> {
        let id = target.id;
        let own: Vec<_> = target
            .filters
            .iter()
            .filter(|f| is_audio_filter(&f.type_id) == audio)
            .cloned()
            .collect();
        let before = records.len();
        let same_types = own.len() == chain.len()
            && own.iter().zip(chain).all(|(a, b)| a.type_id == b.type_id);
        if mode == FilterPaste::Replace && same_types {
            for (mine, theirs) in own.iter().zip(chain) {
                if mine.settings != theirs.settings {
                    records.push(
                        self.collection
                            .set_filter_settings(id, mine.id, theirs.settings.clone())
                            .map_err(|e| Refusal::of(&e))?,
                    );
                }
                if mine.enabled != theirs.enabled {
                    records.push(
                        self.collection
                            .set_filter_enabled(id, mine.id, theirs.enabled)
                            .map_err(|e| Refusal::of(&e))?,
                    );
                }
            }
        } else {
            if mode == FilterPaste::Replace {
                for f in &own {
                    records.push(
                        self.collection
                            .remove_filter(id, f.id)
                            .map_err(|e| Refusal::of(&e))?,
                    );
                }
            }
            for f in chain {
                let (new, undo) = self
                    .collection
                    .add_filter(id, f.type_id.clone(), f.settings.clone())
                    .map_err(|e| Refusal::of(&e))?;
                records.push(undo);
                if !f.enabled {
                    records.push(
                        self.collection
                            .set_filter_enabled(id, new, false)
                            .map_err(|e| Refusal::of(&e))?,
                    );
                }
            }
        }
        Ok(if records.len() == before {
            Err(Skip::Unchanged)
        } else {
            Ok(())
        })
    }
}

/// A live write queued until the undo step is recorded.
#[derive(Debug, Clone, Copy)]
pub(super) enum LiveWrite {
    Sound(SourceId, SourceAudio),
    SyncOffset(SourceId, i32),
}

impl LiveWrite {
    fn group(self) -> AttributeGroup {
        match self {
            Self::Sound(..) => AttributeGroup::Sound,
            Self::SyncOffset(..) => AttributeGroup::SyncOffset,
        }
    }
}

/// A browser page's size on a `w` x `h` canvas: the canvas, at most 1920x1080, even.
pub(super) fn browser_size(w: u32, h: u32) -> (i64, i64) {
    let width = i64::from(w).min(1920) & !1;
    let height = i64::from(h).min(1080) & !1;
    (width.max(2), height.max(2))
}

/// `crop` of a `from` picture, in proportion on a `to` picture.
fn scale_crop(crop: Crop, from: (u32, u32), to: (u32, u32)) -> Crop {
    let s = |v: u32, a: u32, b: u32| -> u32 {
        if a == 0 {
            return v;
        }
        let scaled = (u64::from(v) * u64::from(b) + u64::from(a) / 2) / u64::from(a);
        u32::try_from(scaled).unwrap_or(u32::MAX)
    };
    Crop::new(
        s(crop.left, from.0, to.0),
        s(crop.top, from.1, to.1),
        s(crop.right, from.0, to.0),
        s(crop.bottom, from.1, to.1),
    )
}

// ----- control API (protocol 1.68) -----

use rbs_api::error::{ApiError, ErrorKind};
use rbs_api::event::{EventPayload, StateKey, Topic};
use rbs_api::request::{DuplicateScene, PasteAttributes, ResetSource, Response, SourceInScene};
use rbs_api::types as api;

use super::{Update, refused};

/// The groups named by `ids`: `invalid_params` for an empty list or an unknown id.
fn parse_groups(ids: &[String]) -> Result<Vec<AttributeGroup>, ApiError> {
    if ids.is_empty() {
        return Err(ApiError::new(
            ErrorKind::InvalidParams,
            "groups must name at least one group",
        ));
    }
    ids.iter()
        .map(|id| {
            AttributeGroup::from_id(id).ok_or_else(|| {
                ApiError::new(ErrorKind::InvalidParams, format!("there is no group {id:?}"))
            })
        })
        .collect()
}

fn report_view(r: &TargetReport) -> api::AttributeTarget {
    api::AttributeTarget {
        source: r.source.get(),
        item: r.item.map(ItemId::get),
        changed: r.changed.iter().map(|g| g.id().to_owned()).collect(),
        live: r.live.iter().map(|g| g.id().to_owned()).collect(),
        skipped: r
            .skipped
            .iter()
            .map(|(g, why)| api::SkippedGroup {
                group: g.id().to_owned(),
                reason: why.id().to_owned(),
            })
            .collect(),
    }
}

impl AttributeSnapshot {
    /// What `source.attributes_clipboard` answers.
    pub(super) fn view(&self) -> api::AttributesClipboard {
        api::AttributesClipboard {
            source: self.source.get(),
            name: self.state.name.clone(),
            type_id: self.state.type_id.as_str().to_owned(),
            groups: self.groups.iter().map(|g| g.id().to_owned()).collect(),
        }
    }
}

impl Host {
    /// Runs `f` on the scene an API call names: `None` is the scene being edited. A
    /// global source with a `scene` is refused (ADR-0133 decision 3).
    fn in_named_scene<T>(
        &mut self,
        source: SourceId,
        scene: Option<u32>,
        f: impl FnOnce(&mut Self) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        match scene {
            None => f(self),
            Some(_) if self.global_item(source).is_some() => Err(ApiError::new(
                ErrorKind::InvalidParams,
                "a global source takes no scene",
            )),
            Some(_) => self.placing_into(scene, f),
        }
    }

    /// Publishes `source.attributes_clipboard.changed` and tells the window.
    pub(super) fn clipboard_changed(&mut self, notify: &dyn Fn(Update)) {
        let view = self.attribute_clipboard.as_ref().map(AttributeSnapshot::view);
        notify(Update::AttributesClipboard(view.clone()));
        self.events.changed(
            StateKey::topic(Topic::AttributesClipboardChanged),
            || {
                EventPayload::AttributesClipboardChanged(api::AttributesClipboardChanged {
                    clipboard: view,
                })
            },
        );
    }

    pub(super) fn api_scene_duplicate(
        &mut self,
        p: DuplicateScene,
        notify: &dyn Fn(Update),
    ) -> Result<Response, ApiError> {
        let scene = SceneId::from_raw(p.scene);
        let answer = match p.sources {
            api::DuplicateSources::Shared => api::SceneDuplicated {
                scene: self.duplicate_scene(scene).map_err(|e| refused(&e))?.get(),
                sources: Vec::new(),
            },
            api::DuplicateSources::Copied => {
                let (id, pairs) = self
                    .duplicate_scene_with_copies(scene)
                    .map_err(|e| refused(&e))?;
                api::SceneDuplicated {
                    scene: id.get(),
                    sources: pairs
                        .into_iter()
                        .map(|(from, to)| api::CopiedSource {
                            from: from.get(),
                            to: to.get(),
                        })
                        .collect(),
                }
            }
        };
        self.apply(notify);
        Ok(Response::SceneDuplicated(answer))
    }

    pub(super) fn api_source_duplicate(
        &mut self,
        p: SourceInScene,
        notify: &dyn Fn(Update),
    ) -> Result<Response, ApiError> {
        let id = SourceId::from_raw(p.source);
        let made = self.in_named_scene(id, p.scene, |host| {
            host.duplicate_source(id).map_err(|e| refused(&e))
        })?;
        self.apply(notify);
        Ok(Response::SourceDuplicated(api::SourceDuplicated {
            source: made.source.get(),
            item: made.item.get(),
            active: made.active,
            companions: made
                .companions
                .iter()
                .map(|(s, i)| api::PlacedCopy {
                    source: s.get(),
                    item: i.get(),
                })
                .collect(),
        }))
    }

    pub(super) fn api_source_reset(
        &mut self,
        p: ResetSource,
        notify: &dyn Fn(Update),
    ) -> Result<Response, ApiError> {
        let id = SourceId::from_raw(p.source);
        let groups = match &p.groups {
            Some(ids) => parse_groups(ids)?,
            None => AttributeGroup::default_choice(),
        };
        let canvas = p.canvas;
        let report = self.in_named_scene(id, p.scene, |host| {
            host.reset_source(id, &groups, canvas).map_err(|e| refused(&e))
        })?;
        if !report.changed.is_empty() || !report.live.is_empty() {
            self.apply(notify);
        }
        Ok(Response::AttributesApplied(api::AttributesApplied {
            targets: vec![report_view(&report)],
        }))
    }

    pub(super) fn api_copy_attributes(
        &mut self,
        p: SourceInScene,
        notify: &dyn Fn(Update),
    ) -> Result<Response, ApiError> {
        let id = SourceId::from_raw(p.source);
        let canvas = p.canvas;
        let snapshot = self.in_named_scene(id, p.scene, |host| {
            host.copy_attributes(id, canvas).map_err(|e| refused(&e))
        })?;
        self.clipboard_changed(notify);
        Ok(Response::AttributesClipboard(Some(snapshot.view())))
    }

    pub(super) fn api_attributes_clipboard(&self) -> Response {
        Response::AttributesClipboard(self.attribute_clipboard.as_ref().map(AttributeSnapshot::view))
    }

    pub(super) fn api_paste_attributes(
        &mut self,
        p: PasteAttributes,
        notify: &dyn Fn(Update),
    ) -> Result<Response, ApiError> {
        let groups = parse_groups(&p.groups)?;
        if p.items.is_empty() {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "items must name at least one item",
            ));
        }
        let items: Vec<ItemId> = p.items.iter().map(|i| ItemId::from_raw(*i)).collect();
        let mode = match p.filters {
            api::FilterPasteMode::Replace => FilterPaste::Replace,
            api::FilterPasteMode::Append => FilterPaste::Append,
        };
        let reports = self
            .paste_attributes(&items, &groups, mode, p.canvas)
            .map_err(|e| refused(&e))?;
        if reports
            .iter()
            .any(|r| !r.changed.is_empty() || !r.live.is_empty())
        {
            self.apply(notify);
        }
        Ok(Response::AttributesApplied(api::AttributesApplied {
            targets: reports.iter().map(report_view).collect(),
        }))
    }
}
