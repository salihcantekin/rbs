//! The planner: OBS objects to a validated RBS collection plus the report
//! (ADR-0131 decisions 4 to 8).

use std::collections::{BTreeSet, HashMap};

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{
    Canvas, CanvasId, Filter, FilterId, ItemContent, ItemId, Layout, MAX_SOURCE_VOLUME,
    MAX_SYNC_OFFSET_MS, Scene, SceneId, SceneItem, SceneScope, SettingValue, Source, SourceAudio,
    SourceId, SourceMonitor,
};
use rbs_scene::{
    IdCounters, MAX_CANVAS_SIDE, MAX_FRAME_RATE, MAX_LAYERS, MAX_NESTING_DEPTH, SceneCollection,
    SourceRegion,
};
use serde_json::Value;

use crate::env::{Catalogue, Lookup};
use crate::mappers::filters::OBS_AUDIO_FILTERS;
use crate::mappers::sources::{rbs as src_types, unmapped_reason};
use crate::mappers::{
    FilterKind, FilterOutcome, MapContext, MappedSource, Mappers, SourceOutcome, available,
};
use crate::obs::{GLOBAL_AUDIO_KEYS, ObsItem, ObsSource, flag, num, type_label, uint, vec2};
use crate::report::{Area, ImportReport, Outcome, ReportEntry};
use crate::transform::{Bounds, ObsTransform, Placed, compose, layout_from_corners, place};
use crate::{
    FALLBACK_CANVAS, FALLBACK_FPS, ImportError, ImportPlan, ImportedSecrets, ObsProfileFacts,
    Options, SceneChoice,
};

/// The name RBS gives a canvas's global scene (ADR-0065).
const GLOBAL_SCENE_NAME: &str = "Global sources";

/// Names unique ignoring case, as the app compares them.
#[derive(Default)]
struct Names(BTreeSet<String>);

impl Names {
    fn unique(&mut self, wanted: &str, fallback: &str) -> String {
        let base = match wanted.trim() {
            "" => fallback.to_owned(),
            w => w.to_owned(),
        };
        let mut name = base.clone();
        let mut n = 2;
        while !self.0.insert(name.to_lowercase()) {
            name = format!("{base} {n}");
            n += 1;
        }
        name
    }
}

/// A source that came across.
#[derive(Clone)]
struct Imported {
    id: SourceId,
    native: Option<(u32, u32)>,
    region: Option<[u32; 4]>,
    audio_only: bool,
}

/// An item before limits and ids.
struct Pending {
    content: PendingContent,
    layout: Layout,
    visible: bool,
    locked: bool,
}

#[derive(Clone, Copy)]
enum PendingContent {
    Source(SourceId),
    Scene(usize),
}

struct Ctx<'a> {
    all: Vec<ObsSource>,
    by_name: HashMap<String, usize>,
    by_uuid: HashMap<String, usize>,
    mappers: &'a Mappers,
    catalogue: &'a dyn Catalogue,
    options: &'a Options,
    report: ImportReport,
    obs_canvas: (u32, u32),
    next_source: u32,
    next_filter: u32,
    source_names: Names,
    imported: HashMap<String, Imported>,
    sources: Vec<Source>,
    /// `(source, filter, key, OBS name)` to resolve in pass two.
    refs: Vec<(SourceId, FilterId, String, String, String)>,
}

fn entry(
    outcome: Outcome,
    area: Area,
    name: &str,
    ty: &str,
    reason: impl Into<String>,
) -> ReportEntry {
    ReportEntry::new(outcome, area, name, ty, reason)
}

/// An imported scene: its OBS source index, RBS id and name.
pub(crate) type PlannedScene = (usize, SceneId, String);

pub(crate) fn build(
    root: &Value,
    profile: Option<ObsProfileFacts>,
    mappers: &Mappers,
    catalogue: &dyn Catalogue,
    lookup: &dyn Lookup,
    options: &Options,
    mut report: ImportReport,
) -> Result<(ImportPlan, Vec<PlannedScene>), ImportError> {
    let name = root
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.trim().is_empty())
        .unwrap_or("OBS")
        .trim()
        .to_owned();

    // Canvas: positions are in OBS canvas pixels, so the OBS canvas is the layout space
    // (ADR-0110); the RBS canvas takes it only when chosen.
    let valid_size = |(w, h): (u32, u32)| {
        (1..=MAX_CANVAS_SIDE).contains(&w) && (1..=MAX_CANVAS_SIDE).contains(&h)
    };
    let valid_fps = |(n, d): (u32, u32)| {
        n > 0 && d > 0 && u64::from(n) <= u64::from(MAX_FRAME_RATE) * u64::from(d)
    };
    let (obs_canvas, fps) = match &profile {
        None => {
            report.push(entry(
                Outcome::Approximated,
                Area::Profile,
                "Canvas",
                "",
                "No OBS profile was chosen, so the canvas is 1920 x 1080 at 30 frames per second and positions assume that size.",
            ).with_hint("Choose the OBS profile too, so positions match."));
            (FALLBACK_CANVAS, FALLBACK_FPS)
        }
        Some(p) => {
            let size = p.video.base.filter(|s| valid_size(*s));
            let rate = p.video.fps.filter(|r| valid_fps(*r));
            if size.is_none() || rate.is_none() {
                report.push(entry(
                    Outcome::Approximated,
                    Area::Profile,
                    "Canvas",
                    "",
                    "The profile's canvas size or frame rate is missing or out of range, so 1920 x 1080 at 30 frames per second is used.",
                ));
            }
            (
                size.unwrap_or(FALLBACK_CANVAS),
                rate.unwrap_or(FALLBACK_FPS),
            )
        }
    };
    let (rbs_size, rbs_fps) = if options.canvas {
        (obs_canvas, fps)
    } else {
        if profile.is_some() {
            report.push(entry(
                Outcome::NotChosen,
                Area::Profile,
                "Canvas",
                "",
                "The canvas size and frame rate were not chosen; RBS's defaults are used.",
            ));
        }
        (FALLBACK_CANVAS, FALLBACK_FPS)
    };

    let mut all: Vec<ObsSource> = Vec::new();
    for key in ["sources", "groups"] {
        if let Some(a) = root.get(key).and_then(Value::as_array) {
            all.extend(a.iter().filter_map(ObsSource::parse));
        }
    }
    let mut by_name = HashMap::new();
    let mut by_uuid = HashMap::new();
    for (i, s) in all.iter().enumerate() {
        by_name.entry(s.name.clone()).or_insert(i);
        if let Some(u) = &s.uuid {
            by_uuid.entry(u.clone()).or_insert(i);
        }
    }

    let mut ctx = Ctx {
        all,
        by_name,
        by_uuid,
        mappers,
        catalogue,
        options,
        report,
        obs_canvas,
        next_source: 1,
        next_filter: 1,
        source_names: Names::default(),
        imported: HashMap::new(),
        sources: Vec::new(),
        refs: Vec::new(),
    };
    let mut map_ctx = MapContext::new(lookup, catalogue, obs_canvas);

    // Scenes in OBS's order: `scene_order`, then any other scene.
    let mut order: Vec<usize> = Vec::new();
    if let Some(a) = root.get("scene_order").and_then(Value::as_array) {
        for s in a {
            if let Some(i) = s
                .get("name")
                .and_then(Value::as_str)
                .and_then(|n| ctx.by_name.get(n))
                && ctx.all[*i].is_scene()
                && !order.contains(i)
            {
                order.push(*i);
            }
        }
    }
    for (i, s) in ctx.all.iter().enumerate() {
        if s.is_scene() && !order.contains(&i) {
            order.push(i);
        }
    }
    let chosen = ctx.chosen_scenes(&order);
    for &i in &order {
        if !chosen.contains(&i) {
            let s = &ctx.all[i];
            ctx.report.push(entry(
                Outcome::NotChosen,
                Area::Scene,
                &s.name,
                "scene",
                "This scene was not chosen.",
            ));
        }
    }
    let scenes: Vec<usize> = order
        .iter()
        .copied()
        .filter(|i| chosen.contains(i))
        .collect();

    // Sources placed in the chosen scenes, with the scenes they are in.
    let mut placed: Vec<(usize, BTreeSet<String>)> = Vec::new();
    for &si in &scenes {
        let scene_name = ctx.all[si].name.clone();
        let mut stack = vec![si];
        while let Some(owner) = stack.pop() {
            for item in ctx.all[owner].items() {
                let Some(t) = ctx.resolve(&item) else {
                    continue;
                };
                let target = &ctx.all[t];
                if target.is_group() {
                    stack.push(t);
                } else if !target.is_scene() {
                    match placed.iter_mut().find(|(i, _)| *i == t) {
                        Some((_, in_scenes)) => {
                            in_scenes.insert(scene_name.clone());
                        }
                        None => placed.push((t, BTreeSet::from([scene_name.clone()]))),
                    }
                }
            }
        }
    }
    for (i, in_scenes) in placed {
        let src = ctx.all[i].clone();
        ctx.import_source(&src, in_scenes.into_iter().collect(), &mut map_ctx);
    }

    // OBS's global audio devices.
    let mut globals: Vec<SourceId> = Vec::new();
    for key in GLOBAL_AUDIO_KEYS {
        let Some(src) = root.get(key).and_then(ObsSource::parse) else {
            continue;
        };
        if !ctx.options.global_audio {
            ctx.report.push(entry(
                Outcome::NotChosen,
                Area::Source,
                &src.name,
                &src.id,
                "Global audio devices were not chosen.",
            ));
            continue;
        }
        if let Some(imp) = ctx.import_source(&src, Vec::new(), &mut map_ctx) {
            globals.push(imp.id);
        }
    }

    ctx.resolve_refs();

    // Scenes and items.
    let mut scene_ids: HashMap<usize, SceneId> = HashMap::new();
    let mut scene_names = Names::default();
    let mut scene_list: Vec<(usize, SceneId, String)> = Vec::new();
    let mut renamed = 0usize;
    for (n, &si) in scenes.iter().enumerate() {
        let id = SceneId::from_raw(n as u32 + 1);
        scene_ids.insert(si, id);
        let obs = &ctx.all[si];
        let name = scene_names.unique(&obs.name, "Scene");
        if name != obs.name.trim() {
            renamed += 1;
            ctx.report.push(entry(
                Outcome::Approximated,
                Area::Scene,
                &obs.name,
                "scene",
                format!("Renamed to \"{name}\" because the name was empty or taken."),
            ));
        }
        if !obs.filters.is_empty() {
            for f in &obs.filters {
                ctx.report.push(entry(
                    Outcome::Skipped,
                    Area::Filter,
                    &format!("{}: {}", obs.name, f.name),
                    &f.id,
                    "Filters on scenes are not in RBS.",
                ));
            }
        }
        scene_list.push((si, id, name));
    }
    let mut pending: HashMap<usize, Vec<Pending>> = HashMap::new();
    for &si in &scenes {
        let items = ctx.scene_items(si, &scene_ids);
        pending.insert(si, items);
    }

    // Limits (ADR-0131 decision 4): cycles, nesting depth, layers with the globals.
    let global_layers = globals.len() as u64;
    let mut memo: HashMap<usize, (usize, u64)> = HashMap::new();
    for &si in &scenes {
        let mut on_stack = Vec::new();
        ctx.limit(si, &mut pending, &mut memo, &mut on_stack, global_layers);
    }

    // Ids and the collection.
    let canvas_id = CanvasId::from_raw(1);
    let mut next_item = 1u32;
    let mut item_id = || {
        let id = ItemId::from_raw(next_item);
        next_item += 1;
        id
    };
    let mut out_scenes = Vec::new();
    for (si, id, name) in &scene_list {
        let mut scene = Scene::new(*id, name, SceneScope::Shared);
        for p in pending.remove(si).unwrap_or_default() {
            let content = match p.content {
                PendingContent::Source(s) => ItemContent::Source(s),
                PendingContent::Scene(c) => match scene_ids.get(&c) {
                    Some(id) => ItemContent::Scene(*id),
                    None => continue,
                },
            };
            let mut item = SceneItem::new(item_id(), content, p.layout);
            item.visible = p.visible;
            item.locked = p.locked;
            scene.items.push(item);
        }
        out_scenes.push(scene);
    }
    let mut canvas = Canvas::new(
        canvas_id,
        "Main",
        Size::new(rbs_size.0, rbs_size.1),
        FrameRate::new(rbs_fps.0, rbs_fps.1).unwrap_or(FrameRate::FPS_30),
    );
    canvas.layout_size = Size::new(obs_canvas.0, obs_canvas.1);
    if !globals.is_empty() {
        let gid = SceneId::from_raw(out_scenes.len() as u32 + 1);
        let mut g = Scene::new(gid, GLOBAL_SCENE_NAME, SceneScope::Canvas(canvas_id));
        for s in &globals {
            g.items.push(SceneItem::new(
                item_id(),
                ItemContent::Source(*s),
                full_canvas(obs_canvas),
            ));
        }
        out_scenes.push(g);
        canvas.globals = Some(gid);
    }
    let program_name = ["current_program_scene", "current_scene"]
        .iter()
        .find_map(|k| root.get(*k).and_then(Value::as_str));
    canvas.program = program_name
        .and_then(|n| ctx.by_name.get(n))
        .and_then(|i| scene_ids.get(i))
        .copied()
        .or_else(|| scene_list.first().map(|(_, id, _)| *id));
    ctx.report.exact.scenes = scene_list.len() - renamed;

    let counters = IdCounters {
        canvas: 2,
        source: ctx.next_source,
        scene: out_scenes.len() as u32 + 1,
        item: next_item,
        filter: ctx.next_filter,
    };
    let collection = SceneCollection::from_parts(
        vec![canvas],
        std::mem::take(&mut ctx.sources),
        out_scenes,
        counters,
    )
    .map_err(|e| ImportError::Internal(e.to_string()))?;
    Ok((
        ImportPlan {
            name,
            collection,
            profile,
            secrets: ImportedSecrets::default(),
            shortcuts: Vec::new(),
            report: ctx.report,
        },
        scene_list,
    ))
}

fn full_canvas(canvas: (u32, u32)) -> Layout {
    Layout::new([0.0, 0.0], [canvas.0 as f32, canvas.1 as f32])
}

/// An OBS item's transform. `None` for OBS 31 items with only relative keys.
fn obs_transform(item: &ObsItem) -> ObsTransform {
    let r = &item.raw;
    let d = ObsTransform::default();
    ObsTransform {
        pos: vec2(r, "pos").unwrap_or(d.pos),
        rot: num(r, "rot", 0.0),
        scale: vec2(r, "scale").unwrap_or(d.scale),
        align: uint(r, "align", 5) as u32,
        bounds: Bounds::from_obs(uint(r, "bounds_type", 0)),
        bounds_align: uint(r, "bounds_align", 0) as u32,
        bounds_size: vec2(r, "bounds").unwrap_or([0.0, 0.0]),
        bounds_crop: flag(r, "bounds_crop", false),
        crop: [
            uint(r, "crop_left", 0) as u32,
            uint(r, "crop_top", 0) as u32,
            uint(r, "crop_right", 0) as u32,
            uint(r, "crop_bottom", 0) as u32,
        ],
    }
}

impl Ctx<'_> {
    fn resolve(&self, item: &ObsItem) -> Option<usize> {
        item.source_uuid
            .as_ref()
            .and_then(|u| self.by_uuid.get(u))
            .or_else(|| self.by_name.get(&item.name))
            .copied()
    }

    /// The chosen scenes and every scene they nest, also through groups.
    fn chosen_scenes(&self, order: &[usize]) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        let mut todo: Vec<usize> = match &self.options.scenes {
            SceneChoice::All => order.to_vec(),
            SceneChoice::Only(names) => order
                .iter()
                .copied()
                .filter(|i| names.contains(&self.all[*i].name))
                .collect(),
        };
        while let Some(i) = todo.pop() {
            let s = &self.all[i];
            if s.is_scene() && !out.insert(i) {
                continue;
            }
            for item in s.items() {
                if let Some(t) = self.resolve(&item) {
                    let ts = &self.all[t];
                    if (ts.is_scene() && !out.contains(&t)) || ts.is_group() {
                        todo.push(t);
                    }
                }
            }
        }
        out
    }

    fn import_source(
        &mut self,
        src: &ObsSource,
        scenes: Vec<String>,
        map_ctx: &mut MapContext<'_>,
    ) -> Option<Imported> {
        if let Some(done) = self.imported.get(&src.name) {
            return Some(done.clone());
        }
        let label = type_label(&src.id);
        let skip = |report: &mut ImportReport, reason: String, hint: Option<String>| {
            let mut e = entry(
                Outcome::Skipped,
                Area::Source,
                &src.name,
                &src.versioned_id,
                reason,
            );
            e.scenes = scenes.clone();
            e.hint = hint;
            report.push(e);
        };
        let Some(mapper) = self.mappers.source(&src.id) else {
            let (reason, hint) = unmapped_reason(&src.id);
            skip(&mut self.report, reason, hint);
            return None;
        };
        if !available(mapper.rbs_types(), self.catalogue) {
            skip(
                &mut self.report,
                format!("{label} sources are not available in this build of RBS."),
                None,
            );
            return None;
        }
        let m: MappedSource = match mapper.map(src, map_ctx) {
            SourceOutcome::Mapped(m) => m,
            SourceOutcome::Skipped { reason, hint } => {
                skip(&mut self.report, reason, hint);
                return None;
            }
        };
        let mut notes = m.notes.clone();
        let id = SourceId::from_raw(self.next_source);
        self.next_source += 1;
        let name = self.source_names.unique(&src.name, label);
        if name != src.name.trim() {
            notes.push(format!(
                "Renamed to \"{name}\" because the name was empty or taken."
            ));
        }
        let mut source = Source::new(id, &name, NodeTypeId::new(m.type_id), m.settings.clone());
        source.hints = m.hints.clone();
        source.activity = m.activity;
        source.audio = self.audio_of(src, &m, &mut notes);
        let audio_only = matches!(
            m.type_id,
            src_types::AUDIO_INPUT | src_types::AUDIO_OUTPUT | src_types::APP_AUDIO
        );
        let region = self.filters_of(src, &name, id, &m, &mut source, map_ctx);
        source.region = region.map(|[l, t, r, b]| SourceRegion::new(l, t, r, b));
        if notes.is_empty() {
            self.report.exact.sources += 1;
        } else {
            let mut e = entry(
                Outcome::Approximated,
                Area::Source,
                &src.name,
                &src.versioned_id,
                notes.join(" "),
            );
            e.scenes = scenes;
            self.report.push(e);
        }
        self.sources.push(source);
        let imp = Imported {
            id,
            native: m.native,
            region,
            audio_only,
        };
        self.imported.insert(src.name.clone(), imp.clone());
        Some(imp)
    }

    fn audio_of(&self, src: &ObsSource, m: &MappedSource, notes: &mut Vec<String>) -> SourceAudio {
        let volume = if src.volume.is_finite() {
            (src.volume as f32).clamp(0.0, MAX_SOURCE_VOLUME)
        } else {
            MAX_SOURCE_VOLUME
        };
        if m.has_audio && src.volume.is_finite() && src.volume > f64::from(MAX_SOURCE_VOLUME) + 1e-6
        {
            notes.push("Its volume was above 0 dB; RBS limits a source to 0 dB (use a Gain filter to boost).".to_owned());
        }
        let ms = src.sync_ns / 1_000_000;
        let max = i64::from(MAX_SYNC_OFFSET_MS);
        let sync = ms.clamp(-max, max) as i32;
        if m.has_audio && i64::from(sync) != ms {
            notes.push(format!(
                "Its sync offset was {ms} ms; RBS allows up to {max} ms either way."
            ));
        }
        let monitor = match src.monitoring_type {
            1 => SourceMonitor::Only,
            2 => SourceMonitor::Also,
            _ => SourceMonitor::Off,
        };
        if m.has_audio {
            if (src.balance - 0.5).abs() > 1e-6 {
                notes.push("Left-right balance is not in RBS.".to_owned());
            }
            if src.audio_mixers & 0x3F != 0x3F {
                notes.push(
                    "Separate audio tracks are not in RBS yet; the source is in the one mix."
                        .to_owned(),
                );
            }
            if src.push_to {
                notes.push("Push-to-talk and push-to-mute are not in RBS yet.".to_owned());
            }
        }
        SourceAudio::new(volume, src.muted)
            .with_sync_offset(sync)
            .with_monitor(monitor)
            .with_mixer_hidden(flag(&src.private_settings, "mixer_hidden", false))
    }

    /// Maps the filters; returns the region of a crop filter.
    fn filters_of(
        &mut self,
        src: &ObsSource,
        rbs_name: &str,
        source_id: SourceId,
        m: &MappedSource,
        source: &mut Source,
        map_ctx: &mut MapContext<'_>,
    ) -> Option<[u32; 4]> {
        let mut region = None;
        let mut video_before = false;
        for f in &src.filters {
            let label = format!("{}: {}", src.name, f.name);
            let mapper = self.mappers.filter(&f.id);
            let kind =
                mapper
                    .map(|m| m.kind())
                    .unwrap_or(if OBS_AUDIO_FILTERS.contains(&f.id.as_str()) {
                        FilterKind::Audio
                    } else {
                        FilterKind::Video
                    });
            let chosen = match kind {
                FilterKind::Audio => self.options.audio_filters,
                FilterKind::Video => self.options.video_filters,
            };
            if !chosen {
                let what = if kind == FilterKind::Audio {
                    "Audio"
                } else {
                    "Video"
                };
                self.report.push(entry(
                    Outcome::NotChosen,
                    Area::Filter,
                    &label,
                    &f.id,
                    format!("{what} filters were not chosen."),
                ));
                continue;
            }
            let Some(mapper) = mapper else {
                self.report.push(entry(
                    Outcome::Skipped,
                    Area::Filter,
                    &label,
                    &f.id,
                    "This filter is not in RBS yet.",
                ));
                continue;
            };
            if !available(mapper.rbs_types(), self.catalogue) {
                self.report.push(entry(
                    Outcome::Skipped,
                    Area::Filter,
                    &label,
                    &f.id,
                    "This filter is not available in this build of RBS.",
                ));
                continue;
            }
            match mapper.map(f, map_ctx) {
                FilterOutcome::Filter {
                    type_id,
                    settings,
                    source_refs,
                    notes,
                } => {
                    let fid = FilterId::from_raw(self.next_filter);
                    self.next_filter += 1;
                    let mut filter = Filter::new(fid, NodeTypeId::new(type_id), settings);
                    filter.enabled = f.enabled;
                    source.filters.push(filter);
                    for (key, obs_name) in source_refs {
                        self.refs
                            .push((source_id, fid, key, obs_name, label.clone()));
                    }
                    if kind == FilterKind::Video && f.enabled {
                        video_before = true;
                    }
                    if notes.is_empty() {
                        self.report.exact.filters += 1;
                    } else {
                        self.report.push(entry(
                            Outcome::Approximated,
                            Area::Filter,
                            &label,
                            &f.id,
                            notes.join(" "),
                        ));
                    }
                }
                FilterOutcome::Region {
                    left,
                    top,
                    right,
                    bottom,
                    size,
                } => {
                    let skip = |report: &mut ImportReport, reason: &str| {
                        report.push(entry(Outcome::Skipped, Area::Filter, &label, &f.id, reason));
                    };
                    if !f.enabled {
                        skip(
                            &mut self.report,
                            "The crop filter is off in OBS, so it was not used.",
                        );
                        continue;
                    }
                    if region.is_some() {
                        skip(
                            &mut self.report,
                            "Only the first crop filter of a source is used.",
                        );
                        continue;
                    }
                    let insets = match (right, bottom, size) {
                        (Some(r), Some(b), _) => Some([left, top, r, b]),
                        (_, _, Some((cx, cy))) => m.native.map(|(w, h)| {
                            [
                                left,
                                top,
                                w.saturating_sub(left.saturating_add(cx)),
                                h.saturating_sub(top.saturating_add(cy)),
                            ]
                        }),
                        _ => None,
                    };
                    let Some(insets) = insets else {
                        skip(
                            &mut self.report,
                            "The crop filter keeps a size, and the source's picture size is not known here.",
                        );
                        continue;
                    };
                    region = Some(insets);
                    if video_before {
                        self.report.push(entry(
                            Outcome::Approximated,
                            Area::Filter,
                            &label,
                            &f.id,
                            "The crop now happens after the source's other filters.",
                        ));
                    } else {
                        self.report.exact.filters += 1;
                    }
                }
                FilterOutcome::Skipped { reason } => {
                    self.report
                        .push(entry(Outcome::Skipped, Area::Filter, &label, &f.id, reason));
                }
            }
        }
        let _ = rbs_name;
        region
    }

    /// Pass two: settings that name another source by OBS name.
    fn resolve_refs(&mut self) {
        for (sid, fid, key, obs_name, label) in std::mem::take(&mut self.refs) {
            let target = self.imported.get(&obs_name).map(|i| i.id);
            let Some(source) = self.sources.iter_mut().find(|s| s.id == sid) else {
                continue;
            };
            let Some(filter) = source.filters.iter_mut().find(|f| f.id == fid) else {
                continue;
            };
            match target {
                Some(t) if t != sid => {
                    filter
                        .settings
                        .insert(key, SettingValue::Int(i64::from(t.get())));
                }
                _ => self.report.push(entry(
                    Outcome::Approximated,
                    Area::Filter,
                    &label,
                    "",
                    format!("Its key source \"{obs_name}\" was not imported, so it does nothing until one is chosen."),
                )),
            }
        }
    }

    /// The items of a scene, groups flattened.
    fn scene_items(&mut self, si: usize, scene_ids: &HashMap<usize, SceneId>) -> Vec<Pending> {
        let scene_name = self.all[si].name.clone();
        let mut out = Vec::new();
        for item in self.all[si].items() {
            self.item(&item, &scene_name, None, scene_ids, &mut out);
        }
        out
    }

    fn item(
        &mut self,
        item: &ObsItem,
        scene_name: &str,
        group: Option<(&Placed, (u32, u32), bool, bool)>,
        scene_ids: &HashMap<usize, SceneId>,
        out: &mut Vec<Pending>,
    ) {
        let item_entry = |report: &mut ImportReport, outcome: Outcome, reason: String| {
            let mut e = entry(outcome, Area::Item, &item.name, "", reason);
            e.scenes = vec![scene_name.to_owned()];
            report.push(e);
        };
        if item.relative_only() {
            item_entry(
                &mut self.report,
                Outcome::Skipped,
                "OBS 31 saved this item with relative positions only, which RBS does not read yet."
                    .to_owned(),
            );
            return;
        }
        let Some(t) = self.resolve(item) else {
            item_entry(
                &mut self.report,
                Outcome::Skipped,
                "It places a source that is not in the file.".to_owned(),
            );
            return;
        };
        let target = self.all[t].clone();
        let visible = item.visible && group.is_none_or(|g| g.2);
        let locked = item.locked || group.is_some_and(|g| g.3);
        let mut notes: Vec<String> = Vec::new();
        let mut t_obs = obs_transform(item);
        if target.is_scene() && (t_obs.crop != [0; 4] || t_obs.bounds_crop) {
            // RBS draws a nested scene whole (CropOnScene): place it uncropped.
            notes.push("A crop on a nested scene is not in RBS.".to_owned());
            t_obs.crop = [0; 4];
            t_obs.bounds_crop = false;
        }

        if target.is_group() {
            if group.is_some() {
                item_entry(
                    &mut self.report,
                    Outcome::Skipped,
                    "A group inside a group is not in OBS's format.".to_owned(),
                );
                return;
            }
            let gsize = (
                uint(&target.settings, "cx", u64::from(self.obs_canvas.0)).max(1) as u32,
                uint(&target.settings, "cy", u64::from(self.obs_canvas.1)).max(1) as u32,
            );
            let Some(gp) = place(&t_obs, gsize) else {
                return;
            };
            let mut e = entry(
                Outcome::Approximated,
                Area::Scene,
                &target.name,
                "group",
                "The group became separate items in the scene.",
            );
            if t_obs.crop != [0; 4] || t_obs.bounds_crop {
                e.reason
                    .push_str(" The group's crop, which clipped its items, is not kept.");
            }
            e.scenes = vec![scene_name.to_owned()];
            self.report.push(e);
            for child in target.items() {
                self.item(
                    &child,
                    scene_name,
                    Some((&gp, gsize, visible, locked)),
                    scene_ids,
                    out,
                );
            }
            return;
        }

        let (content, native, crop_extra, audio_only) = if target.is_scene() {
            if !scene_ids.contains_key(&t) {
                return;
            }
            (PendingContent::Scene(t), self.obs_canvas, [0; 4], false)
        } else {
            let Some(imp) = self.imported.get(&target.name).cloned() else {
                return; // The source's own entry says why, with this scene.
            };
            let native = imp.native.unwrap_or_else(|| {
                notes.push(
                    "The source's picture size is not known here, so its size on the canvas may differ."
                        .to_owned(),
                );
                self.obs_canvas
            });
            let region = imp.region.unwrap_or([0; 4]);
            let native = (
                native.0.saturating_sub(region[0] + region[2]).max(1),
                native.1.saturating_sub(region[1] + region[3]).max(1),
            );
            (
                PendingContent::Source(imp.id),
                native,
                region,
                imp.audio_only,
            )
        };

        let layout = if audio_only {
            full_canvas(self.obs_canvas)
        } else {
            let Some(mut p) = place(&t_obs, native) else {
                item_entry(
                    &mut self.report,
                    Outcome::Skipped,
                    "Its picture is cropped away completely.".to_owned(),
                );
                return;
            };
            if let Some((gp, gsize, _, _)) = group {
                p = compose(gp, gsize, &p);
            }
            let (mut layout, sheared) = layout_from_corners(&p);
            if sheared {
                notes.push("Its group stretches it in a way RBS cannot draw exactly.".to_owned());
            }
            if matches!(content, PendingContent::Scene(_)) {
                layout.crop = rbs_scene::model::Crop::default();
            } else {
                // OBS adds the item crop to the crop filter; RBS takes the larger inset
                // (ADR-0103 decision 2), so the item crop is written as their sum.
                let c = layout.crop;
                layout.crop = rbs_scene::model::Crop::new(
                    c.left.saturating_add(crop_extra[0]),
                    c.top.saturating_add(crop_extra[1]),
                    c.right.saturating_add(crop_extra[2]),
                    c.bottom.saturating_add(crop_extra[3]),
                );
            }
            if !layout.is_valid() {
                item_entry(
                    &mut self.report,
                    Outcome::Skipped,
                    "Its position or size is out of range.".to_owned(),
                );
                return;
            }
            layout
        };
        let blend = crate::obs::text(&item.raw, "blend_type");
        if !blend.is_empty() && blend != "normal" {
            notes.push(format!("Blending mode \"{blend}\" is not in RBS."));
        }
        if item
            .raw
            .get("show_transition")
            .and_then(|v| v.get("id"))
            .is_some()
            || item
                .raw
                .get("hide_transition")
                .and_then(|v| v.get("id"))
                .is_some()
        {
            notes.push("Show and hide transitions are not in RBS yet.".to_owned());
        }
        if notes.is_empty() {
            self.report.exact.items += 1;
        } else {
            item_entry(&mut self.report, Outcome::Approximated, notes.join(" "));
        }
        out.push(Pending {
            content,
            layout,
            visible,
            locked,
        });
    }

    /// Drops items that would close a cycle, nest too deep or pass the layer limit;
    /// returns the scene's depth and layers.
    fn limit(
        &mut self,
        si: usize,
        pending: &mut HashMap<usize, Vec<Pending>>,
        memo: &mut HashMap<usize, (usize, u64)>,
        on_stack: &mut Vec<usize>,
        global_layers: u64,
    ) -> (usize, u64) {
        if let Some(m) = memo.get(&si) {
            return *m;
        }
        on_stack.push(si);
        let items = pending.remove(&si).unwrap_or_default();
        let mut kept = Vec::with_capacity(items.len());
        let mut depth = 1usize;
        let mut layers = 0u64;
        let scene_name = self.all[si].name.clone();
        for p in items {
            let (d, l) = match p.content {
                PendingContent::Source(_) => (0, 1),
                PendingContent::Scene(c) => {
                    if on_stack.contains(&c) || on_stack.len() > MAX_NESTING_DEPTH {
                        let mut e = entry(
                            Outcome::Skipped,
                            Area::Item,
                            &self.all[c].name,
                            "scene",
                            "Nesting this scene here would loop or go deeper than RBS allows.",
                        );
                        e.scenes = vec![scene_name.clone()];
                        self.report.push(e);
                        continue;
                    }
                    self.limit(c, pending, memo, on_stack, global_layers)
                }
            };
            if d + 1 > MAX_NESTING_DEPTH {
                let mut e = entry(
                    Outcome::Skipped,
                    Area::Item,
                    "",
                    "scene",
                    "This nested scene goes deeper than RBS allows.",
                );
                e.scenes = vec![scene_name.clone()];
                self.report.push(e);
                continue;
            }
            if layers + l + global_layers > MAX_LAYERS {
                let mut e = entry(
                    Outcome::Skipped,
                    Area::Item,
                    "",
                    "",
                    "The scene has more layers than RBS draws.",
                );
                e.scenes = vec![scene_name.clone()];
                self.report.push(e);
                continue;
            }
            depth = depth.max(d + 1);
            layers += l;
            kept.push(p);
        }
        pending.insert(si, kept);
        on_stack.pop();
        memo.insert(si, (depth, layers));
        (depth, layers)
    }
}
