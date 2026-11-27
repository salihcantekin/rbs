//! The scene collection document (user data spec, "Scene collection document").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::TempDir;
use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{
    Border, CollectionGlide, Crop, DisplayChrome, Fallback, GlideEasing, ItemContent, ItemStyle,
    Layout, RecordSeparately, SceneScope, SettingValue, Settings, Shadow, Shape, SourceGlide,
    SourceMonitor, SourceRegion, VideoAs,
};
use rbs_scene::{SceneAudio, SceneCollection, SceneLevel};
use rbs_user_data::document;
use rbs_user_data::{BodyError, DataError, UserData};

fn ty(name: &str) -> NodeTypeId {
    NodeTypeId::new(format!("org.rbs.source.{name}"))
}

/// Two canvases, shared and canvas-scoped scenes, filters, every setting type,
/// per-canvas layouts, crop, rotation, opacity, a hidden item and studio mode.
fn show() -> SceneCollection {
    let mut col = SceneCollection::new();
    let (wide, _) = col
        .add_canvas("Horizontal", Size::new(1920, 1080), FrameRate::FPS_59_94)
        .unwrap();
    let (tall, _) = col
        .add_canvas("Vertical", Size::new(1080, 1920), FrameRate::FPS_30)
        .unwrap();
    let mut settings = Settings::new();
    settings.insert("cursor".to_owned(), SettingValue::Bool(true));
    settings.insert("fps".to_owned(), SettingValue::Int(60));
    settings.insert("scale".to_owned(), SettingValue::Float(0.1));
    settings.insert(
        "target".to_owned(),
        SettingValue::Text("display:1 \"main\"".to_owned()),
    );
    let (screen, _) = col.add_source("Screen", ty("display"), settings).unwrap();
    let (cam, _) = col
        .add_source("Camera", ty("camera"), Settings::new())
        .unwrap();
    let (blur, _) = col
        .add_filter(cam, NodeTypeId::new("org.rbs.filter.blur"), Settings::new())
        .unwrap();
    col.add_filter(cam, NodeTypeId::new("org.rbs.filter.crop"), Settings::new())
        .unwrap();
    col.set_filter_enabled(cam, blur, false).unwrap();
    let (mic, _) = col
        .add_source("Mic", ty("audio-input"), Settings::new())
        .unwrap();
    col.set_source_volume(mic, 0.3).unwrap();
    col.set_source_muted(mic, true).unwrap();
    col.set_source_sync_offset(mic, -40).unwrap();
    col.set_source_mixer_hidden(mic, true).unwrap();
    col.set_source_monitor(mic, SourceMonitor::Only).unwrap();
    col.set_source_monitor(cam, SourceMonitor::Also).unwrap();
    let hinted = col.source(screen).unwrap().settings.clone();
    col.set_source_settings_and_hints(
        screen,
        hinted,
        [("target_app".to_owned(), "Terminal".to_owned())].into(),
    )
    .unwrap();
    col.set_canvas_master_volume(wide, 0.5).unwrap();
    col.set_canvas_master_muted(wide, true).unwrap();

    let (talk, _) = col.add_scene("Talk", SceneScope::Shared).unwrap();
    let (cam_item, _) = col
        .add_item(
            talk,
            ItemContent::Source(cam),
            Layout::new([0.0, 0.0], [1920.0, 1080.0]).with_style(
                ItemStyle::new(Shape::Circle)
                    .with_border(Some(Border::new(6.5, [255, 0, 128, 200]))),
            ),
            None,
        )
        .unwrap();
    col.set_item_layout(
        cam_item,
        Some(tall),
        Layout::new([0.0, 420.0], [1080.0, 1080.0])
            .with_crop(Crop::new(420, 0, 420, 0))
            .with_style(
                ItemStyle::new(Shape::Rectangle { radius: 24.0 }).with_shadow(Some(Shadow::new(
                    [4.0, 6.0],
                    12.0,
                    [0, 0, 0, 128],
                ))),
            ),
    )
    .unwrap();
    col.add_item(
        talk,
        ItemContent::Source(mic),
        Layout::new([0.0, 0.0], [1.0, 1.0]),
        None,
    )
    .unwrap();

    let (game, _) = col.add_scene("Game", SceneScope::Canvas(wide)).unwrap();
    col.add_item(
        game,
        ItemContent::Source(screen),
        Layout::new([0.0, 0.0], [1920.0, 1080.0]),
        None,
    )
    .unwrap();
    let (pip, _) = col
        .add_item(
            game,
            ItemContent::Source(cam),
            Layout::new([1440.5, 810.25], [-480.0, 270.0])
                .with_rotation(12.5)
                .with_opacity(0.75),
            None,
        )
        .unwrap();
    col.set_item_visible(pip, false).unwrap();
    col.set_item_locked(pip, true).unwrap();
    col.set_program(wide, Some(game)).unwrap();
    col.set_preview(wide, Some(talk)).unwrap();
    col.set_program(tall, Some(talk)).unwrap();
    col.set_canvas_follow(tall, Some(wide), Fallback::Scene(talk))
        .unwrap();
    // A global source on the wide canvas (format 5, ADR-0065).
    let (desk, _) = col
        .add_source("Desk mic", ty("audio-input"), Settings::new())
        .unwrap();
    let (globals, _) = col
        .add_scene("Global sources", SceneScope::Canvas(wide))
        .unwrap();
    col.add_item(
        globals,
        ItemContent::Source(desk),
        Layout::new([0.0, 0.0], [1.0, 1.0]),
        None,
    )
    .unwrap();
    col.set_globals(wide, Some(globals)).unwrap();
    // An issued, then removed, id.
    let (gone, _) = col.add_scene("Gone", SceneScope::Shared).unwrap();
    col.remove_scene(gone).unwrap();
    col
}

#[test]
fn a_collection_round_trips_exactly_with_its_counters() {
    let col = show();
    let text = document::render(&col, Some("My show")).unwrap();
    let read = document::parse::<SceneCollection>(&text).unwrap();
    assert_eq!(read.document, col);
    assert_eq!(read.document.id_counters(), col.id_counters());
    assert_eq!(read.name.as_deref(), Some("My show"));
    // Readable numbers and the documented keys.
    for want in [
        "kind = \"scene-collection\"",
        "frame_rate = [60000, 1001]",
        "scale = 0.1",
        "rotation = 12.5",
        "opacity = 0.75",
        "crop = [420, 0, 420, 0]",
        "type = \"org.rbs.filter.blur\"",
        "enabled = false",
        "visible = false",
        "canvas = 0",
        "format = 15",
        "monitor = \"only\"",
        "monitor = \"also\"",
        "follow = 0",
        "[canvases.fallback]",
        "target_app = \"Terminal\"",
        "[canvases.master]",
        "mixer_hidden = true",
        "globals = ",
        "sync_offset_ms = -40",
        "locked = true",
        "volume = 0.3",
        "muted = true",
        "shape = \"circle\"",
        "color = \"#ff0080c8\"",
        "radius = 24.0",
        "offset = [4.0, 6.0]",
    ] {
        assert!(text.contains(want), "{want} missing in:\n{text}");
    }
    // Rendering again gives the same text (stable diffs).
    assert_eq!(
        document::render(&read.document, Some("My show")).unwrap(),
        text
    );
}

#[test]
fn the_documented_example_loads() {
    let text = r#"
format = 1
kind = "scene-collection"
name = "Main show"

[next]
canvas = 1
source = 1
scene = 1
item = 1
filter = 0

[[canvases]]
id = 0
name = "Main"
width = 1920
height = 1080
frame_rate = [30, 1]
program = 0

[[sources]]
id = 0
name = "Display"
type = "org.rbs.source.display"
settings = { cursor = true }

[[scenes]]
id = 0
name = "Scene"

[[scenes.items]]
id = 0
source = 0
layout = { position = [0.0, 0.0], size = [1920.0, 1080.0] }
"#;
    let col = document::parse::<SceneCollection>(text).unwrap().document;
    assert_eq!(col.canvases().count(), 1);
    let scene = col.scenes().next().unwrap();
    assert_eq!(scene.scope, SceneScope::Shared);
    assert_eq!(
        scene.items[0].layout,
        Layout::new([0.0, 0.0], [1920.0, 1080.0])
    );
    // An empty collection is a valid document too.
    let empty = document::parse::<SceneCollection>("format = 1\nkind = \"scene-collection\"\n");
    assert_eq!(empty.unwrap().document, SceneCollection::new());
}

fn invalid(body: &str) -> String {
    let text = format!("format = 1\nkind = \"scene-collection\"\n{body}");
    match document::parse::<SceneCollection>(&text) {
        Err(DataError::Invalid(why)) => why.to_string(),
        other => panic!("expected Invalid for {body:?}, got {other:?}"),
    }
}

#[test]
fn a_follow_in_a_format_before_9_is_refused() {
    // `invalid` writes format 1: the keys of format 9 do not belong there.
    let why = invalid(&format!("{CANVAS}follow = 0\n"));
    assert!(why.contains("follow"), "{why}");
}

#[test]
fn a_monitor_in_a_format_before_10_is_refused() {
    let why = invalid(&format!("{SOURCE}monitor = \"also\"\n"));
    assert!(why.contains("monitor"), "{why}");
}

#[test]
fn an_order_in_a_format_before_11_is_refused() {
    let why = invalid(&format!("{CANVAS}order = [0]\n"));
    assert!(why.contains("order"), "{why}");
}

#[test]
fn a_glide_in_a_format_before_12_is_refused() {
    let why = invalid("glide = { duration_ms = 200, easing = \"linear\" }\n");
    assert!(why.contains("glide"), "{why}");
    let why = invalid(&format!(
        "{SOURCE}glide = {{ duration_ms = 200, easing = \"linear\" }}\n"
    ));
    assert!(why.contains("glide"), "{why}");
}

#[test]
fn format_11_loads_as_default_glide_and_format_12_round_trips() {
    let text = format!("format = 11\nkind = \"scene-collection\"\n{CANVAS}{SOURCE}");
    let col = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(col.glide(), CollectionGlide::default());
    assert!(col.sources().all(|s| s.glide.is_none()));

    let mut show = show();
    show.set_glide(CollectionGlide {
        duration_ms: 0,
        easing: GlideEasing::Linear,
    })
    .unwrap();
    let cam = show.sources().find(|s| s.name == "Camera").unwrap().id;
    show.set_source_glide(
        cam,
        Some(SourceGlide {
            duration_ms: 900,
            easing: GlideEasing::Linear,
        }),
    )
    .unwrap();
    let text = document::render(&show, Some("Show")).unwrap();
    // A 0 ms glide is written as format 19, so it is not read back as Cut (ADR-0128).
    assert!(text.contains("format = 19"), "{text}");
    assert!(text.contains("duration_ms = 0"), "{text}");
    assert!(text.contains("easing = \"linear\""), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(back, show);
}

#[test]
fn show_for_ms_round_trips_and_an_older_format_refuses_it() {
    let mut show = show();
    let item = show.scenes().next().unwrap().items[0].id;
    show.set_item_show_for(item, Some(4_000)).unwrap();
    let text = document::render(&show, Some("Show")).unwrap();
    assert!(text.contains("show_for_ms = 4000"), "{text}");
    assert!(text.contains("format = 15"), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(back.item(item).unwrap().1.show_for_ms, Some(4_000));
    assert_eq!(back, show);

    let older = format!(
        "format = 12\nkind = \"scene-collection\"\n{CANVAS}{SOURCE}\n[[scenes]]\nid = 0\nname = \"S\"\n[[scenes.items]]\nid = 0\nsource = 0\nshow_for_ms = 4000\nlayout = {{ position = [0.0, 0.0], size = [1.0, 1.0] }}\n"
    );
    match document::parse::<SceneCollection>(&older) {
        Err(DataError::Invalid(BodyError::LaterKey { keys, since, .. })) => {
            assert!(keys.contains("show_for_ms"), "{keys}");
            assert_eq!(since, 13);
        }
        other => panic!("expected LaterKey, got {other:?}"),
    }
}

#[test]
fn an_unknown_monitor_mode_is_refused() {
    let text = format!("format = 10\nkind = \"scene-collection\"\n{SOURCE}monitor = \"solo\"\n");
    assert!(document::parse::<SceneCollection>(&text).is_err());
}

const CANVAS: &str =
    "[[canvases]]\nid = 0\nname = \"Main\"\nwidth = 1920\nheight = 1080\nframe_rate = [30, 1]\n";
const SOURCE: &str = "[[sources]]\nid = 0\nname = \"S\"\ntype = \"org.rbs.source.display\"\n";

#[test]
fn invalid_documents_are_refused_with_a_reason() {
    // Unknown keys (typos) are errors, not silently dropped.
    invalid(&format!("{CANVAS}colour = 1\n"));
    invalid("unknown = 1\n");
    // Frame rate and size.
    assert!(
        invalid(
            "[[canvases]]\nid = 0\nname = \"M\"\nwidth = 2\nheight = 2\nframe_rate = [30, 0]\n"
        )
        .contains("frame rate")
    );
    invalid("[[canvases]]\nid = 0\nname = \"M\"\nwidth = 0\nheight = 2\nframe_rate = [30, 1]\n");
    // Items place exactly one thing that exists.
    let scene = "[[scenes]]\nid = 0\nname = \"Sc\"\n[[scenes.items]]\nid = 0\n";
    let layout = "layout = { position = [0.0, 0.0], size = [1.0, 1.0] }\n";
    assert!(invalid(&format!("{SOURCE}{scene}{layout}")).contains("either source or scene"));
    assert!(
        invalid(&format!("{SOURCE}{scene}source = 0\nscene = 0\n{layout}"))
            .contains("Scenes cannot be placed inside other scenes.")
    );
    invalid(&format!("{scene}source = 0\n{layout}"));
    // Layouts and settings must be finite; settings must be plain values.
    invalid(&format!(
        "{SOURCE}{scene}source = 0\nlayout = {{ position = [nan, 0.0], size = [1.0, 1.0] }}\n"
    ));
    invalid(&format!(
        "{SOURCE}{scene}source = 0\nlayout = {{ position = [0.0, 0.0], size = [1.0, 1.0], opacity = 2.0 }}\n"
    ));
    invalid(&format!(
        "{SOURCE}{scene}source = 0\nlayout = {{ position = [1e300, 0.0], size = [1.0, 1.0] }}\n"
    ));
    invalid(&format!("{SOURCE}settings = {{ x = inf }}\n"));
    // Sound: the volume and sync offset ranges are part of the format.
    assert!(invalid(&format!("{SOURCE}volume = 1.5\n")).contains("volume"));
    assert!(invalid(&format!("{SOURCE}sync_offset_ms = 201\n")).contains("sync offset"));
    assert!(invalid(&format!("{SOURCE}sync_offset_ms = -201\n")).contains("sync offset"));
    invalid(&format!("{SOURCE}settings = {{ x = [1, 2] }}\n"));
    // Type ids and names.
    invalid("[[sources]]\nid = 0\nname = \"S\"\ntype = \"display\"\n");
    invalid("[[sources]]\nid = 0\nname = \" \"\ntype = \"org.rbs.source.display\"\n");
    // Duplicate ids and two overrides for one canvas.
    invalid(&format!("{SOURCE}{SOURCE}"));
    let two = format!(
        "{CANVAS}{SOURCE}{scene}source = 0\n{layout}overrides = [{{ canvas = 0, layout = {{ position = [0.0, 0.0], size = [1.0, 1.0] }} }}, {{ canvas = 0, layout = {{ position = [0.0, 0.0], size = [2.0, 2.0] }} }}]\n"
    );
    assert!(invalid(&two).contains("two layouts"));
    // Scope (U15): a canvas scene shown on another canvas.
    invalid(&format!(
        "{CANVAS}program = 0\n[[canvases]]\nid = 1\nname = \"V\"\nwidth = 2\nheight = 2\nframe_rate = [30, 1]\n[[scenes]]\nid = 0\nname = \"Sc\"\ncanvas = 1\n"
    ));
}

#[test]
fn collections_live_in_their_own_library() {
    let d = TempDir::new("scene-lib");
    let w = UserData::new(d.path()).try_lock().unwrap().unwrap();
    let data = UserData::new(d.path());
    let lib = data.scene_collections();
    let id = lib.create(&w, "Main show", &show()).unwrap();
    assert_eq!(
        lib.path(&id),
        d.path().join("scenes").join("main-show.toml")
    );
    assert_eq!(lib.load(&id).unwrap().document, show());
    // A profile file is not a collection.
    let p = data
        .profiles()
        .create(&w, "P", &rbs_user_data::OutputProfile::new())
        .unwrap();
    assert!(matches!(
        lib.import(&w, &data.profiles().path(&p)),
        Err(DataError::WrongKind { .. })
    ));
}

#[test]
fn every_scene_model_invariant_is_checked_on_load() {
    let two = "[[canvases]]\nid = 1\nname = \"V\"\nwidth = 2\nheight = 2\nframe_rate = [30, 1]\n";
    let lay = "layout = { position = [0.0, 0.0], size = [1.0, 1.0] }\n";
    let scene = |id: u32, extra: &str| format!("[[scenes]]\nid = {id}\nname = \"S{id}\"\n{extra}");
    // Nesting is rejected before cycle and crop validation.
    let cycle = format!(
        "{}[[scenes.items]]\nid = 0\nscene = 1\n{lay}{}[[scenes.items]]\nid = 1\nscene = 0\n{lay}",
        scene(0, ""),
        scene(1, "")
    );
    assert!(invalid(&cycle).contains("Scenes cannot be placed inside other scenes."));
    // Crop on a nested scene.
    let crop = format!(
        "{}{}[[scenes.items]]\nid = 0\nscene = 0\nlayout = {{ position = [0.0, 0.0], size = [1.0, 1.0], crop = [1, 0, 0, 0] }}\n",
        scene(0, ""),
        scene(1, "")
    );
    assert!(invalid(&crop).contains("Scenes cannot be placed inside other scenes."));
    // A per-canvas layout for a canvas that may not show the scene.
    let over = format!(
        "{CANVAS}{two}{SOURCE}{}[[scenes.items]]\nid = 0\nsource = 0\n{lay}overrides = [{{ canvas = 1, layout = {{ position = [0.0, 0.0], size = [1.0, 1.0] }} }}]\n",
        scene(0, "canvas = 0\n")
    );
    invalid(&over);
    // A shared scene nesting a canvas scene.
    let nest = format!(
        "{CANVAS}{}{}[[scenes.items]]\nid = 0\nscene = 0\n{lay}",
        scene(0, "canvas = 0\n"),
        scene(1, "")
    );
    invalid(&nest);
    // The same filter id on two sources.
    let filters = "filters = [{ id = 0, type = \"org.rbs.filter.blur\" }]\n";
    let dup = format!(
        "{SOURCE}{filters}[[sources]]\nid = 1\nname = \"T\"\ntype = \"org.rbs.source.display\"\n{filters}"
    );
    assert!(invalid(&dup).contains("used twice"));
    // Nesting is rejected before the former depth limit.
    let mut deep = String::new();
    for i in 0..=rbs_scene::MAX_NESTING_DEPTH as u32 {
        deep += &scene(i, "");
        deep += &format!("[[scenes.items]]\nid = {i}\nscene = {}\n{lay}", i + 1);
    }
    deep += &scene(rbs_scene::MAX_NESTING_DEPTH as u32 + 1, "");
    assert!(invalid(&deep).contains("Scenes cannot be placed inside other scenes."));
    // Canvas bounds: shared files are untrusted input.
    assert!(invalid("[[canvases]]\nid = 0\nname = \"M\"\nwidth = 4294967295\nheight = 2\nframe_rate = [30, 1]\n").contains("at most"));
    assert!(invalid("[[canvases]]\nid = 0\nname = \"M\"\nwidth = 2\nheight = 2\nframe_rate = [4294967295, 1]\n").contains("at most"));
    // A global scene (ADR-0065) that is shared, or that is also the program.
    let shared_globals = format!(
        "[[canvases]]\nid = 0\nname = \"M\"\nwidth = 2\nheight = 2\nframe_rate = [30, 1]\nglobals = 0\n{}",
        scene(0, "")
    );
    assert!(invalid(&shared_globals).contains("global scene"));
    let shown_globals = format!(
        "[[canvases]]\nid = 0\nname = \"M\"\nwidth = 2\nheight = 2\nframe_rate = [30, 1]\nprogram = 0\nglobals = 0\n{}",
        scene(0, "canvas = 0\n")
    );
    assert!(invalid(&shown_globals).contains("global scene"));
    // An id with no next id left.
    invalid(
        "[[canvases]]\nid = 4294967295\nname = \"M\"\nwidth = 2\nheight = 2\nframe_rate = [30, 1]\n",
    );
}

/// A small deterministic generator (no randomness between runs).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) % n as u64) as usize
    }
}

fn pick<T: Copy>(rng: &mut Lcg, v: &[T]) -> T {
    v[rng.next(v.len())]
}

#[test]
fn every_state_of_a_long_edit_sequence_round_trips() {
    let mut col = show();
    let mut rng = Lcg(7);
    for step in 0..200 {
        let canvases: Vec<_> = col.canvases().map(|c| c.id).collect();
        let sources: Vec<_> = col.sources().map(|s| s.id).collect();
        let scenes: Vec<_> = col.scenes().map(|s| s.id).collect();
        let items: Vec<_> = col
            .scenes()
            .flat_map(|s| s.items.iter().map(|i| i.id))
            .collect();
        // Refused edits are fine: the collection stays valid either way.
        let _ = match rng.next(11) {
            0 => col
                .add_source(&format!("S{step}"), ty("camera"), Settings::new())
                .map(|_| ()),
            1 if !sources.is_empty() => col.delete_source(pick(&mut rng, &sources)).map(|_| ()),
            2 => col
                .add_scene(
                    &format!("Sc{step}"),
                    if rng.next(2) == 0 {
                        SceneScope::Shared
                    } else {
                        SceneScope::Canvas(pick(&mut rng, &canvases))
                    },
                )
                .map(|_| ()),
            3 if !scenes.is_empty() && !sources.is_empty() => col
                .add_item(
                    pick(&mut rng, &scenes),
                    ItemContent::Source(pick(&mut rng, &sources)),
                    Layout::new([rng.next(100) as f32 / 3.0, 0.5], [10.0, 7.25]),
                    None,
                )
                .map(|_| ()),
            4 if scenes.len() > 1 => col
                .add_item(
                    pick(&mut rng, &scenes),
                    ItemContent::Scene(pick(&mut rng, &scenes)),
                    Layout::new([0.0, 0.0], [5.0, 5.0]).with_opacity(0.3),
                    None,
                )
                .map(|_| ()),
            5 if !items.is_empty() => col.remove_item(pick(&mut rng, &items)).map(|_| ()),
            6 if !items.is_empty() => col
                .set_item_layout(
                    pick(&mut rng, &items),
                    if rng.next(2) == 0 {
                        None
                    } else {
                        Some(pick(&mut rng, &canvases))
                    },
                    Layout::new([1.0, 2.0], [3.0, -4.0]).with_rotation(rng.next(360) as f32 * 0.7),
                )
                .map(|_| ()),
            7 if !scenes.is_empty() => col
                .set_program(pick(&mut rng, &canvases), Some(pick(&mut rng, &scenes)))
                .map(|_| ()),
            8 if !scenes.is_empty() => col.remove_scene(pick(&mut rng, &scenes)).map(|_| ()),
            10 if !items.is_empty() => col
                .set_item_locked(pick(&mut rng, &items), rng.next(2) == 0)
                .map(|_| ()),
            _ if !items.is_empty() => col
                .set_item_visible(pick(&mut rng, &items), rng.next(2) == 0)
                .map(|_| ()),
            _ => Ok(()),
        };
        let text = document::render(&col, Some("Show")).unwrap();
        let back = document::parse::<SceneCollection>(&text)
            .unwrap_or_else(|e| panic!("step {step}: {e}\n{text}"))
            .document;
        assert_eq!(back, col, "step {step}");
        assert_eq!(back.id_counters(), col.id_counters(), "step {step}");
    }
}

/// Measures a save of a large collection in a library of 20 (ADR-0051 consequence):
/// `cargo test -p rbs-user-data --release --test scenes -- --ignored --nocapture`.
#[test]
#[ignore = "slow: measures saving large collections"]
fn slow_save_cost_of_a_large_collection() {
    let d = TempDir::new("scene-bench");
    let w = UserData::new(d.path()).try_lock().unwrap().unwrap();
    let lib = UserData::new(d.path()).scene_collections();
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_60)
        .unwrap();
    let mut scenes = Vec::new();
    for s in 0..50 {
        scenes.push(
            col.add_scene(&format!("Scene {s}"), SceneScope::Canvas(canvas))
                .unwrap()
                .0,
        );
    }
    for i in 0..2000 {
        let mut settings = Settings::new();
        settings.insert(
            "target".to_owned(),
            SettingValue::Text(format!("display:{i}")),
        );
        let (src, _) = col
            .add_source(&format!("Source {i}"), ty("display"), settings)
            .unwrap();
        col.add_item(
            scenes[i % 50],
            ItemContent::Source(src),
            Layout::new([1.5, 2.5], [640.0, 360.0]),
            None,
        )
        .unwrap();
    }
    let mut ids = Vec::new();
    for n in 0..20 {
        ids.push(lib.create(&w, &format!("Show {n}"), &col).unwrap());
    }
    let entry = rbs_user_data::Entry::new("Show 0", col.clone());
    let t = std::time::Instant::now();
    let runs = 20;
    for _ in 0..runs {
        lib.save(&w, &ids[0], &entry).unwrap();
    }
    let save = t.elapsed() / runs;
    let t = std::time::Instant::now();
    let loaded = lib.load(&ids[0]).unwrap();
    let load = t.elapsed();
    let t = std::time::Instant::now();
    let listed = lib.list().unwrap();
    let list = t.elapsed();
    assert_eq!(loaded.document, col);
    assert_eq!(listed.entries.len(), 20);
    let size = std::fs::metadata(lib.path(&ids[0])).unwrap().len();
    println!("2000 sources/items, {size} bytes: save {save:?}, load {load:?}, list of 20 {list:?}");
}

#[test]
fn styles_are_checked_on_load() {
    let item = |style: &str| {
        format!(
            "{CANVAS}{SOURCE}[[scenes]]\nid = 0\nname = \"Sc\"\n[[scenes.items]]\nid = 0\nsource = 0\nlayout = {{ position = [0.0, 0.0], size = [10.0, 10.0], style = {style} }}\n"
        )
    };
    // A good one loads, in a format 1 file too (format 2 only adds keys).
    let text = format!(
        "format = 1\nkind = \"scene-collection\"\n{}",
        item("{ shape = \"ellipse\", border = { width = 2.0, color = \"#FFFFFF\" } }")
    );
    let col = document::parse::<SceneCollection>(&text).unwrap().document;
    let scene = col.scenes().next().unwrap();
    let style = scene.items[0].layout.style;
    assert_eq!(style.shape, Shape::Ellipse);
    assert_eq!(style.border, Some(Border::new(2.0, [255, 255, 255, 255])));
    for (style, why) in [
        ("{ shape = \"circle\", radius = 3.0 }", "radius"),
        ("{ shape = \"star\" }", "star"),
        ("{ border = { width = 2.0, color = \"red\" } }", "colour"),
        ("{ border = { width = 2.0, color = \"#12345\" } }", "colour"),
        (
            "{ border = { width = 0.0, color = \"#000000\" } }",
            "layout",
        ),
        ("{ radius = -1.0 }", "layout"),
        (
            "{ shadow = { blur = -2.0, color = \"#000000\" } }",
            "layout",
        ),
        ("{ glow = 1 }", "glow"),
    ] {
        let reason = invalid(&item(style));
        assert!(reason.contains(why), "{style}: {reason}");
    }
    // A scene placement is refused before its style is checked.
    let nested = format!(
        "{CANVAS}[[scenes]]\nid = 0\nname = \"A\"\n[[scenes]]\nid = 1\nname = \"B\"\n[[scenes.items]]\nid = 0\nscene = 0\nlayout = {{ position = [0.0, 0.0], size = [10.0, 10.0], style = {{ shape = \"circle\" }} }}\n"
    );
    assert!(invalid(&nested).contains("Scenes cannot be placed inside other scenes."));
}

#[test]
fn a_region_and_display_chrome_round_trip_and_an_older_file_refuses_them() {
    let mut col = SceneCollection::new();
    let (display, _) = col
        .add_source("Screen", ty("display"), Settings::new())
        .unwrap();
    col.set_display_chrome(
        display,
        Some(DisplayChrome::new(24, 0, 48, 0).with_fill(rbs_scene::ChromeFill::Fit)),
    )
    .unwrap();
    col.set_source_region(display, Some(SourceRegion::new(0, 24, 0, 48)))
        .unwrap();
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(text.contains("format = 15"), "{text}");
    assert!(text.contains("region = [0, 24, 0, 48]"), "{text}");
    assert!(text.contains("fill = \"fit\""), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    let source = back.source(display).unwrap();
    assert_eq!(source.region, Some(SourceRegion::new(0, 24, 0, 48)));
    assert_eq!(
        source.chrome.map(|c| c.fill),
        Some(rbs_scene::ChromeFill::Fit)
    );

    let old = "format = 13\nkind = \"scene-collection\"\n[[sources]]\nid = 0\nname = \"S\"\ntype = \"org.rbs.source.display\"\nregion = [1, 0, 0, 0]\n";
    assert!(document::parse::<SceneCollection>(old).is_err());
}

#[test]
fn a_changed_canvas_keeps_its_layout_space_in_the_file() {
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_30)
        .unwrap();
    col.set_canvas_format(canvas, Size::new(1080, 1920), FrameRate::FPS_30)
        .unwrap();
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(text.contains("format = 15"), "{text}");
    assert!(text.contains("layout_width = 1920"), "{text}");
    assert!(text.contains("layout_height = 1080"), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    let c = back.canvas(canvas).unwrap();
    assert_eq!((c.size.width, c.size.height), (1080, 1920));
    assert_eq!((c.layout_size.width, c.layout_size.height), (1920, 1080));

    let older = "format = 14\nkind = \"scene-collection\"\n[[canvases]]\nid = 0\nname = \"M\"\nwidth = 1080\nheight = 1920\nlayout_width = 1920\nlayout_height = 1080\nframe_rate = [30, 1]\n";
    match document::parse::<SceneCollection>(older) {
        Err(DataError::Invalid(BodyError::LaterKey { keys, since, .. })) => {
            assert!(keys.contains("layout_width"), "{keys}");
            assert_eq!(since, 15);
        }
        other => panic!("expected LaterKey, got {other:?}"),
    }
}

#[test]
fn a_source_recorded_separately_raises_the_format_and_an_older_file_refuses_it() {
    let mut plain = SceneCollection::new();
    plain
        .add_source("Mic", ty("audio-input"), Settings::new())
        .unwrap();
    let plain_text = document::render(&plain, Some("Show")).unwrap();
    assert!(plain_text.contains("format = 15"), "{plain_text}");
    assert!(!plain_text.contains("record_separately"), "{plain_text}");

    let mut col = SceneCollection::new();
    let (mic, _) = col
        .add_source("Mic", ty("audio-input"), Settings::new())
        .unwrap();
    col.set_source_record_separately(mic, RecordSeparately::new(false, true))
        .unwrap();
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(text.contains("format = 16"), "{text}");
    assert!(text.contains("[sources.record_separately]"), "{text}");
    assert!(text.contains("audio = true"), "{text}");
    assert!(!text.contains("video"), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert!(back.source(mic).unwrap().record_separately.audio);
    assert!(!back.source(mic).unwrap().record_separately.video);

    let older = "format = 15\nkind = \"scene-collection\"\n[[sources]]\nid = 0\nname = \"Mic\"\ntype = \"org.rbs.source.audio-input\"\nrecord_separately = { audio = true }\n";
    match document::parse::<SceneCollection>(older) {
        Err(DataError::Invalid(BodyError::LaterKey { keys, since, .. })) => {
            assert!(keys.contains("record_separately"), "{keys}");
            assert_eq!(since, 16);
        }
        other => panic!("expected LaterKey, got {other:?}"),
    }
}

#[test]
fn a_picture_form_raises_the_format_to_18_and_an_older_file_refuses_it() {
    let mut col = SceneCollection::new();
    let (logo, _) = col
        .add_source("Logo", ty("image"), Settings::new())
        .unwrap();
    col.set_source_record_separately(logo, RecordSeparately::new(true, false))
        .unwrap();
    // Automatic is the default and is not written: the format stays 16.
    let auto = document::render(&col, Some("Show")).unwrap();
    assert!(auto.contains("format = 16"), "{auto}");
    assert!(!auto.contains("video_as"), "{auto}");

    col.set_source_record_separately(
        logo,
        RecordSeparately::new(true, false).with_video_as(VideoAs::Video),
    )
    .unwrap();
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(text.contains("format = 18"), "{text}");
    assert!(text.contains("video_as = \"video\""), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(
        back.source(logo).unwrap().record_separately,
        RecordSeparately::new(true, false).with_video_as(VideoAs::Video)
    );

    // A form without the picture switch is not written.
    col.set_source_record_separately(
        logo,
        RecordSeparately::new(false, true).with_video_as(VideoAs::Image),
    )
    .unwrap();
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(!text.contains("video_as"), "{text}");

    let body = "kind = \"scene-collection\"\n[[sources]]\nid = 0\nname = \"Logo\"\ntype = \"org.rbs.source.image\"\nrecord_separately = { video = true, video_as = \"image\" }\n";
    match document::parse::<SceneCollection>(&format!("format = 17\n{body}")) {
        Err(DataError::Invalid(BodyError::LaterKey { keys, since, .. })) => {
            assert!(keys.contains("video_as"), "{keys}");
            assert_eq!(since, 18);
        }
        other => panic!("expected LaterKey, got {other:?}"),
    }
    let ok = document::parse::<SceneCollection>(&format!("format = 18\n{body}"))
        .unwrap()
        .document;
    let source = ok.sources().next().unwrap();
    assert_eq!(source.record_separately.video_as, VideoAs::Image);
    let bad = "format = 18\nkind = \"scene-collection\"\n[[sources]]\nid = 0\nname = \"Logo\"\ntype = \"org.rbs.source.image\"\nrecord_separately = { video = true, video_as = \"gif\" }\n";
    assert!(matches!(
        document::parse::<SceneCollection>(bad),
        Err(DataError::Invalid(_))
    ));
}

#[test]
fn scene_levels_raise_the_format_round_trip_and_an_older_file_refuses_them() {
    let mut col = SceneCollection::new();
    let (mic, _) = col
        .add_source("Mic", ty("audio-input"), Settings::new())
        .unwrap();
    let (music, _) = col
        .add_source("Music", ty("audio-input"), Settings::new())
        .unwrap();
    let (talk, _) = col.add_scene("Talking", SceneScope::Shared).unwrap();
    let (brb, _) = col.add_scene("Be right back", SceneScope::Shared).unwrap();
    let plain = document::render(&col, Some("Show")).unwrap();
    assert!(!plain.contains("format = 17"), "{plain}");
    assert!(!plain.contains("[scenes.audio]"), "{plain}");

    col.set_scene_own_levels(talk, SceneAudio::default())
        .unwrap();
    col.set_scene_level(talk, mic, Some(SceneLevel::new(None, Some(true))))
        .unwrap();
    col.set_scene_level(talk, music, Some(SceneLevel::new(Some(0.25), None)))
        .unwrap();
    // Own levels with no entries is kept too.
    col.set_scene_own_levels(brb, SceneAudio::default())
        .unwrap();
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(text.contains("format = 17"), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(
        back.scene(talk).unwrap().audio,
        col.scene(talk).unwrap().audio
    );
    assert!(back.scene(brb).unwrap().own_levels());
    assert!(
        back.scene(brb)
            .unwrap()
            .audio
            .as_ref()
            .unwrap()
            .levels
            .is_empty()
    );

    let scene = "[[scenes]]\nid = 0\nname = \"S\"\n";
    let older = format!(
        "format = 16\nkind = \"scene-collection\"\n[next]\nscene = 1\n{scene}[scenes.audio]\n"
    );
    match document::parse::<SceneCollection>(&older) {
        Err(DataError::Invalid(BodyError::LaterKey { keys, since, .. })) => {
            assert!(keys.contains("audio"), "{keys}");
            assert_eq!(since, 17);
        }
        other => panic!("expected LaterKey, got {other:?}"),
    }
    // A level for a source that does not exist is refused (scene-model invariant 4).
    let dangling = format!(
        "format = 17\nkind = \"scene-collection\"\n[next]\nscene = 1\n{scene}audio = {{ levels = [{{ source = 7, muted = true }}] }}\n"
    );
    assert!(matches!(
        document::parse::<SceneCollection>(&dangling),
        Err(DataError::Invalid(_))
    ));
    // So is a volume out of range.
    let loud = format!(
        "format = 17\nkind = \"scene-collection\"\n[next]\nsource = 1\nscene = 1\n[[sources]]\nid = 0\nname = \"Mic\"\ntype = \"org.rbs.source.mic\"\n{scene}audio = {{ levels = [{{ source = 0, volume = 2.0 }}] }}\n"
    );
    assert!(matches!(
        document::parse::<SceneCollection>(&loud),
        Err(DataError::Invalid(_))
    ));
}

#[test]
fn format_18_loads_as_move_or_cut_and_format_19_round_trips() {
    use rbs_scene::{CollectionTransition, SwipeDirection, TransitionKind};
    let moving = format!("format = 18\nkind = \"scene-collection\"\n{CANVAS}{SOURCE}");
    let col = document::parse::<SceneCollection>(&moving)
        .unwrap()
        .document;
    assert_eq!(col.transition(), CollectionTransition::default());
    assert_eq!(col.transition().kind, TransitionKind::Move);

    let still = format!(
        "format = 18\nkind = \"scene-collection\"\nglide = {{ duration_ms = 0 }}\n{CANVAS}{SOURCE}"
    );
    let col = document::parse::<SceneCollection>(&still).unwrap().document;
    assert_eq!(col.transition().kind, TransitionKind::Cut);
    // Written again it stays Cut: the file goes to format 19.
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(text.contains("format = 19"), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(back, col);

    // The default is not written and keeps the lowest format.
    let plain = document::render(&show(), Some("Show")).unwrap();
    assert!(!plain.contains("transition"), "{plain}");
    assert!(!plain.contains("format = 19"), "{plain}");

    let mut col = show();
    let mut t = CollectionTransition {
        kind: TransitionKind::Fade,
        ..Default::default()
    };
    t.fade.duration_ms = 750;
    t.fade.via = Some([0x12, 0xab, 0xff]);
    t.slide.direction = SwipeDirection::Up;
    col.set_transition(t).unwrap();
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(text.contains("format = 19"), "{text}");
    assert!(text.contains("kind = \"fade\""), "{text}");
    assert!(text.contains("#12abff"), "{text}");
    assert!(!text.contains("swipe"), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(back.transition(), t);
    assert_eq!(back, col);
}

#[test]
fn the_kind_outside_studio_mode_round_trips_in_format_27_and_older_formats_refuse_it() {
    use rbs_scene::{CollectionTransition, TransitionKind};
    // Absent: Move, and the file keeps its lower format (ADR-0153).
    let plain = document::render(&show(), Some("Show")).unwrap();
    assert!(!plain.contains("direct"), "{plain}");
    assert!(!plain.contains("format = 27"), "{plain}");
    let older = format!(
        "format = 26\nkind = \"scene-collection\"\n[transition]\nkind = \"slide\"\n{CANVAS}{SOURCE}"
    );
    let col = document::parse::<SceneCollection>(&older).unwrap().document;
    assert_eq!(col.transition().kind, TransitionKind::Slide);
    assert_eq!(col.transition().direct, TransitionKind::Move);

    let mut col = show();
    col.set_transition(CollectionTransition {
        kind: TransitionKind::Slide,
        direct: TransitionKind::Fade,
        ..Default::default()
    })
    .unwrap();
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(text.contains("format = 27"), "{text}");
    assert!(text.contains("direct = \"fade\""), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(back, col);

    // A format 26 file cannot carry it.
    let hand = text.replace("format = 27", "format = 26");
    match document::parse::<SceneCollection>(&hand) {
        Err(DataError::Invalid(BodyError::LaterKey { keys, since, .. })) => {
            assert!(keys.contains("transition.direct"), "{keys}");
            assert_eq!(since, 27);
        }
        other => panic!("expected LaterKey, got {other:?}"),
    }
    // An unknown kind is refused.
    let odd = text.replace("direct = \"fade\"", "direct = \"spin\"");
    assert!(matches!(
        document::parse::<SceneCollection>(&odd),
        Err(DataError::Invalid(_))
    ));
}

#[test]
fn a_transition_in_an_older_format_or_out_of_range_is_refused() {
    let why = invalid("transition = { kind = \"fade\" }\n");
    assert!(why.contains("transition"), "{why}");
    for bad in [
        "transition = { kind = \"wipe\" }\n",
        "transition = { fade = { duration_ms = 9000, easing = \"linear\" } }\n",
        "transition = { fade = { duration_ms = 300, easing = \"linear\", via = \"black\" } }\n",
        "transition = { swipe = { duration_ms = 300, easing = \"linear\", direction = \"sideways\" } }\n",
    ] {
        let text = format!("format = 19\nkind = \"scene-collection\"\n{bad}");
        assert!(
            matches!(
                document::parse::<SceneCollection>(&text),
                Err(DataError::Invalid(_))
            ),
            "{bad}"
        );
    }
}

#[test]
fn a_scene_transition_round_trips_in_format_20_and_older_formats_refuse_it() {
    // ADR-0135 decision 9.
    use rbs_scene::{SceneTransition, TransitionKind};
    let mut col = show();
    let scene = col.scenes().next().unwrap().id;
    // Without an override the format stays below 20.
    let plain = document::render(&col, Some("Show")).unwrap();
    assert!(!plain.contains("transition_in"), "{plain}");
    assert!(!plain.contains("format = 20"), "{plain}");
    for t in [
        SceneTransition {
            kind: Some(TransitionKind::Swipe),
            duration_ms: Some(800),
        },
        SceneTransition {
            kind: None,
            duration_ms: Some(0),
        },
        SceneTransition {
            kind: Some(TransitionKind::Cut),
            duration_ms: None,
        },
    ] {
        col.set_scene_transition(scene, Some(t)).unwrap();
        let text = document::render(&col, Some("Show")).unwrap();
        assert!(text.contains("format = 20"), "{text}");
        let back = document::parse::<SceneCollection>(&text).unwrap().document;
        assert_eq!(back.scene(scene).unwrap().transition_in, Some(t));
        assert_eq!(back, col);
    }
    let scene_doc = "[[scenes]]\nid = 0\nname = \"A\"\n";
    for (format, bad) in [
        (19, "transition_in = { kind = \"fade\" }\n"),
        (20, "transition_in = { kind = \"wipe\" }\n"),
        (20, "transition_in = { duration_ms = 9000 }\n"),
        (
            20,
            "transition_in = { kind = \"fade\", easing = \"linear\" }\n",
        ),
    ] {
        let text = format!(
            "format = {format}\nkind = \"scene-collection\"\n[next]\nscene = 1\n{scene_doc}{bad}"
        );
        assert!(
            matches!(
                document::parse::<SceneCollection>(&text),
                Err(DataError::Invalid(_))
            ),
            "{format} {bad}"
        );
    }
    // An empty table is no override.
    let text = format!(
        "format = 20\nkind = \"scene-collection\"\n[next]\nscene = 1\n{scene_doc}transition_in = {{}}\n"
    );
    let col = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(col.scenes().next().unwrap().transition_in, None);
}

#[test]
fn a_stinger_round_trips_in_format_21_and_older_formats_refuse_it() {
    // ADR-0140 decision 1.
    use rbs_scene::{CollectionTransition, StingerSettings, TransitionKind};
    let mut col = show();
    let plain = document::render(&col, Some("Show")).unwrap();
    assert!(!plain.contains("format = 21"), "{plain}");
    let (clip, _) = col
        .add_source("Stinger", ty("media-file"), Settings::new())
        .unwrap();
    for stinger in [
        StingerSettings {
            source: Some(clip),
            cut_ms: Some(700),
        },
        StingerSettings {
            source: None,
            cut_ms: None,
        },
    ] {
        col.set_transition(CollectionTransition {
            kind: TransitionKind::Stinger,
            stinger,
            ..CollectionTransition::default()
        })
        .unwrap();
        let text = document::render(&col, Some("Show")).unwrap();
        assert!(text.contains("format = 21"), "{text}");
        let back = document::parse::<SceneCollection>(&text).unwrap().document;
        assert_eq!(back.transition().stinger, stinger);
        assert_eq!(back, col);
    }
    for (format, bad) in [
        (20, "[transition]\nstinger = { cut_ms = 500 }\n"),
        (21, "[transition]\nstinger = { cut = 500 }\n"),
        (21, "[transition]\nstinger = { source = 7 }\n"),
    ] {
        let text = format!("format = {format}\nkind = \"scene-collection\"\n{bad}");
        assert!(
            matches!(
                document::parse::<SceneCollection>(&text),
                Err(DataError::Invalid(_))
            ),
            "{format} {bad}"
        );
    }
}

#[test]
fn a_scroll_round_trips_in_format_23_and_older_formats_refuse_it() {
    // ADR-0150 decision 1.
    use rbs_scene::model::{Scroll, ScrollDirection, ScrollRepeat, ScrollStart};
    let mut col = show();
    let plain = document::render(&col, Some("Show")).unwrap();
    assert!(!plain.contains("format = 23"), "{plain}");
    assert!(!plain.contains("scroll"), "{plain}");
    let (text_source, _) = col
        .add_source("Ticker", ty("text"), Settings::new())
        .unwrap();
    let (scene, _) = col.add_scene("News", SceneScope::Shared).unwrap();
    let tall = col.canvases().nth(1).unwrap().id;
    let full = Scroll::new(ScrollDirection::Up, 120.5)
        .with_gap(48.0)
        .with_repeat(ScrollRepeat::Once)
        .with_start(ScrollStart::Outside)
        .with_fade(24.5)
        .with_ramp_ms(0);
    for (default, over) in [
        (Some(Scroll::new(ScrollDirection::Left, 80.0)), None),
        (None, Some(full)),
        (
            Some(full),
            Some(Scroll::new(ScrollDirection::Right, 4000.0)),
        ),
    ] {
        let (item, _) = col
            .add_item(
                scene,
                ItemContent::Source(text_source),
                Layout::new([0.0, 980.0], [1920.0, 100.0]).with_scroll(default),
                None,
            )
            .unwrap();
        if over.is_some() {
            col.set_item_layout(
                item,
                Some(tall),
                Layout::new([0.0, 1800.0], [1080.0, 100.0]).with_scroll(over),
            )
            .unwrap();
        }
        let text = document::render(&col, Some("Show")).unwrap();
        assert!(text.contains("format = 23"), "{text}");
        let back = document::parse::<SceneCollection>(&text).unwrap().document;
        assert_eq!(back, col);
    }
    // Defaults are left out: a plain left scroll is only its speed.
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(
        text.contains("[scenes.items.layout.scroll]\nspeed = 80.0\n\n"),
        "{text}"
    );

    let scene_doc = "[[canvases]]\nid = 0\nname = \"C\"\nwidth = 1920\nheight = 1080\nframe_rate = [30, 1]\n\
                     [[sources]]\nid = 0\nname = \"T\"\ntype = \"org.rbs.source.text\"\n\
                     [[scenes]]\nid = 0\nname = \"S\"\n[[scenes.items]]\nid = 0\nsource = 0\n";
    for (format, bad) in [
        // Older formats have no scroll.
        (
            21,
            "layout = { position = [0.0, 0.0], size = [10.0, 10.0], scroll = { speed = 10.0 } }\n",
        ),
        (
            22,
            "layout = { position = [0.0, 0.0], size = [10.0, 10.0], scroll = { speed = 10.0 } }\n",
        ),
        (
            15,
            "layout = { position = [0.0, 0.0], size = [10.0, 10.0], scroll = { speed = 10.0 } }\n",
        ),
        (
            21,
            "layout = { position = [0.0, 0.0], size = [10.0, 10.0] }\n\
              overrides = [{ canvas = 0, layout = { position = [0.0, 0.0], size = [10.0, 10.0], scroll = { speed = 10.0 } } }]\n",
        ),
        // Out of range, unknown names and keys.
        (
            23,
            "layout = { position = [0.0, 0.0], size = [10.0, 10.0], scroll = { speed = 0.5 } }\n",
        ),
        (
            23,
            "layout = { position = [0.0, 0.0], size = [10.0, 10.0], scroll = { speed = 4001.0 } }\n",
        ),
        (
            23,
            "layout = { position = [0.0, 0.0], size = [10.0, 10.0], scroll = { speed = 10.0, gap = -1.0 } }\n",
        ),
        (
            23,
            "layout = { position = [0.0, 0.0], size = [10.0, 10.0], scroll = { speed = 10.0, fade = 2049.0 } }\n",
        ),
        (
            23,
            "layout = { position = [0.0, 0.0], size = [10.0, 10.0], scroll = { speed = 10.0, ramp_ms = 5001 } }\n",
        ),
        (
            23,
            "layout = { position = [0.0, 0.0], size = [10.0, 10.0], scroll = { speed = 10.0, direction = \"sideways\" } }\n",
        ),
        (
            23,
            "layout = { position = [0.0, 0.0], size = [10.0, 10.0], scroll = { speed = 10.0, bounce = true } }\n",
        ),
        (
            23,
            "layout = { position = [0.0, 0.0], size = [10.0, 10.0], scroll = { direction = \"up\" } }\n",
        ),
    ] {
        let text = format!(
            "format = {format}\nkind = \"scene-collection\"\n[next]\nscene = 1\nsource = 1\nitem = 1\ncanvas = 1\n{scene_doc}{bad}"
        );
        assert!(
            matches!(
                document::parse::<SceneCollection>(&text),
                Err(DataError::Invalid(_))
            ),
            "{format} {bad}"
        );
    }
    // The same document without the scroll loads in format 21.
    let ok = format!(
        "format = 21\nkind = \"scene-collection\"\n[next]\nscene = 1\nsource = 1\nitem = 1\ncanvas = 1\n{scene_doc}layout = {{ position = [0.0, 0.0], size = [10.0, 10.0] }}\n"
    );
    document::parse::<SceneCollection>(&ok).unwrap();
}

#[test]
fn browser_page_settings_are_written_as_format_22() {
    // ADR-0132 decision 11, ADR-0149 decision 1.
    use rbs_scene::model::SettingValue;
    let mut col = show();
    let mut page = Settings::new();
    page.insert(
        "url".into(),
        SettingValue::Text("https://example.com/".into()),
    );
    let (id, _) = col
        .add_source(
            "Page",
            NodeTypeId::new("org.rbs.source.browser"),
            page.clone(),
        )
        .unwrap();
    let plain = document::render(&col, Some("Show")).unwrap();
    assert!(!plain.contains("format = 22"), "{plain}");
    page.insert("zoom_percent".into(), SettingValue::Int(150));
    page.insert("script".into(), SettingValue::Text("1 + 1".into()));
    col.set_source_settings(id, page).unwrap();
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(text.contains("format = 22"), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(back, col);
    // A format 21 file reads as before.
    let old = plain.replace("format = 15", "format = 21");
    assert!(document::parse::<SceneCollection>(&old).is_ok());
}

#[test]
fn a_source_activity_round_trips_in_format_24_and_older_formats_refuse_it() {
    // ADR-0152 decision 10.
    use rbs_scene::activity::{OnReturn, Run, SourceActivity};
    let mut col = show();
    let plain = document::render(&col, Some("Show")).unwrap();
    assert!(!plain.contains("format = 24"), "{plain}");
    assert!(!plain.contains("activity"), "{plain}");
    let (film, _) = col
        .add_source("Film", ty("media"), Settings::new())
        .unwrap();
    // An `on_return` needs format 25: a format 24 build plays a film by its old key.
    for (policy, format) in [
        (SourceActivity::new(Some(Run::OnAir), None, None), 24),
        (SourceActivity::new(None, Some(OnReturn::Resume), None), 25),
        (
            SourceActivity::new(Some(Run::Placed), Some(OnReturn::Restart), Some(600)),
            25,
        ),
    ] {
        col.set_source_activity(film, policy).unwrap();
        let text = document::render(&col, Some("Show")).unwrap();
        assert!(text.contains(&format!("format = {format}")), "{text}");
        let back = document::parse::<SceneCollection>(&text).unwrap().document;
        assert_eq!(back, col);
    }

    let scene_doc = "[[canvases]]\nid = 0\nname = \"C\"\nwidth = 1920\nheight = 1080\nframe_rate = [30, 1]\n\
                     [[sources]]\nid = 0\nname = \"F\"\ntype = \"org.rbs.source.media\"\n";
    for (format, extra, ok) in [
        (23, "activity = { run = \"on_air\" }\n", false),
        (24, "activity = { run = \"on_air\" }\n", true),
        (24, "activity = { run = \"always\" }\n", false),
        (24, "activity = { on_return = \"rewind\" }\n", false),
        (24, "activity = { linger_s = 601 }\n", false),
        (24, "activity = { run = \"shown\", colour = 1 }\n", false),
        // A value a type does not allow still loads (the binder reports it).
        (
            24,
            "activity = { run = \"shown\", on_return = \"continue\" }\n",
            true,
        ),
    ] {
        let text = format!("kind = \"scene-collection\"\nformat = {format}\n{scene_doc}{extra}");
        let parsed = document::parse::<SceneCollection>(&text);
        assert_eq!(parsed.is_ok(), ok, "{format} {extra}: {parsed:?}");
    }
}

#[test]
fn format_24_restart_when_shown_becomes_the_media_on_return() {
    // ADR-0152 decision 10: true is `restart`, false `resume`, the key is dropped; a
    // saved `on_return` wins; another type's key of that name is left alone.
    use rbs_scene::activity::OnReturn;
    let head = "kind = \"scene-collection\"\nformat = 24\n[[canvases]]\nid = 0\nname = \"C\"\n\
                width = 1920\nheight = 1080\nframe_rate = [30, 1]\n";
    let media = |id: u32, extra: &str| {
        format!(
            "[[sources]]\nid = {id}\nname = \"F{id}\"\ntype = \"org.rbs.source.media\"\n{extra}"
        )
    };
    let text = format!(
        "{head}{}{}{}{}",
        media(
            0,
            "settings = { restart_when_shown = false, loop = true }\n"
        ),
        media(1, "settings = { restart_when_shown = true }\n"),
        media(
            2,
            "settings = { restart_when_shown = false }\nactivity = { on_return = \"restart\" }\n"
        ),
        media(3, ""),
    );
    let col = document::parse::<SceneCollection>(&text).unwrap().document;
    let on_return = |id: u32| {
        let s = col.sources().find(|s| s.name == format!("F{id}")).unwrap();
        assert!(!s.settings.contains_key("restart_when_shown"));
        s.activity.on_return
    };
    assert_eq!(on_return(0), Some(OnReturn::Resume));
    assert_eq!(on_return(1), Some(OnReturn::Restart));
    assert_eq!(on_return(2), Some(OnReturn::Restart));
    assert_eq!(on_return(3), None);
    let f0 = col.sources().find(|s| s.name == "F0").unwrap();
    assert!(f0.settings.contains_key("loop"));
    let out = document::render(&col, Some("Show")).unwrap();
    assert!(out.contains("format = 25"), "{out}");
    assert!(!out.contains("restart_when_shown"), "{out}");
    // A non-flag value stays, so the source reports the bad setting.
    let bad = format!(
        "{head}{}",
        media(0, "settings = { restart_when_shown = 1 }\n")
    );
    let col = document::parse::<SceneCollection>(&bad).unwrap().document;
    assert!(
        col.sources()
            .next()
            .unwrap()
            .settings
            .contains_key("restart_when_shown")
    );
}

#[test]
fn format_25_keep_running_becomes_the_page_run_placed() {
    // ADR-0152 decision 10: true is `run = "placed"`, false the default, the key is
    // dropped; a saved `run` wins.
    use rbs_scene::activity::Run;
    let head = "kind = \"scene-collection\"\nformat = 25\n[[canvases]]\nid = 0\nname = \"C\"\n\
                width = 1920\nheight = 1080\nframe_rate = [30, 1]\n";
    let page = |id: u32, extra: &str| {
        format!(
            "[[sources]]\nid = {id}\nname = \"P{id}\"\ntype = \"org.rbs.source.browser\"\n{extra}"
        )
    };
    let url = "url = \"https://example.com/\"";
    let text = format!(
        "{head}{}{}{}",
        page(0, &format!("settings = {{ {url}, keep_running = true }}\n")),
        page(
            1,
            &format!("settings = {{ {url}, keep_running = false }}\n")
        ),
        page(
            2,
            &format!(
                "settings = {{ {url}, keep_running = true }}\nactivity = {{ run = \"on_air\" }}\n"
            )
        ),
    );
    let col = document::parse::<SceneCollection>(&text).unwrap().document;
    let run = |id: u32| {
        let s = col.sources().find(|s| s.name == format!("P{id}")).unwrap();
        assert!(!s.settings.contains_key("keep_running"));
        s.activity.run
    };
    assert_eq!(run(0), Some(Run::Placed));
    assert_eq!(run(1), None);
    assert_eq!(run(2), Some(Run::OnAir));
    let out = document::render(&col, Some("Show")).unwrap();
    assert!(out.contains("format = 26"), "{out}");
    assert!(!out.contains("keep_running"), "{out}");
}

#[test]
fn a_continuing_scroll_round_trips_in_format_28() {
    // ADR-0152 decision 6, phase 5: `on_return = "continue"`; `restart` is left out.
    use rbs_scene::activity::OnReturn;
    use rbs_scene::model::{Scroll, ScrollDirection};
    let mut col = show();
    let (ticker, _) = col
        .add_source("Ticker", ty("text"), Settings::new())
        .unwrap();
    let (scene, _) = col.add_scene("News", SceneScope::Shared).unwrap();
    let scroll = Scroll::new(ScrollDirection::Left, 80.0);
    let (item, _) = col
        .add_item(
            scene,
            ItemContent::Source(ticker),
            Layout::new([0.0, 980.0], [1920.0, 100.0]).with_scroll(Some(scroll)),
            None,
        )
        .unwrap();
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(!text.contains("on_return"), "{text}");
    assert!(!text.contains("format = 28"), "{text}");
    col.set_item_layout(
        item,
        None,
        Layout::new([0.0, 980.0], [1920.0, 100.0])
            .with_scroll(Some(scroll.with_on_return(OnReturn::Continue))),
    )
    .unwrap();
    let text = document::render(&col, Some("Show")).unwrap();
    assert!(text.contains("format = 28"), "{text}");
    assert!(text.contains("on_return = \"continue\""), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(back, col);
    // `resume` is not a scroll's.
    let bad = text.replace("on_return = \"continue\"", "on_return = \"resume\"");
    assert!(document::parse::<SceneCollection>(&bad).is_err());
}
