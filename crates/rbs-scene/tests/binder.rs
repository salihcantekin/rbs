//! Contract tests for the scene binder (scene model spec, "Graph binding").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::Arc;

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_scene::SceneCollection;
use rbs_scene::binder::{BindError, BindProblem, Role, SceneBinder, resolve};
use rbs_scene::model::{CanvasId, ItemContent, Layout, SceneId, SceneScope, Settings, SourceId};

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
        "org.rbs.filter.lut" | "org.rbs.filter.key" => d(
            NodeKind::VideoFilter,
            vec![Pin::one("in", LinkKind::Video)],
            vec![Pin::one("out", LinkKind::Video)],
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
    let layers = &report.layers[&(s.wide, Role::Program)];
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
    assert_eq!(state(&s, cam), Lifecycle::Standby);
    // Its compositor link is gone, its layer too.
    let comp = s.binder.compositor(s.wide, Role::Program).unwrap();
    assert!(
        !s.graph
            .links()
            .iter()
            .any(|l| l.from.node == cam && l.to.node == comp)
    );
    assert_eq!(report.layers[&(s.wide, Role::Program)].len(), 1);

    s.col.set_item_visible(item, true).unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(state(&s, cam), Lifecycle::Active);
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
    let layer = &report.layers[&(s.wide, Role::Program)][1];
    assert_eq!(layer.producer, key_n);

    s.col.set_filter_enabled(s.cam, lut, false).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(linked(&s, cam, key_n) && !linked(&s, cam, lut_n));
    assert_eq!(state(&s, lut_n), Lifecycle::Standby);
    assert_eq!(state(&s, key_n), Lifecycle::Active);
    assert_eq!(report.layers[&(s.wide, Role::Program)][1].producer, key_n);

    s.col.remove_filter(s.cam, key).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(s.graph.node(key_n).is_none());
    assert_eq!(report.layers[&(s.wide, Role::Program)][1].producer, cam);
    let comp = s.binder.compositor(s.wide, Role::Program).unwrap();
    assert!(linked(&s, cam, comp));
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
    let layers = &report.layers[&(s.wide, Role::Program)];
    let plan = s.graph.plan();
    let resolved: Vec<usize> = resolve(comp, layers, &plan)
        .iter()
        .map(|(i, _)| *i)
        .collect();
    assert_eq!(resolved, [0, 1]);

    // If a producer is not scheduled (say its prepare failed), its layer drops out and
    // the others keep correct indices.
    let screen = s.binder.source_node(s.screen).unwrap();
    s.graph.set_state(screen, Lifecycle::Standby).unwrap();
    let plan = s.graph.plan();
    let resolved: Vec<(usize, SourceId)> = resolve(comp, layers, &plan)
        .iter()
        .map(|(i, l)| (*i, l.source))
        .collect();
    assert_eq!(resolved, [(0, s.cam)]);
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
    assert_eq!(report.layers[&(s.wide, Role::Program)].len(), 2);
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
    for seed in 0..30_u64 {
        let mut rng = Lcg(seed);
        let mut s = setup();
        let kinds = [
            "org.rbs.source.camera",
            "org.rbs.source.screen",
            "org.example.ghost",
        ];
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
            let _ = match rng.below(9) {
                0 => s
                    .col
                    .add_source(
                        &format!("S{seed}-{step}"),
                        ty(kinds[rng.below(3)]),
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
            let wanted = s.col.wanted_sources();
            for src in s.col.sources() {
                let Some(node) = s.binder.source_node(src.id) else {
                    continue;
                };
                let expect = if wanted.contains(&src.id) {
                    Lifecycle::Active
                } else {
                    Lifecycle::Standby
                };
                assert_eq!(state(&s, node), expect, "seed {seed} step {step}");
            }
            for ((canvas, role), layers) in &report.layers {
                let comp = s.binder.compositor(*canvas, *role).unwrap();
                let mut linked: Vec<NodeId> = s
                    .graph
                    .links()
                    .iter()
                    .filter(|l| l.to.node == comp)
                    .map(|l| l.from.node)
                    .collect();
                let mut producers: Vec<NodeId> = layers.iter().map(|l| l.producer).collect();
                producers.sort();
                producers.dedup();
                linked.sort();
                assert_eq!(linked, producers, "seed {seed} step {step}");
            }
            let bound_sources = s
                .graph
                .nodes()
                .iter()
                .filter(|n| n.descriptor.kind == NodeKind::Source)
                .count();
            let expected = s
                .col
                .sources()
                .filter(|x| s.binder.source_node(x.id).is_some())
                .count();
            assert_eq!(bound_sources, expected, "seed {seed} step {step}");
        }
    }
}
