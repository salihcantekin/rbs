//! Linger (ADR-0152 decisions 3, 4 and 8, phase 3): a source whose level drops below
//! its `run` stays Active for `linger_s` on the binder's injected clock, as a warm
//! source of its type, and stops at once on Off, delete, a linger set to 0 or when it
//! is Active again. Counted on a `TestClock`; no test sleeps (ADR-0032).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::Arc;
use std::time::Duration;

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::Size;
use rbs_core::time::{TestClock, Timestamp};
use rbs_core::timing::FrameRate;
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_scene::SceneCollection;
use rbs_scene::activity::{ActivityTraits, OnReturn, Run, SourceActivity};
use rbs_scene::binder::{NodeTypes, Role, SceneBinder};
use rbs_scene::model::{
    CanvasId, ItemContent, Layout, SceneId, SceneScope, Settings, SourceId,
};

const CAMERA: &str = "org.rbs.source.camera";
const PAGE: &str = "org.rbs.source.browser";

/// A camera that may run only when shown (linked while warm, like the built-in
/// captures) and a page (not linked while warm).
struct Types;

impl NodeTypes for Types {
    fn descriptor(&self, id: &NodeTypeId) -> Option<Arc<NodeDescriptor>> {
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
            CAMERA | PAGE => d(
                NodeKind::Source,
                vec![],
                vec![Pin::one("video", LinkKind::Video)],
            ),
            _ => None,
        }
    }

    fn activity(&self, id: &NodeTypeId) -> ActivityTraits {
        match id.as_str() {
            CAMERA => ActivityTraits::new(
                &[Run::Placed, Run::Shown, Run::OnAir],
                &[],
                Run::Placed,
                None,
                true,
            ),
            PAGE => ActivityTraits::new(
                &[Run::Placed, Run::Shown, Run::OnAir],
                &[OnReturn::Continue, OnReturn::Restart],
                Run::Shown,
                Some(OnReturn::Continue),
                false,
            ),
            _ => ActivityTraits::PLACED,
        }
    }
}

struct Rig {
    col: SceneCollection,
    graph: GraphDescription,
    binder: SceneBinder,
    clock: TestClock,
    canvas: CanvasId,
    live: SceneId,
    shelf: SceneId,
    cam: SourceId,
}

fn full() -> Layout {
    Layout::new([0.0, 0.0], [100.0, 100.0])
}

/// A camera with `run: shown` and a 30 s linger, on the program scene "Live"; an
/// empty "Shelf" scene.
fn rig() -> Rig {
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_30)
        .unwrap();
    let (live, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    let (shelf, _) = col.add_scene("Shelf", SceneScope::Shared).unwrap();
    let (cam, _) = col
        .add_source("Cam", NodeTypeId::new(CAMERA), Settings::new())
        .unwrap();
    col.add_item(live, ItemContent::Source(cam), full(), None)
        .unwrap();
    col.add_item(shelf, ItemContent::Source(cam), full(), None)
        .unwrap();
    col.set_source_activity(cam, SourceActivity::new(Some(Run::Shown), None, Some(30)))
        .unwrap();
    col.set_program(canvas, Some(live)).unwrap();
    let clock = TestClock::new();
    let binder = SceneBinder::new(Types).with_clock(Arc::new(clock.clone()));
    let mut r = Rig {
        col,
        graph: GraphDescription::new(),
        binder,
        clock,
        canvas,
        live,
        shelf,
        cam,
    };
    r.sync();
    r
}

impl Rig {
    fn sync(&mut self) {
        self.binder.sync(&self.col, &mut self.graph).unwrap();
    }

    fn node(&self, id: SourceId) -> NodeId {
        self.binder.source_node(id).unwrap()
    }

    fn active(&self, id: SourceId) -> bool {
        self.graph.node(self.node(id)).unwrap().state == Lifecycle::Active
    }

    fn linked(&self, id: SourceId) -> bool {
        let comp = self.binder.compositor(self.canvas, Role::Program).unwrap();
        let node = self.node(id);
        self.graph
            .links()
            .iter()
            .any(|l| l.to.node == comp && l.from.node == node)
    }

    fn put_on_air(&mut self, scene: SceneId) {
        self.col.set_program(self.canvas, Some(scene)).unwrap();
        self.sync();
    }
}

#[test]
fn a_source_that_leaves_the_screen_lingers_warm_and_stops_when_its_time_is_up() {
    let mut r = rig();
    assert!(r.active(r.cam));
    assert_eq!(r.binder.linger_end(), None, "nothing lingers while on air");

    // Off air at t = 5 s: it lingers until 35 s, linked as a warm camera is.
    r.clock.advance(Duration::from_secs(5));
    r.put_on_air(r.shelf);
    let hidden = r.col.scene(r.shelf).unwrap().items[0].id;
    r.col.set_item_visible(hidden, false).unwrap();
    r.sync();
    assert!(r.active(r.cam));
    assert!(r.binder.is_lingering(r.cam));
    assert!(r.linked(r.cam), "a lingering capture stays linked while warm");
    let end = Timestamp::ZERO + Duration::from_secs(35);
    assert_eq!(r.binder.linger_end(), Some(end));
    assert_eq!(
        r.binder.linger_wait(r.clock_now()),
        Some(Duration::from_secs(30))
    );

    // A sync before the end changes nothing; at the end it stops.
    r.clock.advance(Duration::from_secs(29));
    r.sync();
    assert!(r.active(r.cam));
    assert_eq!(r.binder.linger_end(), Some(end));
    r.clock.advance(Duration::from_secs(1));
    r.sync();
    assert!(!r.active(r.cam));
    assert!(!r.binder.is_lingering(r.cam));
    assert_eq!(r.binder.linger_end(), None, "no wake is left armed");
    // An ended linger does not start again on the next sync.
    r.clock.advance(Duration::from_secs(1));
    r.sync();
    assert!(!r.active(r.cam));
    assert_eq!(r.binder.linger_end(), None);
}

#[test]
fn coming_back_cancels_the_linger_and_leaving_again_starts_a_new_one() {
    let mut r = rig();
    r.put_on_air(r.shelf);
    let hidden = r.col.scene(r.shelf).unwrap().items[0].id;
    r.col.set_item_visible(hidden, false).unwrap();
    r.sync();
    assert!(r.binder.is_lingering(r.cam));
    r.clock.advance(Duration::from_secs(10));
    r.put_on_air(r.live);
    assert!(r.active(r.cam));
    assert!(!r.binder.is_lingering(r.cam));
    assert_eq!(r.binder.linger_end(), None);
    r.clock.advance(Duration::from_secs(10));
    r.put_on_air(r.shelf);
    let end = Timestamp::ZERO + Duration::from_secs(50);
    assert_eq!(r.binder.linger_end(), Some(end), "a full linger from now");
}

#[test]
fn off_delete_and_a_linger_set_to_zero_end_it_at_once() {
    for end_it in 0..3 {
        let mut r = rig();
        r.put_on_air(r.shelf);
        let hidden = r.col.scene(r.shelf).unwrap().items[0].id;
        r.col.set_item_visible(hidden, false).unwrap();
        r.sync();
        assert!(r.binder.is_lingering(r.cam));
        match end_it {
            0 => {
                let source = r.col.source(r.cam).unwrap().clone();
                let mut hints = source.hints.clone();
                hints.insert(rbs_scene::STANDBY_HINT.to_owned(), "1".to_owned());
                r.col
                    .set_source_settings_and_hints(r.cam, source.settings, hints)
                    .unwrap();
            }
            1 => r
                .col
                .set_source_activity(r.cam, SourceActivity::new(Some(Run::Shown), None, None))
                .unwrap(),
            _ => {
                r.col.delete_source(r.cam).unwrap();
                r.sync();
                assert_eq!(r.binder.linger_end(), None, "deleted");
                continue;
            }
        }
        r.sync();
        assert!(!r.active(r.cam), "case {end_it}");
        assert_eq!(r.binder.linger_end(), None, "case {end_it}");
    }
}

#[test]
fn no_linger_and_no_clock_add_nothing() {
    // The default (`linger_s` absent) arms nothing.
    let mut r = rig();
    r.col
        .set_source_activity(r.cam, SourceActivity::new(Some(Run::Shown), None, None))
        .unwrap();
    r.sync();
    r.put_on_air(r.shelf);
    let hidden = r.col.scene(r.shelf).unwrap().items[0].id;
    r.col.set_item_visible(hidden, false).unwrap();
    r.sync();
    assert!(!r.active(r.cam));
    assert_eq!(r.binder.linger_end(), None);

    // A binder without a clock does not linger.
    let mut r = rig();
    r.binder = SceneBinder::new(Types);
    r.graph = GraphDescription::new();
    r.sync();
    r.put_on_air(r.shelf);
    let hidden = r.col.scene(r.shelf).unwrap().items[0].id;
    r.col.set_item_visible(hidden, false).unwrap();
    r.sync();
    assert!(!r.active(r.cam));
    assert_eq!(r.binder.linger_end(), None);
}

#[test]
fn a_lingering_page_is_active_but_not_linked() {
    let mut r = rig();
    let (page, _) = r
        .col
        .add_source("Page", NodeTypeId::new(PAGE), Settings::new())
        .unwrap();
    r.col
        .add_item(r.live, ItemContent::Source(page), full(), None)
        .unwrap();
    r.col
        .set_source_activity(page, SourceActivity::new(None, None, Some(5)))
        .unwrap();
    r.sync();
    assert!(r.active(page));
    r.put_on_air(r.shelf);
    assert!(r.active(page));
    assert!(r.binder.is_lingering(page));
    assert!(!r.linked(page), "a page is not linked while warm");
}

impl Rig {
    fn clock_now(&self) -> Timestamp {
        use rbs_core::time::Clock;
        self.clock.now()
    }
}
