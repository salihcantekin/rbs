//! The scene collection document (user data spec, "Scene collection document").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::TempDir;
use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::SceneCollection;
use rbs_scene::model::{Crop, ItemContent, Layout, SceneScope, SettingValue, Settings};
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

    let (talk, _) = col.add_scene("Talk", SceneScope::Shared).unwrap();
    let (cam_item, _) = col
        .add_item(
            talk,
            ItemContent::Source(cam),
            Layout::new([0.0, 0.0], [1920.0, 1080.0]),
            None,
        )
        .unwrap();
    col.set_item_layout(
        cam_item,
        Some(tall),
        Layout::new([0.0, 420.0], [1080.0, 1080.0]).with_crop(Crop::new(420, 0, 420, 0)),
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
    let data = UserData::new(d.path());
    let lib = data.scene_collections();
    let id = lib.create("Main show", &show()).unwrap();
    assert_eq!(
        lib.path(&id),
        d.path().join("scenes").join("main-show.toml")
    );
    assert_eq!(lib.load(&id).unwrap().document, show());
    // A profile file is not a collection.
    let p = data
        .profiles()
        .create("P", &rbs_user_data::OutputProfile::new())
        .unwrap();
    assert!(matches!(
        lib.import(&data.profiles().path(&p)),
        Err(DataError::WrongKind { .. })
    ));
}
