//! Group spans (ADR-0193): the binder marks the drawn members of a group with group
//! Scroll, or says why the group gets no span.
#![allow(clippy::unwrap_used)]

use rbs_core::{media::Size, timing::FrameRate};
use rbs_graph::{
    description::GraphDescription,
    node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin},
};
use rbs_scene::binder::{
    BindProblem, BindReport, BoundLayer, Role, SceneBinder, SpanLimits, SpanRefusal,
};
use rbs_scene::{
    SceneCollection,
    model::{
        CanvasId, GroupId, GroupScroll, ItemContent, ItemId, Layout, SceneId, SceneScope,
        ScrollDirection, Settings,
    },
};
use std::sync::Arc;

fn types(id: &NodeTypeId) -> Option<Arc<NodeDescriptor>> {
    let (kind, inputs) = if id.as_str() == "org.rbs.compositor" {
        (
            NodeKind::Compositor,
            vec![Pin::many("layers", LinkKind::Video)],
        )
    } else {
        (NodeKind::Source, vec![])
    };
    Some(Arc::new(NodeDescriptor::new(
        id.clone(),
        kind,
        inputs,
        vec![Pin::one("video", LinkKind::Video)],
    )))
}

struct Fixture {
    col: SceneCollection,
    canvas: CanvasId,
    scene: SceneId,
    /// Items bottom first: a loose item, then groups of two.
    items: Vec<ItemId>,
    groups: Vec<GroupId>,
}

/// A 200x100 canvas drawing a 100x50 layout space (scale 2), with one loose item at
/// the bottom and `groups` groups of two items above it.
fn fixture(groups: usize) -> Fixture {
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(100, 50), FrameRate::FPS_60)
        .unwrap();
    // The layout space stays 100x50.
    col.set_canvas_format(canvas, Size::new(200, 100), FrameRate::FPS_60)
        .unwrap();
    let (scene, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    let (source, _) = col
        .add_source(
            "Colour",
            NodeTypeId::new("org.rbs.source.colour"),
            Settings::new(),
        )
        .unwrap();
    let add = |col: &mut SceneCollection, x: f32| {
        col.add_item(
            scene,
            ItemContent::Source(source),
            Layout::new([x, 5.0], [10.0, 10.0]),
            None,
        )
        .unwrap()
        .0
    };
    let mut items = vec![add(&mut col, 0.0)];
    let mut ids = Vec::new();
    for g in 0..groups {
        let pair = [
            add(&mut col, 10.0 + 30.0 * g as f32),
            add(&mut col, 20.0 + 30.0 * g as f32),
        ];
        items.extend(pair);
        ids.push(col.group_items(scene, &pair, None).unwrap().0);
    }
    col.set_program(canvas, Some(scene)).unwrap();
    Fixture {
        col,
        canvas,
        scene,
        items,
        groups: ids,
    }
}

impl Fixture {
    fn scroll(&mut self, g: usize) {
        let members = [self.items[1 + 2 * g], self.items[2 + 2 * g]];
        let scroll = Some(GroupScroll::new(ScrollDirection::Left, 10.0));
        self.col
            .set_group_scroll(self.scene, self.groups[g], &members, self.canvas, scroll)
            .unwrap();
    }

    fn bind(&self, limits: SpanLimits) -> BindReport {
        let mut binder = SceneBinder::new(types).with_span_limits(limits);
        binder
            .sync(&self.col, &mut GraphDescription::new())
            .unwrap()
    }
}

fn program(report: &BindReport, canvas: CanvasId) -> &[BoundLayer] {
    &report.compositors[&(canvas, Role::Program)].layers
}

#[test]
fn no_group_scroll_marks_nothing() {
    let f = fixture(2);
    let report = f.bind(SpanLimits::default());
    assert!(program(&report, f.canvas).iter().all(|l| l.group.is_none()));
    assert!(report.problems.is_empty());
}

#[test]
fn members_of_a_scrolling_group_carry_its_window_with_hidden_members() {
    let mut f = fixture(2);
    f.scroll(0);
    // A hidden member is not drawn but still widens the window.
    f.col.set_item_visible(f.items[2], false).unwrap();
    let report = f.bind(SpanLimits::default());
    let layers = program(&report, f.canvas);
    assert_eq!(layers.len(), 4);
    assert!(layers[0].group.is_none(), "the loose item is not a member");
    let mark = layers[1].group.unwrap();
    assert_eq!((mark.scene, mark.group), (f.scene, f.groups[0]));
    // Layout x 10..30, y 5..15 at scale 2.
    assert_eq!(mark.window, [20.0, 10.0, 40.0, 20.0]);
    assert_eq!(mark.scroll.speed, 20.0);
    assert_eq!(mark.refused, None);
    assert!(
        layers[2..].iter().all(|l| l.group.is_none()),
        "group 1 does not scroll"
    );
    assert!(report.problems.is_empty());
}

#[test]
fn an_oversized_group_gets_no_span_and_is_reported() {
    let mut f = fixture(2);
    f.scroll(0);
    f.scroll(1);
    let limits = SpanLimits {
        max_side: 30,
        ..SpanLimits::default()
    };
    let report = f.bind(limits);
    let layers = program(&report, f.canvas);
    // Group 0 is 40 pixels wide; group 1 is too (each 40x20), so both are refused.
    assert!(
        layers[1..]
            .iter()
            .all(|l| l.group.unwrap().refused == Some(SpanRefusal::TooLarge))
    );
    assert!(report.problems.contains(&BindProblem::GroupSpanRefused(
        f.canvas,
        f.scene,
        f.groups[0],
        SpanRefusal::TooLarge
    )));
    // Device side larger: both fit.
    let report = f.bind(SpanLimits::default());
    assert!(
        program(&report, f.canvas)[1..]
            .iter()
            .all(|l| l.group.unwrap().refused.is_none())
    );
}

#[test]
fn the_span_cap_keeps_the_bottom_most_spans() {
    let mut f = fixture(3);
    for g in 0..3 {
        f.scroll(g);
    }
    let limits = SpanLimits {
        max_spans: 2,
        ..SpanLimits::default()
    };
    let report = f.bind(limits);
    let refused: Vec<_> = program(&report, f.canvas)[1..]
        .iter()
        .map(|l| l.group.unwrap().refused)
        .collect();
    assert_eq!(
        refused,
        [
            None,
            None,
            None,
            None,
            Some(SpanRefusal::Capped),
            Some(SpanRefusal::Capped)
        ]
    );
    assert_eq!(
        report.problems,
        [BindProblem::GroupSpanRefused(
            f.canvas,
            f.scene,
            f.groups[2],
            SpanRefusal::Capped
        )]
    );
}
