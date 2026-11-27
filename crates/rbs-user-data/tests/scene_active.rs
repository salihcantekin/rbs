//! The scene Active flag in the scene collection document (format 32, ADR-0195).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_scene::SceneCollection;
use rbs_scene::model::SceneScope;
use rbs_user_data::document;

fn two_scenes() -> (SceneCollection, rbs_scene::model::SceneId) {
    let mut col = SceneCollection::new();
    col.add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_30)
        .unwrap();
    col.add_scene("A", SceneScope::Shared).unwrap();
    let (b, _) = col.add_scene("B", SceneScope::Shared).unwrap();
    (col, b)
}

#[test]
fn an_inactive_scene_round_trips_only_in_format_32() {
    let (mut col, b) = two_scenes();
    let plain = document::render(&col, None).unwrap();
    assert!(!plain.contains("active"), "{plain}");
    assert!(!plain.starts_with("format = 32"), "{plain}");
    col.set_scene_active(b, false).unwrap();
    let text = document::render(&col, None).unwrap();
    assert!(text.starts_with("format = 32"), "{text}");
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert_eq!(back, col);
    assert!(!back.scene(b).unwrap().active);
    // An older format must not carry the key.
    assert!(
        document::parse::<SceneCollection>(&text.replacen("format = 32", "format = 31", 1))
            .is_err()
    );
}

#[test]
fn an_older_document_loads_with_every_scene_active() {
    let (col, _) = two_scenes();
    let text = document::render(&col, None).unwrap();
    let back = document::parse::<SceneCollection>(&text).unwrap().document;
    assert!(back.scenes().all(|s| s.active));
}
