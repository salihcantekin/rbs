//! Contract tests for the scene collection (scene model spec, "Invariants").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::BTreeSet;

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{
    Border, CanvasId, CollectionGlide, Crop, DisplayChrome, FilterId, GlideEasing, ItemContent,
    ItemId, ItemStyle, Layout, MAX_GLIDE_MS, MAX_SHOW_FOR_MS, MAX_SOURCE_VOLUME,
    MAX_SYNC_OFFSET_MS, MasterAudio, RecordSeparately, SceneId, SceneScope, SettingValue, Settings,
    Shadow, Shape, SourceAudio, SourceGlide, SourceId, SourceMonitor, SourceRegion,
};
use rbs_scene::{Affine2, MAX_LAYERS, MAX_NESTING_DEPTH, SceneCollection, SceneError, Undo};

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
    // A plain item carries the default style and its own box size.
    assert!(l.style.is_plain());
    assert_eq!(l.box_size, [960.0, 1080.0]);

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
    assert!(matches!(
        s.col.add_item(
            scene,
            ItemContent::Scene(other),
            full(1.0, 1.0).with_style(ItemStyle::new(Shape::Circle)),
            None
        ),
        Err(SceneError::StyleOnScene(_))
    ));
    for bad in [
        ItemStyle::new(Shape::Rectangle { radius: -1.0 }),
        ItemStyle::new(Shape::Rectangle { radius: f32::NAN }),
        ItemStyle::new(Shape::Circle).with_border(Some(Border::new(0.0, [0; 4]))),
        ItemStyle::new(Shape::Circle).with_shadow(Some(Shadow::new(
            [f32::INFINITY, 0.0],
            1.0,
            [0; 4],
        ))),
    ] {
        assert_eq!(
            s.col
                .add_item(
                    scene,
                    ItemContent::Source(s.cam),
                    full(1.0, 1.0).with_style(bad),
                    None
                )
                .map(|_| ()),
            Err(SceneError::InvalidLayout)
        );
    }
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
fn placed_sources_stay_wanted_off_air_until_deactivated() {
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
    // Placed sources stay open before any scene is on air. Deactivating is the
    // only way to release one.
    assert_eq!(
        s.col.wanted_sources(),
        BTreeSet::from([s.cam, s.screen, src3])
    );
    s.col.set_program(s.wide, Some(live)).unwrap();
    s.col.set_preview(s.wide, Some(next)).unwrap();
    // The hidden logo stays wanted: hiding does not close the source.
    assert_eq!(
        s.col.wanted_sources(),
        BTreeSet::from([s.cam, s.screen, src3])
    );
}

#[test]
fn temporal_sources_run_only_when_a_shown_scene_draws_them() {
    let mut s = studio();
    let (live, _) = s.col.add_scene("Live", SceneScope::Shared).unwrap();
    let (shelf, _) = s.col.add_scene("Shelf", SceneScope::Shared).unwrap();
    let (preview, _) = s.col.add_scene("Preview", SceneScope::Shared).unwrap();
    let (media, _) = s
        .col
        .add_source("Film", ty("media"), Settings::new())
        .unwrap();
    let (browser, _) = s
        .col
        .add_source("Page", ty("browser"), Settings::new())
        .unwrap();
    let (logo, _) = s
        .col
        .add_source("Logo", ty("image"), Settings::new())
        .unwrap();
    let (hidden_media, _) = s
        .col
        .add_item(live, ItemContent::Source(media), full(1.0, 1.0), None)
        .unwrap();
    let (hidden_page, _) = s
        .col
        .add_item(live, ItemContent::Source(browser), full(1.0, 1.0), None)
        .unwrap();
    let (hidden_cam, _) = s
        .col
        .add_item(live, ItemContent::Source(s.cam), full(1.0, 1.0), None)
        .unwrap();
    let (hidden_logo, _) = s
        .col
        .add_item(live, ItemContent::Source(logo), full(1.0, 1.0), None)
        .unwrap();
    s.col.set_item_visible(hidden_media, false).unwrap();
    s.col.set_item_visible(hidden_page, false).unwrap();
    s.col.set_item_visible(hidden_cam, false).unwrap();
    s.col.set_item_visible(hidden_logo, false).unwrap();
    s.col.set_program(s.wide, Some(live)).unwrap();

    // Hidden media and browser are not running. A hidden camera and a hidden
    // still stay running. Placement alone still counts as wanted.
    assert!(!s.col.running_sources().contains(&media));
    assert!(!s.col.running_sources().contains(&browser));
    assert!(s.col.running_sources().contains(&s.cam));
    assert!(s.col.running_sources().contains(&logo));
    assert!(s.col.wanted_sources().contains(&media));
    assert!(!s.col.warm_sources(s.wide).contains(&media));
    assert!(!s.col.warm_sources(s.wide).contains(&browser));
    assert!(s.col.warm_sources(s.wide).contains(&s.cam));
    assert!(s.col.warm_sources(s.wide).contains(&logo));

    // Opacity 0 with visible true still counts, for both temporal type ids.
    s.col.set_item_visible(hidden_media, true).unwrap();
    s.col
        .set_item_layout(hidden_media, None, full(1.0, 1.0).with_opacity(0.0))
        .unwrap();
    s.col.set_item_visible(hidden_page, true).unwrap();
    s.col
        .set_item_layout(hidden_page, None, full(1.0, 1.0).with_opacity(0.0))
        .unwrap();
    assert!(s.col.running_sources().contains(&media));
    assert!(s.col.running_sources().contains(&browser));
    assert!(
        !s.col
            .flatten(s.wide, live)
            .unwrap()
            .iter()
            .any(|l| l.source == media || l.source == browser)
    );

    // Placed only on a scene that is not program or preview: not running.
    s.col.remove_item(hidden_media).unwrap();
    s.col.remove_item(hidden_page).unwrap();
    s.col
        .add_item(shelf, ItemContent::Source(media), full(1.0, 1.0), None)
        .unwrap();
    s.col
        .add_item(shelf, ItemContent::Source(browser), full(1.0, 1.0), None)
        .unwrap();
    assert!(!s.col.running_sources().contains(&media));
    assert!(!s.col.running_sources().contains(&browser));
    assert!(s.col.wanted_sources().contains(&media));

    // The preview scene counts. One visible placement on any canvas is enough.
    s.col
        .add_item(preview, ItemContent::Source(browser), full(1.0, 1.0), None)
        .unwrap();
    s.col.set_preview(s.wide, Some(preview)).unwrap();
    assert!(s.col.running_sources().contains(&browser));
    assert!(!s.col.running_sources().contains(&media));
    let (tall_live, _) = s.col.add_scene("Tall live", SceneScope::Shared).unwrap();
    s.col
        .add_item(tall_live, ItemContent::Source(media), full(1.0, 1.0), None)
        .unwrap();
    s.col.set_program(s.tall, Some(tall_live)).unwrap();
    assert!(s.col.running_sources().contains(&media));
}

#[test]
fn timed_show_draws_a_hidden_item_without_writing_visible() {
    let mut s = studio();
    let (live, _) = s.col.add_scene("Live", SceneScope::Shared).unwrap();
    let (shelf, _) = s.col.add_scene("Shelf", SceneScope::Shared).unwrap();
    let (inner, _) = s.col.add_scene("Inner", SceneScope::Shared).unwrap();
    let (preview, _) = s.col.add_scene("Preview", SceneScope::Shared).unwrap();
    let (media, _) = s
        .col
        .add_source("Film", ty("media"), Settings::new())
        .unwrap();
    let (logo, _) = s
        .col
        .add_source("Logo", ty("image"), Settings::new())
        .unwrap();
    let (sponsor, _) = s
        .col
        .add_item(live, ItemContent::Source(media), full(1.0, 1.0), None)
        .unwrap();
    let (plate, _) = s
        .col
        .add_item(live, ItemContent::Source(logo), full(1.0, 1.0), None)
        .unwrap();
    let (nest, _) = s
        .col
        .add_item(live, ItemContent::Scene(inner), full(1.0, 1.0), None)
        .unwrap();
    let (nested_logo, _) = s
        .col
        .add_item(inner, ItemContent::Source(logo), full(1.0, 1.0), None)
        .unwrap();
    let (shelf_item, _) = s
        .col
        .add_item(shelf, ItemContent::Source(media), full(1.0, 1.0), None)
        .unwrap();
    let (preview_item, _) = s
        .col
        .add_item(preview, ItemContent::Source(media), full(1.0, 1.0), None)
        .unwrap();
    for id in [sponsor, plate, nest, nested_logo, shelf_item, preview_item] {
        s.col.set_item_visible(id, false).unwrap();
    }
    s.col.set_program(s.wide, Some(live)).unwrap();
    s.col.set_preview(s.wide, Some(preview)).unwrap();

    assert_eq!(
        s.col.set_item_show_for(sponsor, Some(0)),
        Err(SceneError::InvalidShowFor)
    );
    assert_eq!(
        s.col.set_item_show_for(sponsor, Some(MAX_SHOW_FOR_MS + 1)),
        Err(SceneError::InvalidShowFor)
    );
    let set = s.col.set_item_show_for(sponsor, Some(5_000)).unwrap();
    assert_eq!(s.col.item(sponsor).unwrap().1.show_for_ms, Some(5_000));
    assert!(!s.col.item(sponsor).unwrap().1.visible);
    s.col.undo(set).unwrap();
    assert_eq!(s.col.item(sponsor).unwrap().1.show_for_ms, None);
    s.col.set_item_show_for(sponsor, Some(5_000)).unwrap();

    let cued = BTreeSet::from([sponsor]);
    assert!(s.col.flatten(s.wide, live).unwrap().is_empty());
    let drawn = s.col.flatten_cued(s.wide, live, &cued).unwrap();
    assert_eq!(drawn.len(), 1);
    assert_eq!(drawn[0].source, media);
    assert_eq!(drawn[0].path, vec![sponsor]);
    assert!(!s.col.item(sponsor).unwrap().1.visible);
    assert!(
        s.col
            .visible_sources_cued(s.wide, live, &cued)
            .unwrap()
            .contains(&media)
    );
    assert!(
        !s.col
            .visible_sources(s.wide, live)
            .unwrap()
            .contains(&media)
    );
    assert_eq!(s.col.cued_drawn_sources(&cued), BTreeSet::from([media]));
    assert!(
        s.col
            .running_sources_cued(&BTreeSet::from([media]))
            .contains(&media)
    );
    assert!(!s.col.running_sources().contains(&media));
    // A still stays on the warm rule whether or not a cue names it.
    assert!(s.col.running_sources().contains(&logo));

    // A cue on a scene that is not the program does not appear on program output.
    let shelf_cue = BTreeSet::from([shelf_item]);
    assert!(
        s.col
            .flatten_cued(s.wide, live, &shelf_cue)
            .unwrap()
            .is_empty()
    );
    assert!(s.col.cued_drawn_sources(&shelf_cue).is_empty());

    // Preview draws its own cue and does not put it on the program list.
    let preview_cue = BTreeSet::from([preview_item]);
    assert!(
        s.col
            .flatten_cued(s.wide, live, &preview_cue)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        s.col
            .flatten_cued(s.wide, preview, &preview_cue)
            .unwrap()
            .len(),
        1
    );
    assert!(s.col.cued_drawn_sources(&preview_cue).contains(&media));

    // A hidden nested scene stays closed unless that nested item is cued.
    let inner_only = BTreeSet::from([nested_logo]);
    assert!(
        s.col
            .flatten_cued(s.wide, live, &inner_only)
            .unwrap()
            .is_empty()
    );
    let opened = BTreeSet::from([nest, nested_logo]);
    let nested = s.col.flatten_cued(s.wide, live, &opened).unwrap();
    assert_eq!(nested.len(), 1);
    assert_eq!(nested[0].source, logo);
    assert_eq!(nested[0].path, vec![nest, nested_logo]);

    // Opacity 0 is not a layer, and it still counts as drawn for the mixer set.
    s.col
        .set_item_layout(sponsor, None, full(1.0, 1.0).with_opacity(0.0))
        .unwrap();
    assert!(s.col.flatten_cued(s.wide, live, &cued).unwrap().is_empty());
    assert!(
        s.col
            .visible_sources_cued(s.wide, live, &cued)
            .unwrap()
            .contains(&media)
    );
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
    match rng.below(34) {
        // Scene levels (ADR-0091); every fifth volume is out of range.
        31 => {
            let scene = scene.ok_or(missing.clone())?;
            let levels = col.heard_levels(canvas.ok_or(missing)?, scene);
            col.set_scene_own_levels(scene, levels)
        }
        32 => col.set_scene_shared_levels(scene.ok_or(missing)?),
        33 => col.set_scene_level(
            scene.ok_or(missing.clone())?,
            source.ok_or(missing)?,
            (!n.is_multiple_of(7)).then(|| {
                rbs_scene::SceneLevel::new(
                    Some((n % 5) as f32 * 0.3),
                    n.is_multiple_of(3).then_some(true),
                )
            }),
        ),
        29 => col.set_globals(canvas.ok_or(missing)?, scene),
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
        24 => col.set_item_locked(item.ok_or(missing)?, n.is_multiple_of(2)),
        // Every fifth volume is out of range and must be refused.
        25 => col.set_source_volume(source.ok_or(missing)?, (n % 5) as f32 * 0.3),
        26 => col.set_source_muted(source.ok_or(missing)?, n.is_multiple_of(2)),
        // Every fourth offset is out of range and must be refused.
        27 => col.set_source_sync_offset(source.ok_or(missing)?, (n % 4) as i32 * 100 - 150),
        28 => {
            let (s, f) = filter.ok_or(missing)?;
            let mut settings = Settings::new();
            settings.insert("amount".to_owned(), SettingValue::Float((n % 3) as f64));
            col.set_filter_settings(s, f, settings)
        }
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
        30 => {
            let ms = match n % 5 {
                0 => None,
                1 => Some(0),
                2 => Some(MAX_SHOW_FOR_MS + 1),
                _ => Some((n % 10_000) as u32 + 1),
            };
            col.set_item_show_for(item.ok_or(missing)?, ms)
        }
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

#[test]
fn fit_keeps_the_aspect_ratio_and_centres() {
    // 21:9 display on a 16:9 canvas: full width, bars above and below.
    let l = Layout::fit(1920, 1080, 3440, 1440);
    assert_eq!(l.size[0], 1920.0);
    assert!((l.size[1] - 803.7209).abs() < 0.01, "{l:?}");
    assert!((l.position[1] - (1080.0 - l.size[1]) / 2.0).abs() < 0.001);
    assert_eq!(l.position[0], 0.0);
    // 4:3 on 16:9: full height, bars left and right.
    assert_eq!(
        Layout::fit(1920, 1080, 1440, 1080),
        Layout::new([240.0, 0.0], [1440.0, 1080.0])
    );
    // Same aspect fills; an unknown size fills.
    assert_eq!(
        Layout::fit(1920, 1080, 3840, 2160),
        Layout::new([0.0, 0.0], [1920.0, 1080.0])
    );
    assert_eq!(
        Layout::fit(1920, 1080, 0, 0),
        Layout::new([0.0, 0.0], [1920.0, 1080.0])
    );
}

fn parts(
    col: &SceneCollection,
) -> (
    Vec<rbs_scene::model::Canvas>,
    Vec<rbs_scene::model::Source>,
    Vec<rbs_scene::model::Scene>,
) {
    (
        col.canvases().cloned().collect(),
        col.sources().cloned().collect(),
        col.scenes().cloned().collect(),
    )
}

#[test]
fn from_parts_rebuilds_an_equal_collection_with_its_counters() {
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
        .set_item_layout(item, Some(s.tall), full(1080.0, 1920.0))
        .unwrap();
    s.col.set_program(s.wide, Some(scene)).unwrap();
    // A deleted object's id stays issued.
    let (gone, _) = s.col.add_scene("Gone", SceneScope::Shared).unwrap();
    s.col.remove_scene(gone).unwrap();

    let (c, src, sc) = parts(&s.col);
    let loaded = SceneCollection::from_parts(c, src, sc, s.col.id_counters()).unwrap();
    assert_eq!(loaded, s.col);
    assert_eq!(loaded.id_counters(), s.col.id_counters());
    // The next scene does not reuse the deleted one's id.
    let mut loaded = loaded;
    let (next, _) = loaded.add_scene("Next", SceneScope::Shared).unwrap();
    assert!(next.get() > gone.get());
}

#[test]
fn from_parts_raises_stale_counters_and_trims_names() {
    let mut s = studio();
    let (scene, _) = s.col.add_scene("Talk", SceneScope::Shared).unwrap();
    s.col
        .add_item(scene, ItemContent::Source(s.cam), full(1.0, 1.0), None)
        .unwrap();
    let (c, mut src, sc) = parts(&s.col);
    src[0].name = "  Camera  ".to_owned();
    let mut loaded =
        SceneCollection::from_parts(c, src, sc, rbs_scene::IdCounters::default()).unwrap();
    assert_eq!(loaded, s.col);
    let n = loaded.id_counters();
    assert_eq!(n, s.col.id_counters());
    let (cam2, _) = loaded
        .add_source("Camera 2", ty("camera"), Settings::new())
        .unwrap();
    assert!(cam2.get() > s.screen.get());
}

#[test]
fn from_parts_refuses_duplicate_ids_and_broken_invariants() {
    let mut s = studio();
    let (scene, _) = s.col.add_scene("Talk", SceneScope::Shared).unwrap();
    s.col
        .add_item(scene, ItemContent::Source(s.cam), full(1.0, 1.0), None)
        .unwrap();
    let counters = s.col.id_counters();

    let (mut c, src, sc) = parts(&s.col);
    c.push(c[0].clone());
    assert_eq!(
        SceneCollection::from_parts(c, src, sc, counters),
        Err(SceneError::DuplicateId(s.wide.to_string()))
    );

    // The same item id in two scenes.
    let (c, src, mut sc) = parts(&s.col);
    let mut copy = sc[0].clone();
    copy.id = SceneId::from_raw(99);
    copy.name = "Copy".to_owned();
    sc.push(copy);
    let item = sc[0].items[0].id;
    assert_eq!(
        SceneCollection::from_parts(c, src, sc, counters),
        Err(SceneError::DuplicateId(item.to_string()))
    );

    // An item placing a source that is not there.
    let (c, _, sc) = parts(&s.col);
    assert_eq!(
        SceneCollection::from_parts(c, Vec::new(), sc, counters),
        Err(SceneError::UnknownSource(s.cam))
    );

    // A canvas showing another canvas's scene (U15).
    let (mut c, src, mut sc) = parts(&s.col);
    sc[0].scope = SceneScope::Canvas(s.tall);
    c[0].program = Some(scene);
    assert_eq!(
        SceneCollection::from_parts(c, src, sc, counters),
        Err(SceneError::ScopeViolation(scene))
    );

    // Duplicate names.
    let (c, mut src, sc) = parts(&s.col);
    src[1].name = "Camera".to_owned();
    assert_eq!(
        SceneCollection::from_parts(c, src, sc, counters),
        Err(SceneError::DuplicateName("Camera".to_owned()))
    );

    // An id at the very top cannot leave a next id.
    let (mut c, src, sc) = parts(&s.col);
    c[1].id = CanvasId::from_raw(u32::MAX);
    assert_eq!(
        SceneCollection::from_parts(c, src, sc, counters),
        Err(SceneError::IdsExhausted)
    );
}

#[test]
fn canvas_size_and_frame_rate_are_bounded() {
    let mut col = SceneCollection::new();
    let max = rbs_scene::MAX_CANVAS_SIDE;
    assert!(
        col.add_canvas("Max", Size::new(max, max), FrameRate::FPS_60)
            .is_ok()
    );
    assert_eq!(
        col.add_canvas("Big", Size::new(max + 1, 2), FrameRate::FPS_60)
            .map(|_| ()),
        Err(SceneError::CanvasTooLarge)
    );
    let fastest = FrameRate::new(rbs_scene::MAX_FRAME_RATE, 1).unwrap();
    let (c, _) = col.add_canvas("Fast", Size::new(2, 2), fastest).unwrap();
    assert_eq!(
        col.set_canvas_format(c, Size::new(2, 2), FrameRate::new(1001, 1).unwrap())
            .map(|_| ()),
        Err(SceneError::FrameRateTooHigh)
    );
}

#[test]
fn a_style_reaches_the_layer_in_the_items_own_units() {
    let mut s = studio();
    let (inner, _) = s.col.add_scene("Inner", SceneScope::Shared).unwrap();
    let (outer, _) = s.col.add_scene("Outer", SceneScope::Shared).unwrap();
    let style = ItemStyle::new(Shape::Rectangle { radius: 40.0 })
        .with_border(Some(Border::new(8.0, [255, 255, 255, 255])));
    s.col
        .add_item(
            inner,
            ItemContent::Source(s.cam),
            Layout::new([0.0, 0.0], [-640.0, 360.0]).with_style(style),
            None,
        )
        .unwrap();
    // Nested at a quarter of the size: the style keeps the item's units, so the
    // compositor scales it with the picture.
    s.col
        .add_item(
            outer,
            ItemContent::Scene(inner),
            Layout::new([0.0, 0.0], [480.0, 270.0]),
            None,
        )
        .unwrap();
    let l = &s.col.flatten(s.wide, outer).unwrap()[0];
    assert_eq!(l.style, style);
    assert_eq!(l.box_size, [640.0, 360.0]);
}

#[test]
fn lock_and_source_audio_are_undoable_and_validated() {
    let mut s = studio();
    let (scene, _) = s.col.add_scene("Main", SceneScope::Shared).unwrap();
    let (item, _) = s
        .col
        .add_item(scene, ItemContent::Source(s.cam), full(10.0, 10.0), None)
        .unwrap();
    s.col.set_program(s.wide, Some(scene)).unwrap();
    let before = s.col.clone();
    assert!(!s.col.item(item).unwrap().1.locked);

    let lock = s.col.set_item_locked(item, true).unwrap();
    assert!(s.col.item(item).unwrap().1.locked);
    // Locked items still flatten and still take layout edits (ADR-0059).
    assert_eq!(s.col.flatten(s.wide, scene).unwrap().len(), 1);
    let moved = s.col.set_item_layout(item, None, full(20.0, 20.0)).unwrap();

    let source = s.col.source(s.cam).unwrap();
    assert_eq!(source.audio, SourceAudio::default());
    assert!((source.audio.volume - MAX_SOURCE_VOLUME).abs() < f32::EPSILON);
    let quiet = s.col.set_source_volume(s.cam, 0.25).unwrap();
    let mute = s.col.set_source_muted(s.cam, true).unwrap();
    assert_eq!(
        s.col.source(s.cam).unwrap().audio,
        SourceAudio::new(0.25, true)
    );

    let unchanged = s.col.clone();
    for bad in [-0.1, 1.01, f32::NAN, f32::INFINITY] {
        assert_eq!(
            s.col.set_source_volume(s.cam, bad),
            Err(SceneError::InvalidVolume)
        );
    }
    assert_eq!(s.col, unchanged);

    for undo in [mute, quiet, moved, lock] {
        s.col.undo(undo).unwrap();
    }
    assert_eq!(s.col, before);
}

#[test]
fn volume_and_mute_never_conflict_with_or_come_back_through_other_source_records() {
    let mut s = studio();
    let rename = s.col.rename_source(s.cam, "Webcam").unwrap();
    // A live mute after the rename (the app keeps it off the undo stack).
    s.col.set_source_muted(s.cam, true).unwrap();
    s.col.set_source_volume(s.cam, 0.5).unwrap();
    // Undoing the rename works and keeps the sound as it is now.
    let redo = s.col.undo(rename).unwrap();
    let cam = s.col.source(s.cam).unwrap();
    assert_eq!(cam.name, "Camera");
    assert_eq!(cam.audio, SourceAudio::new(0.5, true));
    s.col.set_source_muted(s.cam, false).unwrap();
    s.col.undo(redo).unwrap();
    let cam = s.col.source(s.cam).unwrap();
    assert_eq!(cam.name, "Webcam");
    assert_eq!(cam.audio, SourceAudio::new(0.5, false));
    // An audio record used out of order is refused.
    let a = s.col.set_source_volume(s.cam, 0.2).unwrap();
    s.col.set_source_volume(s.cam, 0.4).unwrap();
    assert_eq!(s.col.undo(a), Err(SceneError::UndoConflict));
}

#[test]
fn mixer_hidden_is_a_view_flag_that_keeps_the_sound() {
    let mut s = studio();
    let before = s.col.clone();
    let hide = s.col.set_source_mixer_hidden(s.cam, true).unwrap();
    s.col.set_source_volume(s.cam, 0.5).unwrap();
    s.col.set_source_muted(s.cam, true).unwrap();
    let audio = s.col.source(s.cam).unwrap().audio;
    assert!(audio.mixer_hidden && audio.muted && audio.volume == 0.5);
    assert!(audio.is_valid() && !audio.is_default());
    // Out of order, the record conflicts like volume's.
    assert_eq!(s.col.undo(hide), Err(SceneError::UndoConflict));
    s.col.set_source_volume(s.cam, 1.0).unwrap();
    s.col.set_source_muted(s.cam, false).unwrap();
    s.col.set_source_mixer_hidden(s.cam, false).unwrap();
    assert_eq!(s.col, before);
}

#[test]
fn monitor_is_a_live_audio_value_with_its_own_undo() {
    let mut s = studio();
    let before = s.col.clone();
    let only = s
        .col
        .set_source_monitor(s.cam, SourceMonitor::Only)
        .unwrap();
    let audio = s.col.source(s.cam).unwrap().audio;
    assert_eq!(audio.monitor, SourceMonitor::Only);
    assert!(audio.is_valid() && !audio.is_default());
    // A rename keeps the monitor mode and does not make the record conflict.
    s.col.rename_source(s.cam, "Camera 2").unwrap();
    assert_eq!(
        s.col.source(s.cam).unwrap().audio.monitor,
        SourceMonitor::Only
    );
    s.col.undo(only).unwrap();
    assert_eq!(
        s.col.source(s.cam).unwrap().audio.monitor,
        SourceMonitor::Off
    );
    s.col
        .rename_source(s.cam, &before.source(s.cam).unwrap().name.clone())
        .unwrap();
    assert_eq!(s.col, before);
    for m in [SourceMonitor::Off, SourceMonitor::Only, SourceMonitor::Also] {
        assert_eq!(SourceMonitor::from_name(m.as_str()), Some(m));
    }
    assert_eq!(SourceMonitor::from_name("solo"), None);
}

#[test]
fn sync_offset_is_a_live_audio_value_kept_by_volume_and_mute() {
    let mut s = studio();
    let before = s.col.clone();
    let late = s.col.set_source_sync_offset(s.cam, 120).unwrap();
    s.col.set_source_volume(s.cam, 0.5).unwrap();
    s.col.set_source_muted(s.cam, true).unwrap();
    assert_eq!(
        s.col.source(s.cam).unwrap().audio,
        SourceAudio::new(0.5, true).with_sync_offset(120)
    );
    let unchanged = s.col.clone();
    for bad in [MAX_SYNC_OFFSET_MS + 1, -MAX_SYNC_OFFSET_MS - 1, i32::MIN] {
        assert_eq!(
            s.col.set_source_sync_offset(s.cam, bad),
            Err(SceneError::InvalidSyncOffset)
        );
    }
    assert_eq!(s.col, unchanged);
    // A rename's undo keeps the offset as it is now.
    let rename = s.col.rename_source(s.cam, "Webcam").unwrap();
    s.col.set_source_sync_offset(s.cam, -80).unwrap();
    s.col.undo(rename).unwrap();
    assert_eq!(s.col.source(s.cam).unwrap().audio.sync_offset_ms, -80);
    // The record used out of order conflicts, like volume's.
    assert_eq!(s.col.undo(late), Err(SceneError::UndoConflict));
    s.col.set_source_sync_offset(s.cam, 0).unwrap();
    s.col.set_source_volume(s.cam, 1.0).unwrap();
    s.col.set_source_muted(s.cam, false).unwrap();
    assert_eq!(s.col, before);
}

#[test]
fn filter_settings_are_one_undo_step() {
    let mut s = studio();
    let (f, _) = s
        .col
        .add_filter(s.cam, ty("org.rbs.filter.blur"), Settings::new())
        .unwrap();
    let before = s.col.clone();
    let mut settings = Settings::new();
    settings.insert("radius".to_owned(), SettingValue::Float(4.0));
    let undo = s
        .col
        .set_filter_settings(s.cam, f, settings.clone())
        .unwrap();
    assert_eq!(s.col.source(s.cam).unwrap().filters[0].settings, settings);
    s.col.undo(undo).unwrap();
    assert_eq!(s.col, before);
    assert_eq!(
        s.col
            .set_filter_settings(s.cam, FilterId::from_raw(99), Settings::new()),
        Err(SceneError::UnknownFilter(FilterId::from_raw(99)))
    );
    let mut bad = Settings::new();
    bad.insert("radius".to_owned(), SettingValue::Float(f64::NAN));
    assert!(s.col.set_filter_settings(s.cam, f, bad).is_err());
    assert_eq!(s.col, before);
}

#[test]
fn a_global_scene_is_drawn_above_every_scene_and_wanted_with_it() {
    let mut s = studio();
    let (live, _) = s.col.add_scene("Live", SceneScope::Shared).unwrap();
    let (next, _) = s.col.add_scene("Next", SceneScope::Shared).unwrap();
    let (globals, _) = s
        .col
        .add_scene("Global sources", SceneScope::Canvas(s.wide))
        .unwrap();
    let (mic, _) = s
        .col
        .add_source("Mic", ty("audio-input"), Settings::new())
        .unwrap();
    s.col
        .add_item(
            live,
            ItemContent::Source(s.screen),
            full(1920.0, 1080.0),
            None,
        )
        .unwrap();
    let (cam_item, _) = s
        .col
        .add_item(
            globals,
            ItemContent::Source(s.cam),
            full(320.0, 180.0),
            None,
        )
        .unwrap();
    s.col
        .add_item(globals, ItemContent::Source(mic), full(1.0, 1.0), None)
        .unwrap();
    s.col.set_program(s.wide, Some(live)).unwrap();
    s.col.set_preview(s.wide, Some(next)).unwrap();
    // Placed already, even before this scene is the canvas's global scene.
    assert_eq!(
        s.col.wanted_sources(),
        BTreeSet::from([s.screen, s.cam, mic])
    );
    let before = s.col.flatten(s.wide, live).unwrap();

    s.col.set_globals(s.wide, Some(globals)).unwrap();
    assert_eq!(s.col.globals_of(s.wide), Some(globals));
    assert!(s.col.is_global_scene(globals));
    let flat = s.col.flatten(s.wide, live).unwrap();
    assert_eq!(flat.len(), 3);
    assert_eq!(flat[0], before[0]);
    // Above the scene, with the global item at the start of its path.
    assert_eq!(flat[1].source, s.cam);
    assert_eq!(flat[1].path, vec![cam_item]);
    assert_eq!(flat[2].source, mic);
    // In every scene the canvas shows, also an empty one.
    let preview: Vec<SourceId> = s
        .col
        .flatten(s.wide, next)
        .unwrap()
        .iter()
        .map(|l| l.source)
        .collect();
    assert_eq!(preview, vec![s.cam, mic]);
    assert_eq!(
        s.col.visible_sources(s.wide, live).unwrap(),
        BTreeSet::from([s.screen, s.cam, mic])
    );
    assert_eq!(
        s.col.wanted_sources(),
        BTreeSet::from([s.screen, s.cam, mic])
    );
    // Drawn once when the global scene itself is flattened.
    assert_eq!(s.col.flatten(s.wide, globals).unwrap().len(), 2);
    // Not on the other canvas.
    s.col.set_program(s.tall, Some(next)).unwrap();
    assert!(s.col.flatten(s.tall, next).unwrap().is_empty());
    assert!(s.col.visible_sources(s.tall, next).unwrap().is_empty());
    // A hidden global item is not drawn, and its source stays wanted so showing
    // it again does not wait for the device.
    s.col.set_item_visible(cam_item, false).unwrap();
    assert_eq!(
        s.col.wanted_sources(),
        BTreeSet::from([s.screen, s.cam, mic])
    );
    assert_eq!(s.col.flatten(s.wide, live).unwrap().len(), 2);
}

#[test]
fn global_scenes_are_never_shown_nested_shared_or_nesting() {
    let mut s = studio();
    let (live, _) = s.col.add_scene("Live", SceneScope::Shared).unwrap();
    let (shared, _) = s.col.add_scene("Shared", SceneScope::Shared).unwrap();
    let (tall_only, _) = s
        .col
        .add_scene("Tall only", SceneScope::Canvas(s.tall))
        .unwrap();
    let (globals, _) = s
        .col
        .add_scene("Globals", SceneScope::Canvas(s.wide))
        .unwrap();
    let refused = |r: Result<Undo, SceneError>| match r {
        Err(SceneError::GlobalScene(_)) => {}
        other => panic!("expected GlobalScene, got {other:?}"),
    };
    // Scoped to its own canvas only.
    refused(s.col.set_globals(s.wide, Some(shared)));
    refused(s.col.set_globals(s.wide, Some(tall_only)));
    // Shown as program or preview.
    s.col.set_program(s.wide, Some(live)).unwrap();
    refused(s.col.set_globals(s.wide, Some(live)));
    s.col.set_globals(s.wide, Some(globals)).unwrap();
    refused(s.col.set_program(s.wide, Some(globals)));
    refused(s.col.set_preview(s.wide, Some(globals)));
    // Nested, or nesting.
    let (wide_scene, _) = s
        .col
        .add_scene("Wide scene", SceneScope::Canvas(s.wide))
        .unwrap();
    refused(
        s.col
            .add_item(
                wide_scene,
                ItemContent::Scene(globals),
                full(1.0, 1.0),
                None,
            )
            .map(|x| x.1),
    );
    refused(
        s.col
            .add_item(globals, ItemContent::Scene(shared), full(1.0, 1.0), None)
            .map(|x| x.1),
    );
    // Its scope cannot change away from its canvas.
    refused(s.col.set_scene_scope(globals, SceneScope::Shared));
    // A canvas that owns its global scene cannot be removed until the scene goes.
    assert!(matches!(
        s.col.remove_canvas(s.wide),
        Err(SceneError::CanvasHasScenes(_))
    ));
}

#[test]
fn removing_a_global_scene_clears_it_and_undo_restores_it() {
    let mut s = studio();
    let (globals, _) = s
        .col
        .add_scene("Globals", SceneScope::Canvas(s.wide))
        .unwrap();
    s.col
        .add_item(globals, ItemContent::Source(s.cam), full(1.0, 1.0), None)
        .unwrap();
    let set = s.col.set_globals(s.wide, Some(globals)).unwrap();
    let before = s.col.clone();
    let undo = s.col.remove_scene(globals).unwrap();
    assert_eq!(s.col.globals_of(s.wide), None);
    assert!(s.col.scene(globals).is_none());
    let redo = s.col.undo(undo).unwrap();
    assert_eq!(s.col, before);
    assert_eq!(s.col.globals_of(s.wide), Some(globals));
    s.col.undo(redo).unwrap();
    assert_eq!(s.col.globals_of(s.wide), None);
    // Clearing it is a canvas edit with its own undo.
    let mut col = before.clone();
    let clear = col.set_globals(s.wide, None).unwrap();
    assert_eq!(col.globals_of(s.wide), None);
    col.undo(clear).unwrap();
    assert_eq!(col, before);
    drop(set);
}

#[test]
fn duplicate_scene_copies_items_and_shares_sources() {
    let mut s = studio();
    let (live, _) = s.col.add_scene("Live", SceneScope::Shared).unwrap();
    let (item, _) = s
        .col
        .add_item(
            live,
            ItemContent::Source(s.cam),
            Layout::new([100.0, 200.0], [320.0, 320.0]),
            None,
        )
        .unwrap();
    s.col
        .add_item(
            live,
            ItemContent::Source(s.screen),
            full(1920.0, 1080.0),
            None,
        )
        .unwrap();
    let before = s.col.clone();
    let (copy, undo) = s.col.duplicate_scene(live, "Live 2").unwrap();
    assert_ne!(copy, live);
    let original = s.col.scene(live).unwrap();
    let cloned = s.col.scene(copy).unwrap();
    assert_eq!(cloned.name, "Live 2");
    assert_eq!(cloned.items.len(), original.items.len());
    for (a, b) in original.items.iter().zip(cloned.items.iter()) {
        assert_ne!(a.id, b.id);
        assert_eq!(a.content, b.content);
        assert_eq!(a.visible, b.visible);
        assert_eq!(a.layout, b.layout);
    }
    assert_eq!(
        s.col.placements_of(s.cam).len(),
        before.placements_of(s.cam).len() + 1
    );
    assert_eq!(s.col.item(item).unwrap().1.layout.size, [320.0, 320.0]);
    s.col.undo(undo).unwrap();
    assert_eq!(s.col, before);
    assert_eq!(
        s.col.duplicate_scene(live, "Live"),
        Err(SceneError::DuplicateName("Live".into()))
    );
}

#[test]
fn a_canvas_takes_its_global_scene_with_it_and_its_name_is_free() {
    let mut s = studio();
    let (globals, _) = s
        .col
        .add_scene("Global sources", SceneScope::Canvas(s.tall))
        .unwrap();
    s.col
        .add_item(globals, ItemContent::Source(s.cam), full(1.0, 1.0), None)
        .unwrap();
    s.col.set_globals(s.tall, Some(globals)).unwrap();
    // The hidden scene's name does not take one from the user.
    let (mine, _) = s
        .col
        .add_scene("Global sources", SceneScope::Shared)
        .unwrap();
    assert_eq!(
        s.col
            .showable_scenes(s.tall)
            .into_iter()
            .map(|x| x.id)
            .collect::<Vec<_>>(),
        vec![mine]
    );
    let before = s.col.clone();
    let undo = s.col.remove_canvas(s.tall).unwrap();
    assert!(s.col.scene(globals).is_none());
    assert!(s.col.canvas(s.tall).is_none());
    s.col.undo(undo).unwrap();
    assert_eq!(s.col, before);
}

#[test]
fn a_scene_and_the_global_scene_together_stay_within_the_layer_limit() {
    use rbs_scene::model::{Scene, SceneItem};
    let s = studio();
    let (mut canvases, sources, _) = parts(&s.col);
    canvases[0].globals = Some(SceneId::from_raw(1));
    let build = |own: usize| {
        let mut live = Scene::new(SceneId::from_raw(0), "Live", SceneScope::Shared);
        let mut globals = Scene::new(SceneId::from_raw(1), "Globals", SceneScope::Canvas(s.wide));
        let mut next = 0;
        let mut item = |content| {
            next += 1;
            SceneItem::new(ItemId::from_raw(next), content, full(1.0, 1.0))
        };
        globals.items.push(item(ItemContent::Source(s.cam)));
        for _ in 0..own {
            live.items.push(item(ItemContent::Source(s.screen)));
        }
        SceneCollection::from_parts(
            canvases.clone(),
            sources.clone(),
            vec![live, globals],
            s.col.id_counters(),
        )
    };
    let limit = usize::try_from(MAX_LAYERS).unwrap();
    assert!(build(limit - 1).is_ok());
    assert!(matches!(
        build(limit),
        Err(SceneError::TooManyLayers(id)) if id == SceneId::from_raw(0)
    ));
}

#[test]
fn a_canvas_master_is_a_live_control_with_its_own_undo_record() {
    let mut s = studio();
    let before = s.col.clone();
    assert_eq!(s.col.canvas(s.wide).unwrap().master, MasterAudio::default());
    let quiet = s.col.set_canvas_master_volume(s.wide, 0.25).unwrap();
    let mute = s.col.set_canvas_master_muted(s.wide, true).unwrap();
    let master = s.col.canvas(s.wide).unwrap().master;
    assert_eq!(master, MasterAudio::new(0.25, true));
    assert!(master.gain().abs() < f32::EPSILON);
    // The other canvas keeps its own master.
    assert_eq!(s.col.canvas(s.tall).unwrap().master, MasterAudio::default());

    let unchanged = s.col.clone();
    for bad in [-0.1, 1.01, f32::NAN, f32::INFINITY] {
        assert_eq!(
            s.col.set_canvas_master_volume(s.wide, bad),
            Err(SceneError::InvalidVolume)
        );
    }
    assert_eq!(s.col, unchanged);

    s.col.undo(mute).unwrap();
    s.col.undo(quiet).unwrap();
    assert_eq!(s.col, before);
}

#[test]
fn a_master_never_conflicts_with_or_comes_back_through_other_canvas_records() {
    let mut s = studio();
    let rename = s.col.rename_canvas(s.wide, "Main").unwrap();
    // A live fader move after the rename.
    s.col.set_canvas_master_volume(s.wide, 0.5).unwrap();
    s.col.undo(rename).unwrap();
    let canvas = s.col.canvas(s.wide).unwrap();
    assert_ne!(canvas.name, "Main");
    assert!((canvas.master.volume - 0.5).abs() < f32::EPSILON);
}

#[test]
fn a_removed_canvas_takes_its_master_and_gets_it_back_on_undo() {
    let mut s = studio();
    s.col.set_canvas_master_muted(s.tall, true).unwrap();
    s.col.set_canvas_master_volume(s.tall, 0.4).unwrap();
    let undo = s.col.remove_canvas(s.tall).unwrap();
    assert!(s.col.canvas(s.tall).is_none());
    s.col.undo(undo).unwrap();
    assert_eq!(
        s.col.canvas(s.tall).unwrap().master,
        MasterAudio::new(0.4, true)
    );
}

#[test]
fn undoing_a_re_pick_restores_the_settings_and_the_hints() {
    let mut col = SceneCollection::new();
    let (src, _) = col
        .add_source("Window", ty("window"), rbs_scene::model::Settings::new())
        .unwrap();
    let hints = |t: &str| [("target_title".to_owned(), t.to_owned())].into();
    let before = col.source(src).unwrap().settings.clone();
    col.set_source_settings_and_hints(src, before.clone(), hints("old"))
        .unwrap();
    let undo = col
        .set_source_settings_and_hints(src, before, hints("new"))
        .unwrap();
    assert_eq!(col.source(src).unwrap().hints, hints("new"));
    let redo = col.undo(undo).unwrap();
    assert_eq!(col.source(src).unwrap().hints, hints("old"));
    col.undo(redo).unwrap();
    assert_eq!(col.source(src).unwrap().hints, hints("new"));
}

#[test]
fn a_canvas_follows_another_one_and_the_edit_undoes() {
    use rbs_scene::model::Fallback;

    let mut s = studio();
    let before = s.col.clone();
    let undo = s
        .col
        .set_canvas_follow(s.tall, Some(s.wide), Fallback::KeepLast)
        .unwrap();
    assert_eq!(s.col.canvas(s.tall).unwrap().follow, Some(s.wide));
    let followed = s.col.clone();
    s.col.undo(undo).unwrap();
    assert_eq!(s.col, before);
    s.col
        .set_canvas_follow(s.tall, Some(s.wide), Fallback::KeepLast)
        .unwrap();
    assert_eq!(s.col, followed);
    // Not itself, not a canvas that follows, and a followed canvas cannot follow.
    assert_eq!(
        s.col
            .set_canvas_follow(s.tall, Some(s.tall), Fallback::KeepLast),
        Err(SceneError::InvalidFollow(s.tall))
    );
    assert_eq!(
        s.col
            .set_canvas_follow(s.wide, Some(s.tall), Fallback::KeepLast),
        Err(SceneError::InvalidFollow(s.wide))
    );
    // The followed canvas cannot be removed while it is followed.
    assert!(s.col.remove_canvas(s.wide).is_err());
    // A fallback scene needs a follower and a scene the follower may show.
    let (own, _) = s.col.add_scene("Own", SceneScope::Canvas(s.tall)).unwrap();
    let (other, _) = s
        .col
        .add_scene("Other", SceneScope::Canvas(s.wide))
        .unwrap();
    assert!(
        s.col
            .set_canvas_follow(s.tall, Some(s.wide), Fallback::Scene(own))
            .is_ok()
    );
    assert_eq!(
        s.col
            .set_canvas_follow(s.tall, Some(s.wide), Fallback::Scene(other)),
        Err(SceneError::InvalidFollow(s.tall))
    );
    assert_eq!(
        s.col.set_canvas_follow(s.wide, None, Fallback::Scene(own)),
        Err(SceneError::InvalidFollow(s.wide))
    );
}

#[test]
fn a_follower_shows_the_followed_program_or_its_fallback() {
    use std::collections::BTreeMap;

    use rbs_scene::model::Fallback;

    let mut s = studio();
    let (live, _) = s.col.add_scene("Live", SceneScope::Shared).unwrap();
    let (game, _) = s.col.add_scene("Game", SceneScope::Canvas(s.wide)).unwrap();
    let (vertical, _) = s
        .col
        .add_scene("Vertical", SceneScope::Canvas(s.tall))
        .unwrap();
    s.col.set_program(s.wide, Some(live)).unwrap();
    s.col
        .set_canvas_follow(s.tall, Some(s.wide), Fallback::KeepLast)
        .unwrap();
    let none = BTreeMap::new();
    assert_eq!(s.col.followed_programs(&none)[&s.tall], live);

    // A scene of the main canvas alone cannot be shown: keep the last one shown.
    s.col.set_program(s.wide, Some(game)).unwrap();
    let last = BTreeMap::from([(s.tall, live)]);
    assert_eq!(s.col.followed_programs(&last)[&s.tall], live);
    // The chosen fallback scene instead.
    s.col
        .set_canvas_follow(s.tall, Some(s.wide), Fallback::Scene(vertical))
        .unwrap();
    assert_eq!(s.col.followed_programs(&last)[&s.tall], vertical);
    // A canvas that follows nothing is left out.
    assert!(!s.col.followed_programs(&last).contains_key(&s.wide));
}

#[test]
fn auto_layout_gives_each_item_a_layout_for_the_canvas_in_one_undo_step() {
    use std::collections::BTreeMap;

    use rbs_scene::AutoLayout;

    let mut s = studio();
    let (scene, _) = s.col.add_scene("Talk", SceneScope::Shared).unwrap();
    let (game, _) = s
        .col
        .add_item(
            scene,
            ItemContent::Source(s.screen),
            full(1920.0, 1080.0),
            None,
        )
        .unwrap();
    let (cam, _) = s
        .col
        .add_item(scene, ItemContent::Source(s.cam), full(640.0, 360.0), None)
        .unwrap();
    let before = s.col.clone();
    let picture = |_| Some((1920, 1080));
    let none = BTreeMap::new();

    let undo = s
        .col
        .auto_layout(s.tall, scene, AutoLayout::Stack, &picture, &none)
        .unwrap();
    let top = s.col.item(game).unwrap().1.overrides[&s.tall];
    let bottom = s.col.item(cam).unwrap().1.overrides[&s.tall];
    // Two equal bands of the 1080x1920 canvas, the lowest layer on top.
    assert_eq!((top.position, top.size), ([0.0, 0.0], [1080.0, 960.0]));
    assert_eq!(
        (bottom.position, bottom.size),
        ([0.0, 960.0], [1080.0, 960.0])
    );
    // A 16:9 picture cut to the 9:8 band keeps 1080 x 960 of its 1920 x 1080 pixels.
    let kept = 1920 - top.crop.left - top.crop.right;
    assert!((i64::from(kept) - 1215).abs() <= 1, "{kept}");
    assert_eq!(top.crop.top + top.crop.bottom, 0);
    // The horizontal canvas is untouched.
    assert!(!s.col.item(game).unwrap().1.overrides.contains_key(&s.wide));

    s.col.undo(undo).unwrap();
    assert_eq!(s.col, before);

    // Fit letterboxes: 1080 wide, 607.5 high, centred.
    s.col
        .auto_layout(s.tall, scene, AutoLayout::Fit, &picture, &none)
        .unwrap();
    let fit = s.col.item(game).unwrap().1.overrides[&s.tall];
    assert_eq!(fit.size, [1080.0, 607.5]);
    assert_eq!(fit.position, [0.0, (1920.0 - 607.5) / 2.0]);

    // Fill with the focus at the left edge cuts only the right side.
    let focus = BTreeMap::from([(game, 0.0)]);
    s.col
        .auto_layout(s.tall, scene, AutoLayout::Fill, &picture, &focus)
        .unwrap();
    let fill = s.col.item(game).unwrap().1.overrides[&s.tall];
    assert_eq!((fill.position, fill.size), ([0.0, 0.0], [1080.0, 1920.0]));
    assert_eq!(fill.crop.left, 0);
    assert!(fill.crop.right > 1000);
}

#[test]
fn collection_and_source_glide_undo_and_refuse_out_of_range() {
    let mut s = studio();
    assert_eq!(s.col.glide(), CollectionGlide::default());
    let before = s.col.clone();
    let undo = s
        .col
        .set_glide(CollectionGlide {
            duration_ms: 250,
            easing: GlideEasing::Linear,
        })
        .unwrap();
    assert_eq!(s.col.glide().duration_ms, 250);
    assert_eq!(s.col.glide().easing, GlideEasing::Linear);
    s.col.undo(undo).unwrap();
    assert_eq!(s.col, before);

    assert_eq!(
        s.col.set_glide(CollectionGlide {
            duration_ms: MAX_GLIDE_MS + 1,
            easing: GlideEasing::Linear,
        }),
        Err(SceneError::InvalidGlide)
    );

    let undo = s
        .col
        .set_source_glide(
            s.cam,
            Some(SourceGlide {
                duration_ms: 800,
                easing: GlideEasing::Linear,
            }),
        )
        .unwrap();
    assert_eq!(
        s.col.source(s.cam).unwrap().glide,
        Some(SourceGlide {
            duration_ms: 800,
            easing: GlideEasing::Linear,
        })
    );
    s.col.undo(undo).unwrap();
    assert!(s.col.source(s.cam).unwrap().glide.is_none());

    let set = s
        .col
        .set_source_glide(
            s.cam,
            Some(SourceGlide {
                duration_ms: 100,
                easing: GlideEasing::EaseInOut,
            }),
        )
        .unwrap();
    let clear = s.col.set_source_glide(s.cam, None).unwrap();
    assert!(s.col.source(s.cam).unwrap().glide.is_none());
    s.col.undo(clear).unwrap();
    assert!(s.col.source(s.cam).unwrap().glide.is_some());
    s.col.undo(set).unwrap();
    assert!(s.col.source(s.cam).unwrap().glide.is_none());
}

#[test]
fn chrome_is_only_for_a_display_and_one_undo_restores_the_region() {
    let mut col = SceneCollection::new();
    let (cam, _) = col
        .add_source("Cam", ty("camera"), Settings::new())
        .unwrap();
    assert!(matches!(
        col.set_display_chrome(cam, Some(DisplayChrome::new(1, 0, 0, 0))),
        Err(SceneError::ChromeNotDisplay)
    ));
    let (display, _) = col
        .add_source("Screen", ty("display"), Settings::new())
        .unwrap();
    let set = col
        .set_display_chrome(display, Some(DisplayChrome::new(8, 0, 40, 0)))
        .unwrap();
    assert_eq!(
        col.source(display).unwrap().region,
        Some(SourceRegion::new(0, 8, 0, 40))
    );
    let cleared = col.set_source_region(display, None).unwrap();
    assert!(col.source(display).unwrap().region.is_none());
    assert!(col.source(display).unwrap().chrome.is_some());
    col.undo(cleared).unwrap();
    assert_eq!(
        col.source(display).unwrap().region,
        Some(SourceRegion::new(0, 8, 0, 40))
    );
    col.undo(set).unwrap();
    assert!(col.source(display).unwrap().chrome.is_none());
    assert!(col.source(display).unwrap().region.is_none());
}

#[test]
fn a_canvas_size_change_keeps_the_stored_box_and_fits_it_on_the_way_back() {
    let mut s = studio();
    let (scene, _) = s.col.add_scene("Talk", SceneScope::Shared).unwrap();
    let stored = Layout::new([100.0, 40.0], [640.0, 360.0]).with_style(
        ItemStyle::new(Shape::Rectangle { radius: 20.0 })
            .with_border(Some(Border::new(4.0, [0; 4]))),
    );
    let (item, _) = s
        .col
        .add_item(scene, ItemContent::Source(s.cam), stored, None)
        .unwrap();
    s.col.set_program(s.wide, Some(scene)).unwrap();
    s.col.set_program(s.tall, Some(scene)).unwrap();
    let wide_before = s.col.flatten(s.wide, scene).unwrap();
    let tall_before = s.col.flatten(s.tall, scene).unwrap();

    s.col
        .set_canvas_format(s.wide, Size::new(1080, 1920), FrameRate::FPS_60)
        .unwrap();
    assert_eq!(s.col.item(item).unwrap().1.layout, stored);
    let canvas = s.col.canvas(s.wide).unwrap();
    assert_eq!(
        (canvas.layout_size.width, canvas.layout_size.height),
        (1920, 1080)
    );
    let placed = stored.placed_in((1920, 1080), (1080, 1920));
    let wide = s.col.flatten(s.wide, scene).unwrap();
    assert_eq!(wide[0].transform, Affine2::of_layout(&placed));
    assert_eq!(wide[0].box_size, [placed.size[0], placed.size[1]]);
    assert_eq!(wide[0].style.shape, placed.style.shape);
    // The other canvas still draws the stored box in its own pixels.
    assert_eq!(s.col.flatten(s.tall, scene).unwrap(), tall_before);

    s.col
        .set_canvas_format(s.wide, Size::new(1920, 1080), FrameRate::FPS_60)
        .unwrap();
    assert_eq!(s.col.item(item).unwrap().1.layout, stored);
    assert_eq!(s.col.flatten(s.wide, scene).unwrap(), wide_before);

    // An edit made in the current pixels is stored in the layout space and draws
    // back where it was put.
    s.col
        .set_canvas_format(s.wide, Size::new(1080, 1920), FrameRate::FPS_60)
        .unwrap();
    let live = Layout::new([0.0, 656.25], [90.0, 45.0]);
    s.col
        .set_item_layout(item, None, live.stored_in((1920, 1080), (1080, 1920)))
        .unwrap();
    let drawn = s.col.flatten(s.wide, scene).unwrap();
    assert_eq!(drawn[0].transform, Affine2::of_layout(&live));
}

#[test]
fn a_following_canvas_does_not_keep_its_own_scenes_sources_warm() {
    let mut s = studio();
    let (shared, _) = s.col.add_scene("Talk", SceneScope::Shared).unwrap();
    s.col
        .add_item(shared, ItemContent::Source(s.screen), full(1.0, 1.0), None)
        .unwrap();
    let (own, _) = s
        .col
        .add_scene("Vertical only", SceneScope::Canvas(s.tall))
        .unwrap();
    s.col
        .add_item(own, ItemContent::Source(s.cam), full(1.0, 1.0), None)
        .unwrap();
    s.col.set_program(s.wide, Some(shared)).unwrap();
    s.col.set_program(s.tall, Some(own)).unwrap();
    // On its own, the vertical canvas keeps the camera of its own scene warm.
    assert!(s.col.warm_sources(s.tall).contains(&s.cam));
    // Following, it shows only what the horizontal canvas puts on air.
    s.col
        .set_canvas_follow(s.tall, Some(s.wide), Default::default())
        .unwrap();
    let warm = s.col.warm_sources(s.tall);
    assert!(!warm.contains(&s.cam), "{warm:?}");
    assert!(warm.contains(&s.screen), "{warm:?}");
}

/// ADR-0110: placing a box in another canvas size and storing it back gives the box
/// again (within float error), and equal sizes leave it exactly as it is.
#[test]
fn a_layout_maps_out_and_back_to_itself() {
    use rbs_scene::model::Layout;
    let sizes = [
        (2, 2),
        (64, 64),
        (1920, 1080),
        (1080, 1920),
        (1440, 2560),
        (2560, 1440),
        (720, 1280),
        (3840, 2160),
        (8192, 2),
    ];
    let boxes = [
        Layout::new([0.0, 0.0], [1920.0, 1080.0]),
        Layout::new([-150.5, 33.25], [640.0, 360.0]),
        Layout::new([1700.0, 900.0], [-320.0, 180.0]),
    ];
    for from in sizes {
        for to in sizes {
            for l in boxes {
                let back = l.placed_in(from, to).stored_in(from, to);
                for i in 0..2 {
                    let tol = 1e-3 * l.position[i].abs().max(l.size[i].abs()).max(1.0);
                    assert!(
                        (back.position[i] - l.position[i]).abs() <= tol,
                        "{from:?} {to:?}"
                    );
                    assert!((back.size[i] - l.size[i]).abs() <= tol, "{from:?} {to:?}");
                }
                assert_eq!(l.placed_in(from, from), l);
            }
        }
    }
}

#[test]
fn recording_a_source_separately_is_one_undo_and_starts_off() {
    let mut col = SceneCollection::new();
    let (mic, _) = col
        .add_source("Mic", ty("audio-input"), Settings::new())
        .unwrap();
    assert!(col.source(mic).unwrap().record_separately.is_none());
    let set = col
        .set_source_record_separately(mic, RecordSeparately::new(false, true))
        .unwrap();
    assert!(col.source(mic).unwrap().record_separately.audio);
    let again = col
        .set_source_record_separately(mic, RecordSeparately::default())
        .unwrap();
    assert!(col.source(mic).unwrap().record_separately.is_none());
    col.undo(again).unwrap();
    assert!(col.source(mic).unwrap().record_separately.audio);
    col.undo(set).unwrap();
    assert!(col.source(mic).unwrap().record_separately.is_none());
    assert!(matches!(
        col.set_source_record_separately(SourceId::from_raw(99), RecordSeparately::default()),
        Err(SceneError::UnknownSource(_))
    ));
}
