//! Contract tests for the scene binder (scene model spec, "Graph binding").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::Arc;

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_scene::SceneCollection;
use rbs_scene::binder::{BindError, BindProblem, Role, SceneBinder, resolve};
use rbs_scene::model::{
    CanvasId, ItemContent, Layout, SceneId, SceneScope, SettingValue, Settings, SourceId,
};

const AUDIO: PartitionId = PartitionId(7);

fn types(id: &NodeTypeId) -> Option<Arc<NodeDescriptor>> {
    let d = |kind, inputs, outputs| {
        Some(Arc::new(NodeDescriptor::new(
            id.clone(),
            kind,
            inputs,
            outputs,
        )))
    };
    match id.as_str() {
        "org.rbs.compositor" => d(
            NodeKind::Compositor,
            vec![Pin::many("layers", LinkKind::Video)],
            vec![Pin::one("video", LinkKind::Video)],
        ),
        "org.rbs.source.camera" | "org.rbs.source.screen" => d(
            NodeKind::Source,
            vec![],
            vec![Pin::one("video", LinkKind::Video)],
        ),
        "org.rbs.source.mic" => d(
            NodeKind::Source,
            vec![],
            vec![Pin::one("audio", LinkKind::Audio)],
        ),
        "org.rbs.source.camera-mic" => d(
            NodeKind::Source,
            vec![],
            vec![
                Pin::one("video", LinkKind::Video),
                Pin::one("audio", LinkKind::Audio),
            ],
        ),
        "org.rbs.audio.mixer" => d(
            NodeKind::AudioMixer,
            vec![Pin::many("in", LinkKind::Audio)],
            vec![Pin::one("main", LinkKind::Audio)],
        ),
        "org.rbs.filter.lut" | "org.rbs.filter.key" => d(
            NodeKind::VideoFilter,
            vec![Pin::one("in", LinkKind::Video)],
            vec![Pin::one("out", LinkKind::Video)],
        ),
        "org.rbs.audio-filter.gain" | "org.rbs.audio-filter.gate" => d(
            NodeKind::AudioFilter,
            vec![Pin::one("in", LinkKind::Audio)],
            vec![Pin::one("out", LinkKind::Audio)],
        ),
        "org.rbs.audio-filter.ducker" => d(
            NodeKind::AudioFilter,
            vec![
                Pin::one("in", LinkKind::Audio),
                Pin::one("key", LinkKind::Audio),
            ],
            vec![Pin::one("out", LinkKind::Audio)],
        ),
        _ => None,
    }
}

fn ty(s: &str) -> NodeTypeId {
    NodeTypeId::new(s)
}

fn full() -> Layout {
    Layout::new([0.0, 0.0], [100.0, 100.0])
}

struct Setup {
    col: SceneCollection,
    graph: GraphDescription,
    binder: SceneBinder,
    wide: CanvasId,
    tall: CanvasId,
    cam: SourceId,
    screen: SourceId,
    live: SceneId,
}

fn setup() -> Setup {
    let mut col = SceneCollection::new();
    let (wide, _) = col
        .add_canvas("Wide", Size::new(1920, 1080), FrameRate::FPS_60)
        .unwrap();
    let (tall, _) = col
        .add_canvas("Tall", Size::new(1080, 1920), FrameRate::FPS_30)
        .unwrap();
    let (cam, _) = col
        .add_source("Cam", ty("org.rbs.source.camera"), Settings::new())
        .unwrap();
    let (screen, _) = col
        .add_source("Screen", ty("org.rbs.source.screen"), Settings::new())
        .unwrap();
    let (live, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    col.add_item(live, ItemContent::Source(screen), full(), None)
        .unwrap();
    col.add_item(live, ItemContent::Source(cam), full(), None)
        .unwrap();
    col.set_program(wide, Some(live)).unwrap();
    Setup {
        col,
        graph: GraphDescription::new(),
        binder: SceneBinder::new(types),
        wide,
        tall,
        cam,
        screen,
        live,
    }
}

/// Attaches an output to a compositor so `plan()` schedules it.
fn attach_output(graph: &mut GraphDescription, comp: NodeId) -> NodeId {
    let out = graph
        .add_node(Arc::new(NodeDescriptor::new(
            ty("org.rbs.output.preview"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        )))
        .unwrap();
    graph.set_state(out, Lifecycle::Standby).unwrap();
    graph.set_state(out, Lifecycle::Active).unwrap();
    graph.connect(comp, "video", out, "in").unwrap();
    out
}

fn state(s: &Setup, id: NodeId) -> Lifecycle {
    s.graph.node(id).unwrap().state
}

#[test]
fn one_node_per_source_and_compositors_per_shown_scene() {
    let mut s = setup();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.problems.is_empty());
    let cam = s.binder.source_node(s.cam).unwrap();
    let screen = s.binder.source_node(s.screen).unwrap();
    let comp = s.binder.compositor(s.wide, Role::Program).unwrap();
    assert!(s.binder.compositor(s.tall, Role::Program).is_none());
    assert_eq!(state(&s, cam), Lifecycle::Active);
    assert_eq!(state(&s, comp), Lifecycle::Active);
    let layers = &report.compositors[&(s.wide, Role::Program)].layers;
    let producers: Vec<NodeId> = layers.iter().map(|l| l.producer).collect();
    assert_eq!(producers, [screen, cam]);

    // The same scene on the tall canvas reuses the same source nodes.
    s.col.set_program(s.tall, Some(s.live)).unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(s.binder.source_node(s.cam), Some(cam));
    let tall = s.binder.compositor(s.tall, Role::Program).unwrap();
    let sources = s
        .graph
        .nodes()
        .iter()
        .filter(|n| n.descriptor.kind == NodeKind::Source)
        .count();
    assert_eq!(sources, 2);
    assert_ne!(tall, comp);
}

#[test]
fn hidden_and_unused_sources_are_standby_and_ids_stay_stable() {
    let mut s = setup();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    let cam = s.binder.source_node(s.cam).unwrap();
    let item = s.col.scene(s.live).unwrap().items[1].id;
    s.col.set_item_visible(item, false).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(s.binder.source_node(s.cam), Some(cam));
    // Hiding keeps the device open, so showing the layer again does not wait.
    assert_eq!(state(&s, cam), Lifecycle::Active);
    // Its compositor link is gone, its layer too.
    let comp = s.binder.compositor(s.wide, Role::Program).unwrap();
    assert!(
        !s.graph
            .links()
            .iter()
            .any(|l| l.from.node == cam && l.to.node == comp)
    );
    assert_eq!(report.compositors[&(s.wide, Role::Program)].layers.len(), 1);

    s.col.set_item_visible(item, true).unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(state(&s, cam), Lifecycle::Active);

    // Deactivating releases it even while the item is visible.
    let source = s.col.source(s.cam).unwrap().clone();
    let mut hints = source.hints.clone();
    hints.insert(rbs_scene::STANDBY_HINT.to_owned(), "1".to_owned());
    s.col
        .set_source_settings_and_hints(s.cam, source.settings, hints)
        .unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(state(&s, cam), Lifecycle::Standby);
}

#[test]
fn filters_chain_in_order_and_follow_enable_and_removal() {
    let mut s = setup();
    let (lut, _) = s
        .col
        .add_filter(s.cam, ty("org.rbs.filter.lut"), Settings::new())
        .unwrap();
    let (key, _) = s
        .col
        .add_filter(s.cam, ty("org.rbs.filter.key"), Settings::new())
        .unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    let cam = s.binder.source_node(s.cam).unwrap();
    let lut_n = s.binder.filter_node(s.cam, lut).unwrap();
    let key_n = s.binder.filter_node(s.cam, key).unwrap();
    let linked = |s: &Setup, a: NodeId, b: NodeId| {
        s.graph
            .links()
            .iter()
            .any(|l| l.from.node == a && l.to.node == b)
    };
    assert!(linked(&s, cam, lut_n) && linked(&s, lut_n, key_n));
    let layer = &report.compositors[&(s.wide, Role::Program)].layers[1];
    assert_eq!(layer.producer, key_n);

    s.col.set_filter_enabled(s.cam, lut, false).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(linked(&s, cam, key_n) && !linked(&s, cam, lut_n));
    assert_eq!(state(&s, lut_n), Lifecycle::Standby);
    assert_eq!(state(&s, key_n), Lifecycle::Active);
    assert_eq!(
        report.compositors[&(s.wide, Role::Program)].layers[1].producer,
        key_n
    );

    s.col.remove_filter(s.cam, key).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(s.graph.node(key_n).is_none());
    assert_eq!(
        report.compositors[&(s.wide, Role::Program)].layers[1].producer,
        cam
    );
    let comp = s.binder.compositor(s.wide, Role::Program).unwrap();
    assert!(linked(&s, cam, comp));
}

#[test]
fn changed_filter_settings_keep_the_node_and_are_reported() {
    let mut s = setup();
    let (lut, _) = s
        .col
        .add_filter(s.cam, ty("org.rbs.filter.lut"), Settings::new())
        .unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(
        report.changed_filters.is_empty(),
        "a new filter is created, not changed"
    );
    let node = s.binder.filter_node(s.cam, lut).unwrap();
    let mut settings = Settings::new();
    settings.insert("amount".to_owned(), SettingValue::Float(0.5));
    s.col.set_filter_settings(s.cam, lut, settings).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(report.changed_filters, vec![(lut, node)]);
    assert!(report.created_filters.is_empty() && report.retired_filters.is_empty());
    assert_eq!(s.binder.filter_node(s.cam, lut), Some(node));
    // Unchanged on the next sync, also across disable and enable.
    assert!(
        s.binder
            .sync(&s.col, &mut s.graph)
            .unwrap()
            .changed_filters
            .is_empty()
    );
    s.col.set_filter_enabled(s.cam, lut, false).unwrap();
    s.col.set_filter_enabled(s.cam, lut, true).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.changed_filters.is_empty());
    assert_eq!(s.binder.filter_node(s.cam, lut), Some(node));
}

#[test]
fn deleting_sources_and_turning_studio_mode_off_removes_nodes() {
    let mut s = setup();
    s.col.set_preview(s.wide, Some(s.live)).unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    let preview = s.binder.compositor(s.wide, Role::Preview).unwrap();
    let cam = s.binder.source_node(s.cam).unwrap();
    s.col.delete_source(s.cam).unwrap();
    s.col.set_preview(s.wide, None).unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(s.graph.node(cam).is_none());
    assert!(s.graph.node(preview).is_none());
    assert!(s.binder.compositor(s.wide, Role::Preview).is_none());
}

#[test]
fn the_plan_schedules_exactly_what_the_program_needs() {
    let mut s = setup();
    let (idle, _) = s
        .col
        .add_source("Idle", ty("org.rbs.source.camera"), Settings::new())
        .unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    let comp = s.binder.compositor(s.wide, Role::Program).unwrap();
    let out = attach_output(&mut s.graph, comp);
    let plan = s.graph.plan();
    let cam = s.binder.source_node(s.cam).unwrap();
    let screen = s.binder.source_node(s.screen).unwrap();
    let mut order = plan.order.clone();
    order.sort();
    let mut expected = vec![cam, screen, comp, out];
    expected.sort();
    assert_eq!(order, expected);
    assert!(!plan.order.contains(&s.binder.source_node(idle).unwrap()));
}

#[test]
fn resolve_maps_layers_to_scheduled_link_indices() {
    let mut s = setup();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    let comp = s.binder.compositor(s.wide, Role::Program).unwrap();
    attach_output(&mut s.graph, comp);
    let bound = &report.compositors[&(s.wide, Role::Program)];
    let plan = s.graph.plan();
    let resolved: Vec<usize> = resolve(bound, &plan).iter().map(|(i, _)| *i).collect();
    assert_eq!(resolved, [0, 1]);

    // If a producer is not scheduled (say its prepare failed, so the controller's plan
    // leaves it out), its layer drops out and the others keep correct indices.
    let screen = s.binder.source_node(s.screen).unwrap();
    s.graph.set_state(screen, Lifecycle::Standby).unwrap();
    let plan = s.graph.plan();
    let resolved: Vec<(usize, SourceId)> = resolve(bound, &plan)
        .iter()
        .map(|(i, l)| (*i, l.source))
        .collect();
    assert_eq!(resolved, [(0, s.cam)]);
}

#[test]
fn links_the_binder_did_not_make_are_left_alone() {
    let mut s = setup();
    s.binder = SceneBinder::new(types_with_background);
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    let comp = s.binder.compositor(s.wide, Role::Program).unwrap();
    attach_output(&mut s.graph, comp);
    // The application links a background into the compositor's other video pin.
    let bg = s
        .graph
        .add_node(Arc::new(NodeDescriptor::new(
            ty("org.rbs.source.camera"),
            NodeKind::Source,
            vec![],
            vec![Pin::one("video", LinkKind::Video)],
        )))
        .unwrap();
    s.graph.set_state(bg, Lifecycle::Standby).unwrap();
    s.graph.set_state(bg, Lifecycle::Active).unwrap();
    s.graph.connect(bg, "video", comp, "background").unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(
        s.graph
            .links()
            .iter()
            .any(|l| l.from.node == bg && l.to.node == comp)
    );
    // resolve only counts the layers pin.
    let plan = s.graph.plan();
    let bound = &report.compositors[&(s.wide, Role::Program)];
    assert_eq!(bound.layers_pin, 1);
    let resolved: Vec<usize> = resolve(bound, &plan).iter().map(|(i, _)| *i).collect();
    assert_eq!(resolved, [0, 1]);
}

fn types_with_background(id: &NodeTypeId) -> Option<Arc<NodeDescriptor>> {
    if id.as_str() == "org.rbs.compositor" {
        return Some(Arc::new(NodeDescriptor::new(
            id.clone(),
            NodeKind::Compositor,
            vec![
                Pin::one("background", LinkKind::Video),
                Pin::many("layers", LinkKind::Video),
            ],
            vec![Pin::one("video", LinkKind::Video)],
        )));
    }
    types(id)
}

#[test]
fn a_source_faded_to_zero_stays_linked_and_scheduled() {
    let mut s = setup();
    let item = s.col.scene(s.live).unwrap().items[1].id;
    s.col
        .set_item_layout(item, None, full().with_opacity(0.0))
        .unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    let comp = s.binder.compositor(s.wide, Role::Program).unwrap();
    attach_output(&mut s.graph, comp);
    let cam = s.binder.source_node(s.cam).unwrap();
    assert_eq!(report.compositors[&(s.wide, Role::Program)].layers.len(), 1);
    assert!(s.graph.plan().order.contains(&cam));
    // Fading in does not change the graph's links.
    let links = s.graph.links().to_vec();
    s.col.set_item_layout(item, None, full()).unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(s.graph.links(), links.as_slice());
}

#[test]
fn nodes_removed_by_someone_else_are_bound_again() {
    let mut s = setup();
    let (lut, _) = s
        .col
        .add_filter(s.cam, ty("org.rbs.filter.lut"), Settings::new())
        .unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    let cam = s.binder.source_node(s.cam).unwrap();
    s.graph.set_state(cam, Lifecycle::Standby).unwrap();
    s.graph.remove_node(cam).unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    let again = s.binder.source_node(s.cam).unwrap();
    assert_ne!(again, cam);
    let f = s.binder.filter_node(s.cam, lut).unwrap();
    assert!(
        s.graph
            .links()
            .iter()
            .any(|l| l.from.node == again && l.to.node == f)
    );
    // No orphan filter nodes left behind.
    let filters = s
        .graph
        .nodes()
        .iter()
        .filter(|n| n.descriptor.kind == NodeKind::VideoFilter)
        .count();
    assert_eq!(filters, 1);
}

#[test]
fn created_and_retired_compositors_are_reported() {
    let mut s = setup();
    s.col.set_preview(s.wide, Some(s.live)).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.compositors[&(s.wide, Role::Preview)].created);
    let preview = report.compositors[&(s.wide, Role::Preview)].node;
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(!report.compositors[&(s.wide, Role::Preview)].created);
    s.col.set_preview(s.wide, None).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(report.retired_compositors, [preview]);
}

#[test]
fn unknown_types_are_reported_and_left_out() {
    let mut s = setup();
    let (ghost, _) = s
        .col
        .add_source("Ghost", ty("org.example.ghost"), Settings::new())
        .unwrap();
    let (mic, _) = s
        .col
        .add_source("Mic", ty("org.rbs.source.mic"), Settings::new())
        .unwrap();
    let (f, _) = s
        .col
        .add_filter(s.cam, ty("org.example.fx"), Settings::new())
        .unwrap();
    s.col
        .add_item(s.live, ItemContent::Source(ghost), full(), None)
        .unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.problems.contains(&BindProblem::UnknownSourceType(
        ghost,
        ty("org.example.ghost")
    )));
    assert!(report.problems.contains(&BindProblem::NoVideoOutput(mic)));
    assert!(
        report
            .problems
            .contains(&BindProblem::UnknownFilterType(f, ty("org.example.fx")))
    );
    assert_eq!(report.compositors[&(s.wide, Role::Program)].layers.len(), 2);
    assert!(s.binder.source_node(ghost).is_none());
}

#[test]
fn missing_compositor_type_changes_nothing() {
    let s = setup();
    let mut binder = SceneBinder::new(|_: &NodeTypeId| None);
    let mut graph = GraphDescription::new();
    assert_eq!(
        binder.sync(&s.col, &mut graph),
        Err(BindError::NoCompositor)
    );
    assert!(graph.nodes().is_empty());
}

#[test]
fn sync_is_idempotent() {
    let mut s = setup();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    let before = (s.graph.nodes().len(), s.graph.links().to_vec());
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!((s.graph.nodes().len(), s.graph.links().to_vec()), before);
}

fn audio_setup() -> (Setup, SourceId) {
    let mut s = setup();
    s.binder = SceneBinder::new(types).with_audio(AUDIO);
    let (mic, _) = s
        .col
        .add_source("Mic", ty("org.rbs.source.mic"), Settings::new())
        .unwrap();
    s.col
        .add_item(s.live, ItemContent::Source(mic), full(), None)
        .unwrap();
    (s, mic)
}

fn links_into(s: &Setup, node: NodeId) -> Vec<NodeId> {
    s.graph
        .links()
        .iter()
        .filter(|l| l.to.node == node)
        .map(|l| l.from.node)
        .collect()
}

#[test]
fn audio_sources_feed_the_mixer_from_program_scenes_only() {
    let (mut s, mic) = audio_setup();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    let mixer = s.binder.mixer(s.wide).unwrap();
    let mic_node = s.binder.source_node(mic).unwrap();
    let bound = report.mixers[&s.wide].clone();
    assert!(bound.created);
    assert_eq!(bound.node, mixer);
    assert_eq!(bound.inputs.len(), 1);
    assert_eq!(bound.inputs[0].source, mic);
    assert_eq!(bound.inputs[0].producer, mic_node);
    assert_eq!(links_into(&s, mixer), [mic_node]);
    // Audio-only nodes and the mixer run in the audio partition; video stays on render.
    assert_eq!(s.graph.node(mic_node).unwrap().partition, AUDIO);
    assert_eq!(s.graph.node(mixer).unwrap().partition, AUDIO);
    let cam = s.binder.source_node(s.cam).unwrap();
    assert_eq!(s.graph.node(cam).unwrap().partition, PartitionId::RENDER);
    assert_eq!(state(&s, mic_node), Lifecycle::Active);
    assert_eq!(state(&s, mixer), Lifecycle::Active);
    // The microphone is not a layer of the compositor.
    let comp = s.binder.compositor(s.wide, Role::Program).unwrap();
    assert!(!links_into(&s, comp).contains(&mic_node));
    let program = &report.compositors[&(s.wide, Role::Program)];
    assert!(program.layers.iter().all(|l| l.source != mic));

    // Shown only in the preview scene: wanted Active, but not on air (no mixer link,
    // so the plan does not schedule it).
    let (rehearsal, _) = s.col.add_scene("Rehearsal", SceneScope::Shared).unwrap();
    s.col
        .add_item(rehearsal, ItemContent::Source(mic), full(), None)
        .unwrap();
    let items: Vec<_> = s
        .col
        .scene(s.live)
        .unwrap()
        .items
        .iter()
        .filter(|i| i.content == ItemContent::Source(mic))
        .map(|i| i.id)
        .collect();
    s.col.remove_item(items[0]).unwrap();
    s.col.set_preview(s.wide, Some(rehearsal)).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(links_into(&s, mixer).is_empty());
    assert!(report.mixers[&s.wide].inputs.is_empty());
    assert!(!report.created_sources.iter().any(|(id, _)| *id == mic));
    assert_eq!(s.binder.source_node(mic), Some(mic_node));

    // Hidden or unused: Standby and unlinked.
    s.col.set_preview(s.wide, None).unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(state(&s, mic_node), Lifecycle::Standby);
    assert!(links_into(&s, mixer).is_empty());
}

#[test]
fn each_canvas_mixes_only_its_own_program() {
    let (mut s, mic) = audio_setup();
    let (sys, _) = s
        .col
        .add_source("System", ty("org.rbs.source.mic"), Settings::new())
        .unwrap();
    let (vertical, _) = s.col.add_scene("Vertical", SceneScope::Shared).unwrap();
    s.col
        .add_item(vertical, ItemContent::Source(sys), full(), None)
        .unwrap();
    s.col.set_program(s.tall, Some(vertical)).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    let wide = s.binder.mixer(s.wide).unwrap();
    let tall = s.binder.mixer(s.tall).unwrap();
    assert_ne!(wide, tall);
    assert_eq!(links_into(&s, wide), [s.binder.source_node(mic).unwrap()]);
    assert_eq!(links_into(&s, tall), [s.binder.source_node(sys).unwrap()]);
    assert_eq!(report.mixers.len(), 2);
    // A canvas without a program scene still has a (silent) mixer; removing the
    // canvas retires it.
    s.col.set_program(s.tall, None).unwrap();
    s.col.remove_canvas(s.tall).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(report.retired_mixers, [tall]);
    assert!(s.graph.node(tall).is_none());
    assert!(s.binder.mixer(s.tall).is_none());
}

#[test]
fn a_source_with_video_and_audio_binds_only_its_video() {
    let (mut s, _) = audio_setup();
    let (both, _) = s
        .col
        .add_source("Cam+mic", ty("org.rbs.source.camera-mic"), Settings::new())
        .unwrap();
    s.col
        .add_item(s.live, ItemContent::Source(both), full(), None)
        .unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(report.problems, [BindProblem::AudioOnVideoSource(both)]);
    let node = s.binder.source_node(both).unwrap();
    assert_eq!(s.graph.node(node).unwrap().partition, PartitionId::RENDER);
    let comp = s.binder.compositor(s.wide, Role::Program).unwrap();
    assert!(links_into(&s, comp).contains(&node));
    let mixer = s.binder.mixer(s.wide).unwrap();
    assert!(!links_into(&s, mixer).contains(&node));
}

#[test]
fn a_video_filter_on_an_audio_source_is_reported() {
    let (mut s, mic) = audio_setup();
    let (f, _) = s
        .col
        .add_filter(mic, ty("org.rbs.filter.lut"), Settings::new())
        .unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(report.problems, [BindProblem::NoAudioPins(f)]);
    assert!(s.binder.filter_node(mic, f).is_none());
}

#[test]
fn an_audio_filter_on_a_video_source_is_reported() {
    let mut s = setup();
    let (f, _) = s
        .col
        .add_filter(s.cam, ty("org.rbs.audio-filter.gain"), Settings::new())
        .unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(report.problems, [BindProblem::NoVideoPins(f)]);
}

#[test]
fn audio_filters_chain_in_the_sources_partition_and_the_mixer_hears_the_tail() {
    let (mut s, mic) = audio_setup();
    let (f1, _) = s
        .col
        .add_filter(mic, ty("org.rbs.audio-filter.gain"), Settings::new())
        .unwrap();
    let (f2, _) = s
        .col
        .add_filter(mic, ty("org.rbs.audio-filter.gate"), Settings::new())
        .unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    let mic_node = s.binder.source_node(mic).unwrap();
    let n1 = s.binder.filter_node(mic, f1).unwrap();
    let n2 = s.binder.filter_node(mic, f2).unwrap();
    for n in [n1, n2] {
        assert_eq!(s.graph.node(n).unwrap().partition, AUDIO);
    }
    assert_eq!(links_into(&s, n1), [mic_node]);
    assert_eq!(links_into(&s, n2), [n1]);
    let mixer = s.binder.mixer(s.wide).unwrap();
    assert_eq!(links_into(&s, mixer), [n2], "the mixer hears the tail only");
    let bound = &report.mixers[&s.wide];
    assert_eq!(bound.inputs.len(), 1);
    assert_eq!(bound.inputs[0].producer, n2);
}

#[test]
fn changing_the_audio_chain_moves_the_mixer_link_to_the_new_tail() {
    let (mut s, mic) = audio_setup();
    let (f, _) = s
        .col
        .add_filter(mic, ty("org.rbs.audio-filter.gain"), Settings::new())
        .unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    let mic_node = s.binder.source_node(mic).unwrap();
    let node = s.binder.filter_node(mic, f).unwrap();
    let mixer = s.binder.mixer(s.wide).unwrap();
    assert_eq!(links_into(&s, mixer), [node]);
    // Disabled: the filter stays in the graph but the source feeds the mixer again.
    s.col.set_filter_enabled(mic, f, false).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(links_into(&s, mixer), [mic_node]);
    assert_eq!(report.mixers[&s.wide].inputs[0].producer, mic_node);
    assert!(s.binder.filter_node(mic, f).is_some());
    // Enabled again, then removed: the node is retired with its links, the mixer link
    // goes back to the source.
    s.col.set_filter_enabled(mic, f, true).unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(links_into(&s, mixer), [node]);
    s.col.remove_filter(mic, f).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(report.retired_filters, [node]);
    assert!(s.graph.node(node).is_none());
    assert_eq!(links_into(&s, mixer), [mic_node]);
}

#[test]
fn without_audio_microphones_are_left_out() {
    let (mut s, mic) = audio_setup();
    s.binder = SceneBinder::new(types);
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(report.problems, [BindProblem::NoVideoOutput(mic)]);
    assert!(report.mixers.is_empty());
    assert!(s.binder.mixer(s.wide).is_none());
}

#[test]
fn missing_mixer_type_changes_nothing() {
    let s = setup();
    let no_mixer = |id: &NodeTypeId| {
        if id.as_str() == "org.rbs.audio.mixer" {
            None
        } else {
            types(id)
        }
    };
    let mut binder = SceneBinder::new(no_mixer).with_audio(AUDIO);
    let mut graph = GraphDescription::new();
    assert_eq!(binder.sync(&s.col, &mut graph), Err(BindError::NoMixer));
    assert!(graph.nodes().is_empty());
}

#[test]
fn changed_settings_give_the_source_a_new_node_and_keep_its_filters() {
    let (mut s, _) = audio_setup();
    let (f, _) = s
        .col
        .add_filter(s.cam, ty("org.rbs.filter.lut"), Settings::new())
        .unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(report.created_sources.len(), 3);
    assert_eq!(report.created_filters.len(), 1);
    let old = s.binder.source_node(s.cam).unwrap();
    let old_filter = s.binder.filter_node(s.cam, f).unwrap();
    let comp = s.binder.compositor(s.wide, Role::Program).unwrap();

    let mut settings = Settings::new();
    settings.insert("target".into(), SettingValue::Text("cam-2".into()));
    s.col.set_source_settings(s.cam, settings).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    let new = s.binder.source_node(s.cam).unwrap();
    let new_filter = s.binder.filter_node(s.cam, f).unwrap();
    assert_ne!(new, old);
    assert_eq!(report.retired_sources, [old]);
    assert_eq!(report.created_sources, [(s.cam, new)]);
    assert_eq!(report.replaced_sources, [(s.cam, old, new)]);
    assert!(report.retired_filters.is_empty() && report.created_filters.is_empty());
    assert_eq!(new_filter, old_filter);
    assert_eq!(state(&s, new), Lifecycle::Active);
    assert!(s.graph.node(old).is_none());
    assert!(links_into(&s, comp).contains(&new_filter));
    assert!(links_into(&s, new_filter).contains(&new));

    // Same settings again: nothing changes.
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.created_sources.is_empty() && report.retired_sources.is_empty());
}

#[test]
fn deleted_sources_and_filters_are_reported_retired() {
    let (mut s, mic) = audio_setup();
    let (f, _) = s
        .col
        .add_filter(s.cam, ty("org.rbs.filter.lut"), Settings::new())
        .unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    let cam = s.binder.source_node(s.cam).unwrap();
    let filter = s.binder.filter_node(s.cam, f).unwrap();
    let mic_node = s.binder.source_node(mic).unwrap();
    s.col.delete_source(s.cam).unwrap();
    s.col.delete_source(mic).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    let mut retired = report.retired_sources.clone();
    retired.sort();
    let mut expected = vec![cam, mic_node];
    expected.sort();
    assert_eq!(retired, expected);
    assert_eq!(report.retired_filters, [filter]);
    let mixer = s.binder.mixer(s.wide).unwrap();
    assert!(links_into(&s, mixer).is_empty());
}

#[test]
fn nodes_removed_by_someone_else_are_reported_retired() {
    let (mut s, _) = audio_setup();
    let (f, _) = s
        .col
        .add_filter(s.cam, ty("org.rbs.filter.lut"), Settings::new())
        .unwrap();
    let (g, _) = s
        .col
        .add_filter(s.screen, ty("org.rbs.filter.lut"), Settings::new())
        .unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    let cam = s.binder.source_node(s.cam).unwrap();
    let cam_filter = s.binder.filter_node(s.cam, f).unwrap();
    let screen_filter = s.binder.filter_node(s.screen, g).unwrap();
    for id in [cam, screen_filter] {
        s.graph.set_state(id, Lifecycle::Standby).unwrap();
        s.graph.remove_node(id).unwrap();
    }
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(report.retired_sources, [cam]);
    let mut retired = report.retired_filters.clone();
    retired.sort();
    let mut expected = vec![cam_filter, screen_filter];
    expected.sort();
    assert_eq!(retired, expected);
    assert_eq!(report.created_sources.len(), 1);
}

#[test]
fn a_mixer_removed_by_someone_else_is_bound_again() {
    let (mut s, mic) = audio_setup();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    let old = s.binder.mixer(s.wide).unwrap();
    s.graph.set_state(old, Lifecycle::Standby).unwrap();
    s.graph.remove_node(old).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    let bound = report.mixers[&s.wide].clone();
    assert!(bound.created);
    assert_ne!(bound.node, old);
    let mic_node = s.binder.source_node(mic).unwrap();
    assert_eq!(links_into(&s, bound.node), [mic_node]);
}

#[test]
fn sync_with_audio_is_idempotent() {
    let (mut s, _) = audio_setup();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    let snapshot = |g: &GraphDescription| {
        let nodes: Vec<_> = g
            .nodes()
            .iter()
            .map(|n| (n.id, n.state, n.partition))
            .collect();
        (nodes, g.links().to_vec())
    };
    let before = snapshot(&s.graph);
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(snapshot(&s.graph), before);
    assert!(report.mixers.values().all(|m| !m.created));
    assert!(report.created_sources.is_empty() && report.created_filters.is_empty());
}

struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) % n.max(1) as u64) as usize
    }
}

/// After any sequence of edits, sync leaves the graph consistent with the collection.
#[test]
fn random_edits_keep_the_graph_in_step() {
    random_edits(false);
}

/// The same with audio bound: mixer links, partitions and created/retired reports too.
#[test]
fn random_edits_with_audio_keep_the_graph_in_step() {
    random_edits(true);
}

fn random_edits(audio: bool) {
    for seed in 0..30_u64 {
        let mut rng = Lcg(seed);
        let mut s = setup();
        if audio {
            s.binder = SceneBinder::new(types).with_audio(AUDIO);
        }
        let kinds = [
            "org.rbs.source.camera",
            "org.rbs.source.screen",
            "org.example.ghost",
            "org.rbs.source.mic",
            "org.rbs.source.camera-mic",
        ];
        // A global scene on the wide canvas (ADR-0065), switched on and off below.
        let (globals, _) = s
            .col
            .add_scene("Globals", SceneScope::Canvas(s.wide))
            .unwrap();
        s.col.set_globals(s.wide, Some(globals)).unwrap();
        // Instances the application would hold, from the reports alone.
        let mut instances = std::collections::BTreeSet::new();
        for step in 0..80 {
            let sources: Vec<SourceId> = s.col.sources().map(|x| x.id).collect();
            let scenes: Vec<SceneId> = s.col.scenes().map(|x| x.id).collect();
            let items: Vec<_> = s
                .col
                .scenes()
                .flat_map(|x| x.items.iter().map(|i| i.id))
                .collect();
            let pick_src = sources.get(rng.below(sources.len())).copied();
            let pick_scene = scenes.get(rng.below(scenes.len())).copied();
            let pick_item = items.get(rng.below(items.len())).copied();
            let _ = match rng.below(17) {
                // The global scene on and off, and a source placed in it.
                15 => {
                    let on = s.col.globals_of(s.wide).is_none();
                    s.col.set_globals(s.wide, on.then_some(globals)).map(|_| ())
                }
                16 => pick_src.map_or(Ok(()), |src| {
                    s.col
                        .add_item(globals, ItemContent::Source(src), full(), None)
                        .map(|_| ())
                }),
                // Someone else removes a source or filter node.
                14 => {
                    let nodes: Vec<NodeId> = s
                        .col
                        .sources()
                        .flat_map(|x| {
                            let filters = x.filters.iter().map(|f| (x.id, Some(f.id)));
                            std::iter::once((x.id, None)).chain(filters)
                        })
                        .filter_map(|(src, f)| match f {
                            None => s.binder.source_node(src),
                            Some(f) => s.binder.filter_node(src, f),
                        })
                        .collect();
                    if let Some(&id) = nodes.get(rng.below(nodes.len())) {
                        let st = s.graph.node(id).unwrap().state;
                        if st == Lifecycle::Active {
                            s.graph.set_state(id, Lifecycle::Standby).unwrap();
                        }
                        s.graph.remove_node(id).unwrap();
                    }
                    Ok(())
                }
                13 => pick_src.map_or(Ok(()), |x| {
                    let mut settings = Settings::new();
                    settings.insert(
                        "target".into(),
                        SettingValue::Text(format!("t{}", rng.below(2))),
                    );
                    s.col.set_source_settings(x, settings).map(|_| ())
                }),
                9 => {
                    let f = pick_src.and_then(|x| {
                        s.col
                            .source(x)
                            .and_then(|src| src.filters.last().map(|f| (x, f.id)))
                    });
                    f.map_or(Ok(()), |(x, f)| s.col.remove_filter(x, f).map(|_| ()))
                }
                10 => {
                    let f = pick_src.and_then(|x| {
                        s.col
                            .source(x)
                            .and_then(|src| src.filters.last().map(|f| (x, f.id)))
                    });
                    f.map_or(Ok(()), |(x, f)| s.col.move_filter(x, f, 0).map(|_| ()))
                }
                11 => pick_item.map_or(Ok(()), |i| {
                    let o = if rng.below(2) == 0 { 0.0 } else { 1.0 };
                    s.col
                        .set_item_layout(i, None, full().with_opacity(o))
                        .map(|_| ())
                }),
                12 => s
                    .col
                    .add_filter(
                        pick_src.unwrap_or(s.cam),
                        ty(["org.rbs.filter.key", "org.example.fx"][rng.below(2)]),
                        Settings::new(),
                    )
                    .map(|_| ()),
                0 => s
                    .col
                    .add_source(
                        &format!("S{seed}-{step}"),
                        ty(kinds[rng.below(if audio { 5 } else { 3 })]),
                        Settings::new(),
                    )
                    .map(|_| ()),
                1 => pick_src.map_or(Ok(()), |x| s.col.delete_source(x).map(|_| ())),
                2 => match (pick_scene, pick_src) {
                    (Some(sc), Some(src)) => s
                        .col
                        .add_item(sc, ItemContent::Source(src), full(), None)
                        .map(|_| ()),
                    _ => Ok(()),
                },
                3 => pick_item.map_or(Ok(()), |i| {
                    s.col.set_item_visible(i, rng.below(2) == 0).map(|_| ())
                }),
                4 => pick_src.map_or(Ok(()), |x| {
                    s.col
                        .add_filter(x, ty("org.rbs.filter.lut"), Settings::new())
                        .map(|_| ())
                }),
                5 => {
                    let f = pick_src.and_then(|x| {
                        s.col
                            .source(x)
                            .and_then(|src| src.filters.first().map(|f| (x, f.id, f.enabled)))
                    });
                    f.map_or(Ok(()), |(x, f, on)| {
                        s.col.set_filter_enabled(x, f, !on).map(|_| ())
                    })
                }
                6 => s
                    .col
                    .add_scene(&format!("Sc{seed}-{step}"), SceneScope::Shared)
                    .map(|_| ()),
                7 => s.col.set_program(s.tall, pick_scene).map(|_| ()),
                _ => s.col.set_preview(s.wide, pick_scene).map(|_| ()),
            };
            let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
            for node in &report.retired_sources {
                assert!(
                    instances.remove(node),
                    "seed {seed} step {step}: retired twice"
                );
            }
            for (_, node) in &report.created_sources {
                assert!(
                    instances.insert(*node),
                    "seed {seed} step {step}: created twice"
                );
            }
            let bound: std::collections::BTreeSet<NodeId> = s
                .col
                .sources()
                .filter_map(|x| s.binder.source_node(x.id))
                .collect();
            assert_eq!(
                instances, bound,
                "seed {seed} step {step}: reported sources"
            );
            check_consistent(&s, &report, seed, step);
        }
    }
}

fn has_video(s: &Setup, src: SourceId) -> bool {
    let t = s.col.source(src).map(|x| x.type_id.as_str().to_owned());
    !matches!(t.as_deref(), Some("org.rbs.source.mic"))
}

fn has_audio(s: &Setup, src: SourceId) -> bool {
    let t = s.col.source(src).map(|x| x.type_id.as_str().to_owned());
    matches!(
        t.as_deref(),
        Some("org.rbs.source.mic" | "org.rbs.source.camera-mic")
    )
}

/// The sources of the visible items of a canvas's global scene, read from the items.
fn direct_globals(s: &Setup, canvas: rbs_scene::model::CanvasId) -> Vec<SourceId> {
    s.col
        .globals_of(canvas)
        .and_then(|g| s.col.scene(g))
        .map(|g| {
            g.items
                .iter()
                .filter(|i| i.visible)
                .filter_map(|i| match i.content {
                    ItemContent::Source(src) => Some(src),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Checks the graph against the collection without using the binder's own report for
/// the expectations.
fn check_consistent(s: &Setup, report: &rbs_scene::binder::BindReport, seed: u64, step: usize) {
    let wanted = s.col.wanted_sources();
    let mut accounted = std::collections::BTreeSet::new();
    let tail_of = |src: SourceId| -> Option<NodeId> {
        if !has_video(s, src) {
            return None;
        }
        let source = s.col.source(src)?;
        let node = s.binder.source_node(src)?;
        Some(
            source
                .filters
                .iter()
                .rev()
                .filter(|f| f.enabled)
                .find_map(|f| s.binder.filter_node(src, f.id))
                .unwrap_or(node),
        )
    };
    for src in s.col.sources() {
        let Some(node) = s.binder.source_node(src.id) else {
            continue;
        };
        accounted.insert(node);
        let expect = if wanted.contains(&src.id) {
            Lifecycle::Active
        } else {
            Lifecycle::Standby
        };
        assert_eq!(
            state(s, node),
            expect,
            "seed {seed} step {step}: source state"
        );
        let partition = s.graph.node(node).unwrap().partition;
        let expect_partition = if has_video(s, src.id) {
            PartitionId::RENDER
        } else {
            AUDIO
        };
        assert_eq!(
            partition, expect_partition,
            "seed {seed} step {step}: partition"
        );
        // The chain is exactly source -> f1 -> ... -> fn over enabled, bound filters.
        let mut chain = vec![node];
        for f in &src.filters {
            if let Some(fnode) = s.binder.filter_node(src.id, f.id) {
                accounted.insert(fnode);
                let fs = if f.enabled {
                    expect
                } else {
                    Lifecycle::Standby
                };
                assert_eq!(state(s, fnode), fs, "seed {seed} step {step}: filter state");
                if f.enabled {
                    chain.push(fnode);
                }
            }
        }
        let mut expected_links: Vec<(NodeId, NodeId)> =
            chain.windows(2).map(|w| (w[0], w[1])).collect();
        let mut actual: Vec<(NodeId, NodeId)> = s
            .graph
            .links()
            .iter()
            .filter(|l| chain.contains(&l.to.node) && l.to.node != node)
            .map(|l| (l.from.node, l.to.node))
            .collect();
        expected_links.sort();
        actual.sort();
        assert_eq!(
            actual, expected_links,
            "seed {seed} step {step}: chain links"
        );
        // Disabled filters have no links into them.
        for f in src.filters.iter().filter(|f| !f.enabled) {
            if let Some(fnode) = s.binder.filter_node(src.id, f.id) {
                assert!(!s.graph.links().iter().any(|l| l.to.node == fnode));
            }
        }
    }
    for canvas in s.col.canvases() {
        for (role, scene) in [
            (Role::Program, canvas.program),
            (Role::Preview, canvas.preview),
        ] {
            let comp = s.binder.compositor(canvas.id, role);
            let Some(scene) = scene else {
                assert!(comp.is_none());
                continue;
            };
            let comp = comp.unwrap();
            accounted.insert(comp);
            assert!(report.compositors.contains_key(&(canvas.id, role)));
            let mut expected: Vec<NodeId> = s
                .col
                .visible_sources(canvas.id, scene)
                .unwrap()
                .into_iter()
                .filter_map(tail_of)
                .collect();
            // Global sources (ADR-0065) are on every compositor of their canvas,
            // checked from the global scene's items directly.
            for src in direct_globals(s, canvas.id) {
                if let Some(t) = tail_of(src) {
                    assert!(
                        expected.contains(&t),
                        "seed {seed} step {step}: global source not drawn"
                    );
                }
            }
            expected.sort();
            expected.dedup();
            let mut linked: Vec<NodeId> = s
                .graph
                .links()
                .iter()
                .filter(|l| l.to.node == comp)
                .map(|l| l.from.node)
                .collect();
            linked.sort();
            assert_eq!(
                linked, expected,
                "seed {seed} step {step}: compositor links"
            );
        }
    }
    for canvas in s.col.canvases() {
        let Some(mixer) = s.binder.mixer(canvas.id) else {
            continue;
        };
        accounted.insert(mixer);
        assert_eq!(state(s, mixer), Lifecycle::Active);
        assert_eq!(s.graph.node(mixer).unwrap().partition, AUDIO);
        let mut expected: Vec<NodeId> = canvas
            .program
            .map(|p| s.col.visible_sources(canvas.id, p).unwrap())
            .into_iter()
            .flatten()
            .filter(|src| has_audio(s, *src) && !has_video(s, *src))
            .filter_map(|src| s.binder.source_node(src))
            .collect();
        if canvas.program.is_some() {
            for src in direct_globals(s, canvas.id) {
                if has_audio(s, src) && !has_video(s, src) {
                    let node = s.binder.source_node(src);
                    assert!(
                        node.is_none_or(|n| expected.contains(&n)),
                        "seed {seed} step {step}: global sound not mixed"
                    );
                }
            }
        }
        expected.sort();
        expected.dedup();
        let mut linked: Vec<NodeId> = s
            .graph
            .links()
            .iter()
            .filter(|l| l.to.node == mixer)
            .map(|l| l.from.node)
            .collect();
        linked.sort();
        assert_eq!(linked, expected, "seed {seed} step {step}: mixer links");
        let mut reported: Vec<NodeId> = report.mixers[&canvas.id]
            .inputs
            .iter()
            .map(|i| i.producer)
            .collect();
        reported.sort();
        assert_eq!(reported, expected, "seed {seed} step {step}: mixer inputs");
    }
    let all: std::collections::BTreeSet<NodeId> = s.graph.nodes().iter().map(|n| n.id).collect();
    assert_eq!(all, accounted, "seed {seed} step {step}: unaccounted nodes");
}

// Key links (ADR-0095).

fn keyed(key: Option<SourceId>) -> Settings {
    let mut settings = Settings::new();
    if let Some(k) = key {
        settings.insert(
            "key_source".to_owned(),
            rbs_scene::model::SettingValue::Int(i64::from(k.get())),
        );
    }
    settings
}

/// A scene with a microphone `voice` and a music source `music`, both heard.
fn music_and_voice() -> (Setup, SourceId, SourceId) {
    let (mut s, voice) = audio_setup();
    let (music, _) = s
        .col
        .add_source("Music", ty("org.rbs.source.mic"), Settings::new())
        .unwrap();
    s.col
        .add_item(s.live, ItemContent::Source(music), full(), None)
        .unwrap();
    (s, music, voice)
}

#[test]
fn a_ducker_hears_the_tail_of_its_key_sources_chain() {
    let (mut s, music, voice) = music_and_voice();
    let (d, _) = s
        .col
        .add_filter(music, ty("org.rbs.audio-filter.ducker"), keyed(Some(voice)))
        .unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    let duck = s.binder.filter_node(music, d).unwrap();
    let voice_node = s.binder.source_node(voice).unwrap();
    let music_node = s.binder.source_node(music).unwrap();
    let mut into = links_into(&s, duck);
    into.sort();
    let mut want = vec![music_node, voice_node];
    want.sort();
    assert_eq!(into, want, "the music on `in`, the voice on `key`");
    // The mixer still hears the music's chain tail (the ducker) and the voice itself.
    let mixer = s.binder.mixer(s.wide).unwrap();
    let mut heard = links_into(&s, mixer);
    heard.sort();
    let mut want = vec![duck, voice_node];
    want.sort();
    assert_eq!(heard, want);
    // A gate on the voice becomes the key tap, and the link moves with it.
    let (g, _) = s
        .col
        .add_filter(voice, ty("org.rbs.audio-filter.gate"), Settings::new())
        .unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    let gate = s.binder.filter_node(voice, g).unwrap();
    assert!(links_into(&s, duck).contains(&gate));
    assert!(!links_into(&s, duck).contains(&voice_node));
    // Without a key setting there is no key link.
    s.col.set_filter_settings(music, d, keyed(None)).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.problems.is_empty());
    assert_eq!(links_into(&s, duck), [music_node]);
}

#[test]
fn a_missing_own_or_video_key_is_reported_and_left_unlinked() {
    let (mut s, music, voice) = music_and_voice();
    let (own, _) = s
        .col
        .add_filter(music, ty("org.rbs.audio-filter.ducker"), keyed(Some(music)))
        .unwrap();
    let (gone, _) = s
        .col
        .add_filter(
            voice,
            ty("org.rbs.audio-filter.ducker"),
            keyed(Some(SourceId::from_raw(999))),
        )
        .unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.problems.contains(&BindProblem::KeyUnavailable(own)));
    assert!(report.problems.contains(&BindProblem::KeyUnavailable(gone)));
    for (src, f) in [(music, own), (voice, gone)] {
        let node = s.binder.filter_node(src, f).unwrap();
        assert_eq!(links_into(&s, node).len(), 1, "only `in`");
    }
    // A camera (video) cannot be a key.
    s.col
        .set_filter_settings(music, own, keyed(Some(s.cam)))
        .unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.problems.contains(&BindProblem::KeyUnavailable(own)));
}

#[test]
fn a_loop_of_keys_drops_the_link_of_the_highest_filter_id_in_any_edit_order() {
    for flip in [false, true] {
        let (mut s, music, voice) = music_and_voice();
        let (a, b);
        if flip {
            b = s
                .col
                .add_filter(voice, ty("org.rbs.audio-filter.ducker"), keyed(None))
                .unwrap()
                .0;
            a = s
                .col
                .add_filter(music, ty("org.rbs.audio-filter.ducker"), keyed(None))
                .unwrap()
                .0;
        } else {
            a = s
                .col
                .add_filter(music, ty("org.rbs.audio-filter.ducker"), keyed(None))
                .unwrap()
                .0;
            b = s
                .col
                .add_filter(voice, ty("org.rbs.audio-filter.ducker"), keyed(None))
                .unwrap()
                .0;
        }
        s.col
            .set_filter_settings(music, a, keyed(Some(voice)))
            .unwrap();
        s.col
            .set_filter_settings(voice, b, keyed(Some(music)))
            .unwrap();
        let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
        let (low, high, low_src, high_src) = if a < b {
            (a, b, music, voice)
        } else {
            (b, a, voice, music)
        };
        assert_eq!(report.problems, [BindProblem::KeyUnavailable(high)]);
        let low_node = s.binder.filter_node(low_src, low).unwrap();
        let high_node = s.binder.filter_node(high_src, high).unwrap();
        assert_eq!(
            links_into(&s, low_node).len(),
            2,
            "the lower id keeps its key"
        );
        assert_eq!(links_into(&s, high_node).len(), 1);
    }
}

#[test]
fn removing_the_key_source_or_the_ducker_cleans_up_the_key_link() {
    let (mut s, music, voice) = music_and_voice();
    let (d, _) = s
        .col
        .add_filter(music, ty("org.rbs.audio-filter.ducker"), keyed(Some(voice)))
        .unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    let duck = s.binder.filter_node(music, d).unwrap();
    assert_eq!(links_into(&s, duck).len(), 2);
    s.col.delete_source(voice).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.problems.contains(&BindProblem::KeyUnavailable(d)));
    assert_eq!(links_into(&s, duck).len(), 1);
    s.col.remove_filter(music, d).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.problems.is_empty());
    assert!(s.graph.node(duck).is_none());
}
