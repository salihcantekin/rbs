//! Contract tests for the scene collection (scene model spec, "Invariants").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::BTreeSet;

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{
    CanvasId, Crop, ItemContent, ItemId, Layout, SceneId, SceneScope, SettingValue, Settings,
    SourceId,
};
use rbs_scene::{MAX_LAYERS, MAX_NESTING_DEPTH, SceneCollection, SceneError, Undo};

fn ty(name: &str) -> NodeTypeId {
    NodeTypeId::new(format!("org.rbs.source.{name}"))
}

fn full(w: f32, h: f32) -> Layout {
    Layout::new([0.0, 0.0], [w, h])
}

struct Studio {
    col: SceneCollection,
    wide: CanvasId,
    tall: CanvasId,
    cam: SourceId,
    screen: SourceId,
}

fn studio() -> Studio {
    let mut col = SceneCollection::new();
    let (wide, _) = col
        .add_canvas("Horizontal", Size::new(1920, 1080), FrameRate::FPS_60)
        .unwrap();
    let (tall, _) = col
        .add_canvas("Vertical", Size::new(1080, 1920), FrameRate::FPS_30)
        .unwrap();
    let (cam, _) = col
        .add_source("Camera", ty("camera"), Settings::new())
        .unwrap();
    let (screen, _) = col
        .add_source("Screen", ty("screen-capture"), Settings::new())
        .unwrap();
    Studio {
        col,
        wide,
        tall,
        cam,
        screen,
    }
}

#[test]
fn a_shared_scene_has_independent_layout_per_canvas() {
    let mut s = studio();
    let (scene, _) = s.col.add_scene("Talk", SceneScope::Shared).unwrap();
    let (item, _) = s
        .col
        .add_item(
            scene,
            ItemContent::Source(s.cam),
            full(1920.0, 1080.0),
            None,
        )
        .unwrap();
    s.col
        .set_item_layout(
            item,
            Some(s.tall),
            Layout::new([0.0, 420.0], [1080.0, 1080.0]),
        )
        .unwrap();
    s.col.set_program(s.wide, Some(scene)).unwrap();
    s.col.set_program(s.tall, Some(scene)).unwrap();

    let wide = s.col.flatten(s.wide, scene).unwrap();
    let tall = s.col.flatten(s.tall, scene).unwrap();
    assert_eq!(wide[0].transform.apply(1.0, 1.0), (1920.0, 1080.0));
    assert_eq!(tall[0].transform.apply(0.0, 0.0), (0.0, 420.0));
    assert_eq!(tall[0].transform.apply(1.0, 1.0), (1080.0, 1500.0));
    // Same source, one instance.
    assert_eq!(wide[0].source, tall[0].source);
}

#[test]
fn canvas_scenes_stay_on_their_canvas() {
    let mut s = studio();
    let (own, _) = s
        .col
        .add_scene("Wide only", SceneScope::Canvas(s.wide))
        .unwrap();
    let (shared, _) = s.col.add_scene("Shared", SceneScope::Shared).unwrap();
    assert_eq!(
        s.col.set_program(s.tall, Some(own)),
        Err(SceneError::ScopeViolation(own))
    );
    // A shared scene can be shown anywhere, so it must not nest a canvas scene.
    assert_eq!(
        s.col
            .add_item(shared, ItemContent::Scene(own), full(10.0, 10.0), None)
            .map(|_| ()),
        Err(SceneError::ScopeViolation(own))
    );
    // The other way round is fine.
    s.col
        .add_item(own, ItemContent::Scene(shared), full(10.0, 10.0), None)
        .unwrap();
    // Narrowing a scope is refused while something relies on the wider one.
    s.col.set_program(s.tall, Some(shared)).unwrap();
    assert_eq!(
        s.col.set_scene_scope(shared, SceneScope::Canvas(s.wide)),
        Err(SceneError::ScopeViolation(shared))
    );
    let names: Vec<&str> = s.col.scenes_for(s.tall).map(|x| x.name.as_str()).collect();
    assert_eq!(names, ["Shared"]);
}

#[test]
fn nesting_cycles_are_refused() {
    let mut s = studio();
    let (a, _) = s.col.add_scene("A", SceneScope::Shared).unwrap();
    let (b, _) = s.col.add_scene("B", SceneScope::Shared).unwrap();
    let (c, _) = s.col.add_scene("C", SceneScope::Shared).unwrap();
    s.col
        .add_item(a, ItemContent::Scene(b), full(1.0, 1.0), None)
        .unwrap();
    s.col
        .add_item(b, ItemContent::Scene(c), full(1.0, 1.0), None)
        .unwrap();
    let before = s.col.clone();
    assert!(matches!(
        s.col
            .add_item(c, ItemContent::Scene(a), full(1.0, 1.0), None),
        Err(SceneError::NestingCycle(_))
    ));
    assert!(matches!(
        s.col
            .add_item(a, ItemContent::Scene(a), full(1.0, 1.0), None),
        Err(SceneError::NestingCycle(_))
    ));
    assert_eq!(s.col, before, "a refused edit changes nothing");
}

#[test]
fn nested_scenes_are_scaled_into_their_box_and_inherit_visibility_and_opacity() {
    let mut s = studio();
    let (inner, _) = s.col.add_scene("Inner", SceneScope::Shared).unwrap();
    let (outer, _) = s.col.add_scene("Outer", SceneScope::Shared).unwrap();
    // The camera fills the inner scene's right half.
    s.col
        .add_item(
            inner,
            ItemContent::Source(s.cam),
            Layout::new([960.0, 0.0], [960.0, 1080.0]).with_crop(Crop::new(10, 0, 10, 0)),
            None,
        )
        .unwrap();
    // The inner scene is drawn into the top-left quarter at half opacity.
    let (nest, _) = s
        .col
        .add_item(
            outer,
            ItemContent::Scene(inner),
            Layout::new([0.0, 0.0], [960.0, 540.0]).with_opacity(0.5),
            None,
        )
        .unwrap();
    let layers = s.col.flatten(s.wide, outer).unwrap();
    assert_eq!(layers.len(), 1);
    let l = &layers[0];
    assert_eq!(l.transform.apply(0.0, 0.0), (480.0, 0.0));
    assert_eq!(l.transform.apply(1.0, 1.0), (960.0, 540.0));
    assert_eq!(l.opacity, 0.5);
    assert_eq!(l.crop, Crop::new(10, 0, 10, 0));
    assert_eq!(l.path.len(), 2);
    assert_eq!(l.path[0], nest);

    s.col.set_item_visible(nest, false).unwrap();
    assert!(s.col.flatten(s.wide, outer).unwrap().is_empty());
}

#[test]
fn rotation_is_about_the_box_centre() {
    let mut s = studio();
    let (scene, _) = s.col.add_scene("Rot", SceneScope::Shared).unwrap();
    s.col
        .add_item(
            scene,
            ItemContent::Source(s.cam),
            Layout::new([100.0, 100.0], [200.0, 100.0]).with_rotation(90.0),
            None,
        )
        .unwrap();
    let t = s.col.flatten(s.wide, scene).unwrap()[0].transform;
    let close = |(x, y): (f32, f32), (ex, ey): (f32, f32)| {
        assert!(
            (x - ex).abs() < 1e-3 && (y - ey).abs() < 1e-3,
            "({x},{y}) vs ({ex},{ey})"
        );
    };
    close(t.apply(0.5, 0.5), (200.0, 150.0)); // centre fixed
    close(t.apply(0.0, 0.0), (250.0, 50.0)); // top-left turns clockwise to top-right
}

#[test]
fn deleting_a_source_removes_every_placement_and_undo_restores_them_in_place() {
    let mut s = studio();
    let (a, _) = s.col.add_scene("A", SceneScope::Shared).unwrap();
    let (b, _) = s.col.add_scene("B", SceneScope::Shared).unwrap();
    let place = |col: &mut SceneCollection, scene, src| {
        col.add_item(scene, ItemContent::Source(src), full(1.0, 1.0), None)
            .unwrap()
            .0
    };
    let _ = place(&mut s.col, a, s.screen);
    let _ = place(&mut s.col, a, s.cam);
    let _ = place(&mut s.col, a, s.screen);
    let _ = place(&mut s.col, a, s.cam);
    let _ = place(&mut s.col, b, s.cam);
    let before = s.col.clone();
    let undo = s.col.delete_source(s.cam).unwrap();
    assert!(s.col.source(s.cam).is_none());
    assert_eq!(s.col.scene(a).unwrap().items.len(), 2);
    assert!(s.col.scene(b).unwrap().items.is_empty());
    let redo = s.col.undo(undo).unwrap();
    assert_eq!(s.col, before);
    s.col.undo(redo).unwrap();
    assert!(s.col.source(s.cam).is_none());
}

#[test]
fn removing_a_scene_on_air_is_undoable() {
    let mut s = studio();
    let (outer, _) = s.col.add_scene("Outer", SceneScope::Shared).unwrap();
    let (scene, _) = s.col.add_scene("Live", SceneScope::Shared).unwrap();
    s.col
        .add_item(outer, ItemContent::Scene(scene), full(1.0, 1.0), None)
        .unwrap();
    s.col.set_program(s.wide, Some(scene)).unwrap();
    s.col.set_preview(s.tall, Some(scene)).unwrap();
    let before = s.col.clone();
    let undo = s.col.remove_scene(scene).unwrap();
    assert_eq!(s.col.canvas(s.wide).unwrap().program, None);
    assert_eq!(s.col.canvas(s.tall).unwrap().preview, None);
    assert!(s.col.scene(outer).unwrap().items.is_empty());
    s.col.undo(undo).unwrap();
    assert_eq!(s.col, before);
}

#[test]
fn removing_a_canvas_drops_its_layouts_and_needs_no_owned_scenes() {
    let mut s = studio();
    let (own, _) = s
        .col
        .add_scene("Tall own", SceneScope::Canvas(s.tall))
        .unwrap();
    assert_eq!(
        s.col.remove_canvas(s.tall),
        Err(SceneError::CanvasHasScenes(s.tall))
    );
    s.col.set_scene_scope(own, SceneScope::Shared).unwrap();
    let (item, _) = s
        .col
        .add_item(own, ItemContent::Source(s.cam), full(1.0, 1.0), None)
        .unwrap();
    s.col
        .set_item_layout(item, Some(s.tall), full(5.0, 5.0))
        .unwrap();
    let before = s.col.clone();
    let undo = s.col.remove_canvas(s.tall).unwrap();
    assert!(s.col.item(item).unwrap().1.overrides.is_empty());
    s.col.undo(undo).unwrap();
    assert_eq!(s.col, before);
}

#[test]
fn invalid_edits_are_refused() {
    let mut s = studio();
    let (scene, _) = s.col.add_scene("S", SceneScope::Shared).unwrap();
    let (other, _) = s.col.add_scene("Other", SceneScope::Shared).unwrap();
    let before = s.col.clone();
    assert_eq!(
        s.col
            .add_source("Camera", ty("camera"), Settings::new())
            .map(|_| ()),
        Err(SceneError::DuplicateName("Camera".into()))
    );
    assert_eq!(s.col.rename_scene(scene, "  "), Err(SceneError::EmptyName));
    assert!(matches!(
        s.col
            .add_source("X", NodeTypeId::new("camera"), Settings::new()),
        Err(SceneError::BadTypeId(_))
    ));
    let mut bad = Settings::new();
    bad.insert("gain".into(), SettingValue::Float(f64::NAN));
    assert_eq!(
        s.col.add_source("Y", ty("y"), bad).map(|_| ()),
        Err(SceneError::InvalidSetting("gain".into()))
    );
    assert_eq!(
        s.col
            .add_item(
                scene,
                ItemContent::Source(s.cam),
                full(f32::INFINITY, 1.0),
                None
            )
            .map(|_| ()),
        Err(SceneError::InvalidLayout)
    );
    assert_eq!(
        s.col
            .add_item(
                scene,
                ItemContent::Source(s.cam),
                full(1.0, 1.0).with_opacity(1.5),
                None
            )
            .map(|_| ()),
        Err(SceneError::InvalidLayout)
    );
    assert!(matches!(
        s.col.add_item(
            scene,
            ItemContent::Scene(other),
            full(1.0, 1.0).with_crop(Crop::new(1, 0, 0, 0)),
            None
        ),
        Err(SceneError::CropOnScene(_))
    ));
    assert_eq!(
        s.col
            .add_item(scene, ItemContent::Source(s.cam), full(1.0, 1.0), Some(5))
            .map(|_| ()),
        Err(SceneError::IndexOutOfRange(5))
    );
    assert_eq!(
        s.col
            .add_canvas("Empty", Size::new(0, 10), FrameRate::FPS_30)
            .map(|_| ()),
        Err(SceneError::EmptyCanvas)
    );
    assert_eq!(s.col, before);
}

#[test]
fn wanted_sources_are_those_visible_on_program_or_preview() {
    let mut s = studio();
    let (live, _) = s.col.add_scene("Live", SceneScope::Shared).unwrap();
    let (next, _) = s.col.add_scene("Next", SceneScope::Shared).unwrap();
    let (idle, _) = s.col.add_scene("Idle", SceneScope::Shared).unwrap();
    let (src3, _) = s
        .col
        .add_source("Logo", ty("image"), Settings::new())
        .unwrap();
    s.col
        .add_item(live, ItemContent::Source(s.cam), full(1.0, 1.0), None)
        .unwrap();
    let (hidden, _) = s
        .col
        .add_item(live, ItemContent::Source(src3), full(1.0, 1.0), None)
        .unwrap();
    s.col.set_item_visible(hidden, false).unwrap();
    s.col
        .add_item(next, ItemContent::Source(s.screen), full(1.0, 1.0), None)
        .unwrap();
    s.col
        .add_item(idle, ItemContent::Source(src3), full(1.0, 1.0), None)
        .unwrap();
    assert!(s.col.wanted_sources().is_empty());
    s.col.set_program(s.wide, Some(live)).unwrap();
    s.col.set_preview(s.wide, Some(next)).unwrap();
    assert_eq!(s.col.wanted_sources(), BTreeSet::from([s.cam, s.screen]));
}

#[test]
fn filters_keep_order_and_ids() {
    let mut s = studio();
    let (f1, _) = s.col.add_filter(s.cam, ty("lut"), Settings::new()).unwrap();
    let (f2, _) = s
        .col
        .add_filter(s.cam, ty("chroma-key"), Settings::new())
        .unwrap();
    s.col.move_filter(s.cam, f2, 0).unwrap();
    s.col.set_filter_enabled(s.cam, f1, false).unwrap();
    let ids: Vec<_> = s
        .col
        .source(s.cam)
        .unwrap()
        .filters
        .iter()
        .map(|f| (f.id, f.enabled))
        .collect();
    assert_eq!(ids, [(f2, true), (f1, false)]);
    let undo = s.col.remove_filter(s.cam, f2).unwrap();
    s.col.undo(undo).unwrap();
    assert_eq!(s.col.source(s.cam).unwrap().filters[0].id, f2);
}

#[test]
fn undo_records_must_be_used_as_a_stack() {
    let mut s = studio();
    let (scene, _) = s.col.add_scene("S", SceneScope::Shared).unwrap();
    let (item, add) = s
        .col
        .add_item(scene, ItemContent::Source(s.cam), full(1.0, 1.0), None)
        .unwrap();
    let _remove = s.col.remove_item(item).unwrap();
    // Undoing the add while the item is already gone conflicts.
    assert_eq!(s.col.undo(add), Err(SceneError::UnknownItem(item)));
}

/// Deterministic generator for the property test.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn random_edit(col: &mut SceneCollection, rng: &mut Lcg) -> Result<Undo, SceneError> {
    let canvases: Vec<CanvasId> = col.canvases().map(|c| c.id).collect();
    let sources: Vec<SourceId> = col.sources().map(|c| c.id).collect();
    let scenes: Vec<SceneId> = col.scenes().map(|c| c.id).collect();
    let items: Vec<ItemId> = col
        .scenes()
        .flat_map(|s| s.items.iter().map(|i| i.id))
        .collect();
    let pick = |rng: &mut Lcg, v: &[u32]| v.get(rng.below(v.len())).copied();
    let canvas = pick(rng, &canvases.iter().map(|c| c.get()).collect::<Vec<_>>())
        .and_then(|raw| canvases.iter().copied().find(|c| c.get() == raw));
    let source = sources.get(rng.below(sources.len())).copied();
    let scene = scenes.get(rng.below(scenes.len())).copied();
    let other = scenes.get(rng.below(scenes.len())).copied();
    let item = items.get(rng.below(items.len())).copied();
    let n = rng.next();
    let layout = full((n % 500) as f32 + 1.0, 100.0);
    let missing = SceneError::IndexOutOfRange(usize::MAX);
    let filters: Vec<(SourceId, rbs_scene::model::FilterId)> = col
        .sources()
        .flat_map(|s| s.filters.iter().map(move |f| (s.id, f.id)))
        .collect();
    let filter = filters.get(rng.below(filters.len())).copied();
    let len = scene
        .and_then(|s| col.scene(s))
        .map_or(0, |s| s.items.len());
    match rng.below(24) {
        14 => {
            let (s, f) = filter.ok_or(missing)?;
            col.remove_filter(s, f)
        }
        15 => {
            let (s, f) = filter.ok_or(missing)?;
            col.move_filter(s, f, rng.below(3))
        }
        16 => {
            let (s, f) = filter.ok_or(missing)?;
            col.set_filter_enabled(s, f, n.is_multiple_of(2))
        }
        17 => col
            .add_filter(source.ok_or(missing)?, ty("f"), Settings::new())
            .map(|x| x.1),
        18 => col.rename_source(source.ok_or(missing)?, &format!("R{}", n % 7)),
        19 => col.rename_canvas(canvas.ok_or(missing)?, &format!("K{}", n % 7)),
        20 => col.set_preview(canvas.ok_or(missing)?, scene),
        21 => col.clear_item_layout(item.ok_or(missing.clone())?, canvas.ok_or(missing)?),
        22 => col
            .add_item(
                scene.ok_or(missing.clone())?,
                ItemContent::Source(source.ok_or(missing)?),
                layout,
                Some(rng.below(len + 1)),
            )
            .map(|x| x.1),
        23 => col.move_item(item.ok_or(missing)?, rng.below(len.max(1))),
        0 => col
            .add_canvas(&format!("C{n}"), Size::new(640, 360), FrameRate::FPS_30)
            .map(|x| x.1),
        1 => col.remove_canvas(canvas.ok_or(missing)?),
        2 => col
            .add_source(&format!("S{n}"), ty("t"), Settings::new())
            .map(|x| x.1),
        3 => col.delete_source(source.ok_or(missing)?),
        4 => {
            let scope = match canvas {
                Some(c) if n.is_multiple_of(2) => SceneScope::Canvas(c),
                _ => SceneScope::Shared,
            };
            col.add_scene(&format!("Sc{n}"), scope).map(|x| x.1)
        }
        5 => col.remove_scene(scene.ok_or(missing.clone())?),
        6 => col
            .add_item(
                scene.ok_or(missing.clone())?,
                ItemContent::Source(source.ok_or(missing)?),
                layout,
                None,
            )
            .map(|x| x.1),
        7 => col
            .add_item(
                scene.ok_or(missing.clone())?,
                ItemContent::Scene(other.ok_or(missing)?),
                layout,
                Some(0),
            )
            .map(|x| x.1),
        8 => col.remove_item(item.ok_or(missing)?),
        9 => col.move_item(item.ok_or(missing)?, 0),
        10 => col.set_item_visible(item.ok_or(missing)?, n.is_multiple_of(2)),
        11 => col.set_item_layout(item.ok_or(missing.clone())?, canvas, layout),
        12 => col.set_program(canvas.ok_or(missing)?, scene),
        _ => col.set_scene_scope(
            scene.ok_or(missing)?,
            match canvas {
                Some(c) if n.is_multiple_of(3) => SceneScope::Canvas(c),
                _ => SceneScope::Shared,
            },
        ),
    }
}

#[test]
fn random_edits_undo_and_redo_exactly() {
    for seed in 0..40_u64 {
        let mut rng = Lcg(seed);
        let mut col = studio().col;
        let mut states = vec![col.clone()];
        let mut undos = Vec::new();
        for _ in 0..120 {
            let before = col.clone();
            match random_edit(&mut col, &mut rng) {
                Ok(undo) => {
                    undos.push(undo);
                    states.push(col.clone());
                }
                Err(_) => assert_eq!(col, before, "seed {seed}: a refused edit changed state"),
            }
            // Every reachable state flattens without error on every canvas.
            for c in col.canvases() {
                for s in col.scenes_for(c.id) {
                    col.flatten(c.id, s.id).unwrap();
                }
            }
        }
        assert!(
            undos.len() >= 30,
            "seed {seed}: only {} edits applied",
            undos.len()
        );
        let last = col.clone();
        let mut redos = Vec::new();
        let mut forward = vec![col.clone()];
        while let Some(undo) = undos.pop() {
            states.pop();
            redos.push(col.undo(undo).unwrap());
            assert_eq!(&col, states.last().unwrap(), "seed {seed}: undo mismatch");
            forward.push(col.clone());
        }
        forward.pop();
        while let Some(redo) = redos.pop() {
            undos.push(col.undo(redo).unwrap());
            assert_eq!(&col, forward.last().unwrap(), "seed {seed}: redo mismatch");
            forward.pop();
        }
        assert_eq!(col, last, "seed {seed}: redo mismatch");
    }
}

#[test]
fn out_of_order_undo_is_refused_not_applied() {
    let mut s = studio();
    let (a, _) = s.col.add_scene("A", SceneScope::Shared).unwrap();
    let (b, _) = s.col.add_scene("B", SceneScope::Shared).unwrap();
    s.col.set_program(s.wide, Some(a)).unwrap();
    let rename = s.col.rename_canvas(s.wide, "X").unwrap();
    s.col.set_program(s.wide, Some(b)).unwrap();
    // Undoing the rename now would also revert the cut: refused.
    let before = s.col.clone();
    assert_eq!(s.col.undo(rename), Err(SceneError::UndoConflict));
    assert_eq!(s.col, before);

    let (f, add_filter) = s.col.add_filter(s.cam, ty("lut"), Settings::new()).unwrap();
    let _ = f;
    s.col.rename_source(s.cam, "Cam 2").unwrap();
    assert_eq!(s.col.undo(add_filter), Err(SceneError::UndoConflict));

    let (item, _) = s
        .col
        .add_item(a, ItemContent::Source(s.cam), full(1.0, 1.0), None)
        .unwrap();
    s.col
        .add_item(a, ItemContent::Source(s.screen), full(1.0, 1.0), None)
        .unwrap();
    let hide = s.col.set_item_visible(item, false).unwrap();
    s.col.set_item_layout(item, None, full(2.0, 2.0)).unwrap();
    assert_eq!(s.col.undo(hide), Err(SceneError::UndoConflict));
    let mv = s.col.move_item(item, 1).unwrap();
    s.col.move_item(item, 0).unwrap();
    assert_eq!(s.col.undo(mv), Err(SceneError::UndoConflict));
    let rename_scene = s.col.rename_scene(b, "Bee").unwrap();
    s.col
        .set_scene_scope(b, SceneScope::Canvas(s.wide))
        .unwrap();
    assert_eq!(s.col.undo(rename_scene), Err(SceneError::UndoConflict));
}

#[test]
fn nesting_depth_and_layer_count_are_bounded() {
    let mut s = studio();
    // A chain of MAX_NESTING_DEPTH scenes is fine; one more is refused.
    let mut chain = Vec::new();
    for i in 0..=MAX_NESTING_DEPTH {
        chain.push(
            s.col
                .add_scene(&format!("L{i}"), SceneScope::Shared)
                .unwrap()
                .0,
        );
    }
    for w in chain.windows(2).take(MAX_NESTING_DEPTH - 1) {
        s.col
            .add_item(w[0], ItemContent::Scene(w[1]), full(1.0, 1.0), None)
            .unwrap();
    }
    let last = chain.len() - 1;
    assert!(matches!(
        s.col.add_item(
            chain[last - 1],
            ItemContent::Scene(chain[last]),
            full(1.0, 1.0),
            None
        ),
        Err(SceneError::NestingTooDeep(_))
    ));

    // Diamonds double the layer count per level.
    let mut col = studio();
    let (base, _) = col.col.add_scene("D0", SceneScope::Shared).unwrap();
    col.col
        .add_item(base, ItemContent::Source(col.cam), full(1.0, 1.0), None)
        .unwrap();
    let mut prev = base;
    let mut result = Ok(());
    for i in 1..14 {
        let (d, _) = col
            .col
            .add_scene(&format!("D{i}"), SceneScope::Shared)
            .unwrap();
        col.col
            .add_item(d, ItemContent::Scene(prev), full(1.0, 1.0), None)
            .unwrap();
        result = col
            .col
            .add_item(d, ItemContent::Scene(prev), full(1.0, 1.0), None)
            .map(|_| ());
        if result.is_err() {
            break;
        }
        prev = d;
    }
    assert!(matches!(result, Err(SceneError::TooManyLayers(_))));
    let biggest = col.col.flatten(col.wide, prev).unwrap().len() as u64;
    assert!(biggest <= MAX_LAYERS);
}

#[test]
fn overrides_only_for_canvases_that_can_show_the_scene() {
    let mut s = studio();
    let (own, _) = s.col.add_scene("Own", SceneScope::Canvas(s.wide)).unwrap();
    let (item, _) = s
        .col
        .add_item(own, ItemContent::Source(s.cam), full(1.0, 1.0), None)
        .unwrap();
    assert_eq!(
        s.col.set_item_layout(item, Some(s.tall), full(2.0, 2.0)),
        Err(SceneError::ScopeViolation(own))
    );
    s.col
        .set_item_layout(item, Some(s.wide), full(2.0, 2.0))
        .unwrap();
}

#[test]
fn transparent_but_visible_sources_stay_wanted() {
    let mut s = studio();
    let (scene, _) = s.col.add_scene("Fade", SceneScope::Shared).unwrap();
    s.col
        .add_item(
            scene,
            ItemContent::Source(s.cam),
            full(1.0, 1.0).with_opacity(0.0),
            None,
        )
        .unwrap();
    s.col.set_program(s.wide, Some(scene)).unwrap();
    assert!(s.col.flatten(s.wide, scene).unwrap().is_empty());
    assert_eq!(s.col.wanted_sources(), BTreeSet::from([s.cam]));
}

#[test]
fn names_are_trimmed_before_comparing() {
    let mut s = studio();
    assert_eq!(
        s.col
            .add_source(" Camera ", ty("camera"), Settings::new())
            .map(|_| ()),
        Err(SceneError::DuplicateName("Camera".into()))
    );
    let (id, _) = s
        .col
        .add_source("  Logo ", ty("image"), Settings::new())
        .unwrap();
    assert_eq!(s.col.source(id).unwrap().name, "Logo");
}

#[test]
fn negative_size_flips_and_rotation_composes_inside_nested_scenes() {
    let mut s = studio();
    let (inner, _) = s.col.add_scene("In", SceneScope::Shared).unwrap();
    let (outer, _) = s.col.add_scene("Out", SceneScope::Shared).unwrap();
    // Mirrored horizontally: unit x=0 lands on the right edge.
    s.col
        .add_item(
            inner,
            ItemContent::Source(s.cam),
            Layout::new([1920.0, 0.0], [-1920.0, 1080.0]),
            None,
        )
        .unwrap();
    // Inner scene squeezed into a 960x1080 box, rotated 180 degrees.
    s.col
        .add_item(
            outer,
            ItemContent::Scene(inner),
            Layout::new([0.0, 0.0], [960.0, 1080.0]).with_rotation(180.0),
            None,
        )
        .unwrap();
    let t = s.col.flatten(s.wide, outer).unwrap()[0].transform;
    let close = |(x, y): (f32, f32), (ex, ey): (f32, f32)| {
        assert!(
            (x - ex).abs() < 1e-2 && (y - ey).abs() < 1e-2,
            "({x},{y}) vs ({ex},{ey})"
        );
    };
    // Mirror then 180-degree turn: x=0 -> inner right edge -> box left edge; y flips.
    close(t.apply(0.0, 0.0), (0.0, 1080.0));
    close(t.apply(1.0, 1.0), (960.0, 0.0));
}
