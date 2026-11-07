//! The scene collection document (user data spec, "Scene collection document").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::TempDir;
use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::SceneCollection;
use rbs_scene::model::{
    Border, Crop, ItemContent, ItemStyle, Layout, SceneScope, SettingValue, Settings, Shadow, Shape,
};
use rbs_user_data::document;
use rbs_user_data::{DataError, UserData};

fn ty(name: &str) -> NodeTypeId {
    NodeTypeId::new(format!("org.rbs.source.{name}"))
}

/// Two canvases, a shared scene nested in a canvas scene, filters, every setting type,
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
            ItemContent::Scene(talk),
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
        "format = 3",
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
        Err(DataError::Invalid(why)) => why,
        other => panic!("expected Invalid for {body:?}, got {other:?}"),
    }
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
    assert!(invalid(&format!("{SOURCE}{scene}source = 0\nscene = 0\n{layout}")).contains("either"));
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
    // A nesting cycle.
    let cycle = format!(
        "{}[[scenes.items]]\nid = 0\nscene = 1\n{lay}{}[[scenes.items]]\nid = 1\nscene = 0\n{lay}",
        scene(0, ""),
        scene(1, "")
    );
    assert!(
        invalid(&cycle).contains("contain itself"),
        "{}",
        invalid(&cycle)
    );
    // Crop on a nested scene.
    let crop = format!(
        "{}{}[[scenes.items]]\nid = 0\nscene = 0\nlayout = {{ position = [0.0, 0.0], size = [1.0, 1.0], crop = [1, 0, 0, 0] }}\n",
        scene(0, ""),
        scene(1, "")
    );
    assert!(invalid(&crop).contains("crop"));
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
    // Nesting deeper than the limit.
    let mut deep = String::new();
    for i in 0..=rbs_scene::MAX_NESTING_DEPTH as u32 {
        deep += &scene(i, "");
        deep += &format!("[[scenes.items]]\nid = {i}\nscene = {}\n{lay}", i + 1);
    }
    deep += &scene(rbs_scene::MAX_NESTING_DEPTH as u32 + 1, "");
    assert!(invalid(&deep).contains("nests more"));
    // Canvas bounds: shared files are untrusted input.
    assert!(invalid("[[canvases]]\nid = 0\nname = \"M\"\nwidth = 4294967295\nheight = 2\nframe_rate = [30, 1]\n").contains("at most"));
    assert!(invalid("[[canvases]]\nid = 0\nname = \"M\"\nwidth = 2\nheight = 2\nframe_rate = [4294967295, 1]\n").contains("at most"));
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
    // Shape and border are for sources, not nested scenes.
    let nested = format!(
        "{CANVAS}[[scenes]]\nid = 0\nname = \"A\"\n[[scenes]]\nid = 1\nname = \"B\"\n[[scenes.items]]\nid = 0\nscene = 0\nlayout = {{ position = [0.0, 0.0], size = [10.0, 10.0], style = {{ shape = \"circle\" }} }}\n"
    );
    assert!(invalid(&nested).contains("shape and border"));
}
