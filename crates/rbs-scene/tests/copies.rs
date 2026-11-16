//! Duplicate source and Duplicate scene with new sources (ADR-0133 decisions 3, 4).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::BTreeMap;

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{
    ItemContent, ItemId, Layout, SceneId, SceneLevel, SceneScope, SettingValue, Settings, SourceId,
};
use rbs_scene::{COMPANION_OF_HINT, STANDBY_HINT, SceneAudio, SceneCollection, SourceCopy};

fn ty(name: &str) -> NodeTypeId {
    NodeTypeId::new(format!("org.rbs.source.{name}"))
}

fn ducker() -> NodeTypeId {
    NodeTypeId::new("org.rbs.audio-filter.ducker")
}

fn keys(t: &NodeTypeId) -> Vec<String> {
    if *t == ducker() {
        vec!["key_source".to_owned()]
    } else {
        Vec::new()
    }
}

struct Fixture {
    col: SceneCollection,
    scene: SceneId,
    picture: SourceId,
    sound: SourceId,
    mic: SourceId,
    picture_item: ItemId,
    sound_item: ItemId,
}

/// A window with its sound companion, and a microphone, in one scene; the companion
/// is ducked by the microphone.
fn fixture() -> Fixture {
    let mut col = SceneCollection::new();
    col.add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_60)
        .unwrap();
    let (scene, _) = col.add_scene("Scene", SceneScope::Shared).unwrap();
    let (picture, _) = col
        .add_source("Chrome", ty("window-capture"), Settings::new())
        .unwrap();
    let (sound, _) = col
        .add_source("Chrome sound", ty("audio-input"), Settings::new())
        .unwrap();
    let (mic, _) = col
        .add_source("Mic", ty("audio-input"), Settings::new())
        .unwrap();
    let mut hints = BTreeMap::new();
    hints.insert(COMPANION_OF_HINT.to_owned(), picture.get().to_string());
    col.set_source_settings_and_hints(sound, Settings::new(), hints)
        .unwrap();
    let mut key = Settings::new();
    key.insert("key_source".into(), SettingValue::Int(i64::from(mic.get())));
    col.add_filter(sound, ducker(), key).unwrap();
    let full = Layout::new([0.0, 0.0], [1920.0, 1080.0]);
    let (picture_item, _) = col
        .add_item(scene, ItemContent::Source(picture), full, None)
        .unwrap();
    let (sound_item, _) = col
        .add_item(scene, ItemContent::Source(sound), full, None)
        .unwrap();
    col.add_item(scene, ItemContent::Source(mic), full, None)
        .unwrap();
    col.set_item_locked(picture_item, true).unwrap();
    Fixture {
        col,
        scene,
        picture,
        sound,
        mic,
        picture_item,
        sound_item,
    }
}

fn order(col: &SceneCollection, scene: SceneId) -> Vec<ItemId> {
    col.scene(scene)
        .unwrap()
        .items
        .iter()
        .map(|i| i.id)
        .collect()
}

#[test]
fn a_duplicate_is_an_independent_copy_placed_above_and_undone_exactly() {
    let mut f = fixture();
    let before = f.col.clone();
    let copies = [SourceCopy::new(f.picture, "Chrome 2", false)];
    let (made, undo) = f
        .col
        .duplicate_source(f.picture_item, &copies, &keys)
        .unwrap();
    let copy = made.copy_of(f.picture).unwrap();
    assert_ne!(copy, f.picture);
    let (c, o) = (
        f.col.source(copy).unwrap(),
        f.col.source(f.picture).unwrap(),
    );
    assert_eq!(c.name, "Chrome 2");
    assert_eq!(
        (c.type_id.clone(), c.settings.clone()),
        (o.type_id.clone(), o.settings.clone())
    );
    let (_, new_item) = made.items[0];
    let items = order(&f.col, f.scene);
    let at = items.iter().position(|i| *i == f.picture_item).unwrap();
    assert_eq!(items[at + 1], new_item, "directly above the original");
    let placed = f.col.item(new_item).unwrap().1;
    let orig = f.col.item(f.picture_item).unwrap().1;
    assert_eq!((placed.layout, placed.locked), (orig.layout, orig.locked));
    // Changing the copy leaves the original alone.
    f.col.rename_source(copy, "Other").unwrap();
    assert_eq!(f.col.source(f.picture).unwrap().name, "Chrome");
    let mut g = fixture();
    let (_, undo2) = g
        .col
        .duplicate_source(g.picture_item, &copies, &keys)
        .unwrap();
    g.col.undo(undo2).unwrap();
    assert_eq!(g.col, before);
    drop(undo);
}

#[test]
fn a_picture_takes_its_companion_muted_with_links_remapped_inside_the_set() {
    let mut f = fixture();
    let copies = [
        SourceCopy::new(f.picture, "Chrome 2", true),
        SourceCopy::new(f.sound, "Chrome sound 2", true).with_muted(true),
    ];
    let (made, _) = f
        .col
        .duplicate_source(f.picture_item, &copies, &keys)
        .unwrap();
    let (pic, snd) = (
        made.copy_of(f.picture).unwrap(),
        made.copy_of(f.sound).unwrap(),
    );
    let s = f.col.source(snd).unwrap();
    assert_eq!(s.companion_of(), Some(pic));
    assert!(s.audio.muted && s.standby());
    assert!(f.col.source(pic).unwrap().standby());
    // The microphone was not copied, so the ducker still listens to it.
    assert_eq!(
        s.filters[0].settings.get("key_source"),
        Some(&SettingValue::Int(i64::from(f.mic.get())))
    );
    assert_ne!(
        s.filters[0].id,
        f.col.source(f.sound).unwrap().filters[0].id
    );
    // The companion copy sits directly above the original companion.
    let items = order(&f.col, f.scene);
    let at = items.iter().position(|i| *i == f.sound_item).unwrap();
    assert_eq!(items[at + 1], made.items[1].1);
    // Deleting the copy takes its companion copy, not the original's.
    f.col.delete_source(pic).unwrap();
    assert!(f.col.source(snd).is_none());
    assert!(f.col.source(f.sound).is_some());
}

#[test]
fn a_companion_copied_alone_is_an_independent_sound() {
    let mut f = fixture();
    let copies = [SourceCopy::new(f.sound, "Chrome sound 2", false)];
    let (made, _) = f
        .col
        .duplicate_source(f.sound_item, &copies, &keys)
        .unwrap();
    let s = f.col.source(made.copy_of(f.sound).unwrap()).unwrap();
    assert_eq!(s.companion_of(), None);
    assert!(!s.hints.contains_key(STANDBY_HINT));
}

#[test]
fn own_levels_give_the_copy_the_original_entry() {
    let mut f = fixture();
    let mut levels = BTreeMap::new();
    levels.insert(f.mic, SceneLevel::new(Some(0.5), Some(false)));
    f.col
        .set_scene_own_levels(f.scene, SceneAudio::new(levels))
        .unwrap();
    let mic_item = order(&f.col, f.scene)[2];
    let copies = [SourceCopy::new(f.mic, "Mic 2", false)];
    let (made, undo) = f.col.duplicate_source(mic_item, &copies, &keys).unwrap();
    let copy = made.copy_of(f.mic).unwrap();
    let audio = f.col.scene(f.scene).unwrap().audio.clone().unwrap();
    assert_eq!(audio.levels.get(&copy), audio.levels.get(&f.mic));
    f.col.undo(undo).unwrap();
    assert!(
        !f.col
            .scene(f.scene)
            .unwrap()
            .audio
            .clone()
            .unwrap()
            .levels
            .contains_key(&copy)
    );
}

#[test]
fn a_wrong_first_copy_or_a_foreign_companion_is_refused_and_changes_nothing() {
    let mut f = fixture();
    let before = f.col.clone();
    let wrong = [SourceCopy::new(f.mic, "Mic 2", false)];
    assert!(
        f.col
            .duplicate_source(f.picture_item, &wrong, &keys)
            .is_err()
    );
    let foreign = [
        SourceCopy::new(f.picture, "Chrome 2", false),
        SourceCopy::new(f.mic, "Mic 2", false),
    ];
    assert!(
        f.col
            .duplicate_source(f.picture_item, &foreign, &keys)
            .is_err()
    );
    let taken = [SourceCopy::new(f.picture, "Mic", false)];
    assert!(
        f.col
            .duplicate_source(f.picture_item, &taken, &keys)
            .is_err()
    );
    assert_eq!(f.col, before);
}

#[test]
fn a_scene_with_new_sources_points_at_copies_rekeys_levels_and_remaps_links() {
    let mut f = fixture();
    let mut levels = BTreeMap::new();
    levels.insert(f.mic, SceneLevel::new(Some(0.25), None));
    f.col
        .set_scene_own_levels(f.scene, SceneAudio::new(levels))
        .unwrap();
    let before = f.col.clone();
    let copies = [
        SourceCopy::new(f.picture, "Chrome 2", false),
        SourceCopy::new(f.sound, "Chrome sound 2", false),
        SourceCopy::new(f.mic, "Mic 2", false),
    ];
    let (scene, made, undo) = f
        .col
        .duplicate_scene_with_copies(f.scene, "Scene 2", &copies, &keys)
        .unwrap();
    let (pic, snd, mic) = (
        made.copy_of(f.picture).unwrap(),
        made.copy_of(f.sound).unwrap(),
        made.copy_of(f.mic).unwrap(),
    );
    let content: Vec<_> = f
        .col
        .scene(scene)
        .unwrap()
        .items
        .iter()
        .map(|i| i.content)
        .collect();
    assert_eq!(
        content,
        vec![
            ItemContent::Source(pic),
            ItemContent::Source(snd),
            ItemContent::Source(mic)
        ]
    );
    let audio = f.col.scene(scene).unwrap().audio.clone().unwrap();
    assert_eq!(audio.levels.keys().copied().collect::<Vec<_>>(), vec![mic]);
    let s = f.col.source(snd).unwrap();
    assert_eq!(s.companion_of(), Some(pic));
    assert_eq!(
        s.filters[0].settings.get("key_source"),
        Some(&SettingValue::Int(i64::from(mic.get())))
    );
    // The original scene is untouched.
    assert_eq!(f.col.scene(f.scene), before.scene(f.scene));
    f.col.undo(undo).unwrap();
    assert_eq!(f.col, before);
}
