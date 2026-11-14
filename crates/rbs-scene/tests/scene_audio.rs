//! Scene levels (ADR-0091): a scene in Own levels keeps its own volume and mute per
//! source, applied while it is the program scene.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::BTreeMap;

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use rbs_graph::node::NodeTypeId;
use rbs_scene::model::{CanvasId, ItemContent, Layout, SceneId, SceneScope, Settings, SourceId};
use rbs_scene::{SceneAudio, SceneCollection, SceneError, SceneLevel};

struct Show {
    col: SceneCollection,
    canvas: CanvasId,
    talk: SceneId,
    brb: SceneId,
    mic: SourceId,
    music: SourceId,
}

fn show() -> Show {
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_30)
        .unwrap();
    let ty = |n: &str| NodeTypeId::new(format!("org.rbs.source.{n}"));
    let (mic, _) = col.add_source("Mic", ty("mic"), Settings::new()).unwrap();
    let (music, _) = col
        .add_source("Music", ty("music"), Settings::new())
        .unwrap();
    let (talk, _) = col.add_scene("Talking", SceneScope::Shared).unwrap();
    let (brb, _) = col.add_scene("Be right back", SceneScope::Shared).unwrap();
    let box_ = Layout::new([0.0, 0.0], [10.0, 10.0]);
    for scene in [talk, brb] {
        for s in [mic, music] {
            col.add_item(scene, ItemContent::Source(s), box_, None)
                .unwrap();
        }
    }
    col.set_source_volume(music, 0.5).unwrap();
    col.set_program(canvas, Some(talk)).unwrap();
    Show {
        col,
        canvas,
        talk,
        brb,
        mic,
        music,
    }
}

fn heard(col: &SceneCollection, canvas: CanvasId, s: SourceId) -> (f32, bool) {
    let a = col.heard_audio(canvas, s).unwrap();
    (a.volume, a.muted)
}

#[test]
fn shared_levels_hear_the_sources_own_values() {
    let s = show();
    assert!(!s.col.scene(s.talk).unwrap().own_levels());
    assert_eq!(heard(&s.col, s.canvas, s.music), (0.5, false));
    assert!(s.col.program_levels(s.canvas).is_none());
}

#[test]
fn own_levels_copy_what_is_heard_so_nothing_jumps() {
    let mut s = show();
    let levels = s.col.heard_levels(s.canvas, s.brb);
    assert_eq!(
        levels.level(s.music),
        Some(&SceneLevel::new(Some(0.5), Some(false)))
    );
    s.col.set_scene_own_levels(s.brb, levels).unwrap();
    s.col.set_program(s.canvas, Some(s.brb)).unwrap();
    assert_eq!(heard(&s.col, s.canvas, s.music), (0.5, false));
}

#[test]
fn the_program_scenes_entries_win_and_a_switch_brings_them_back() {
    let mut s = show();
    s.col
        .set_scene_own_levels(s.brb, SceneAudio::default())
        .unwrap();
    s.col
        .set_scene_level(s.brb, s.music, Some(SceneLevel::new(Some(1.0), None)))
        .unwrap();
    s.col
        .set_scene_level(s.brb, s.mic, Some(SceneLevel::new(None, Some(true))))
        .unwrap();
    // Not on air: the sources' own values.
    assert_eq!(heard(&s.col, s.canvas, s.music), (0.5, false));
    assert_eq!(heard(&s.col, s.canvas, s.mic), (1.0, false));
    s.col.set_program(s.canvas, Some(s.brb)).unwrap();
    assert_eq!(heard(&s.col, s.canvas, s.music), (1.0, false));
    // An absent field keeps the source's own value.
    s.col.set_source_volume(s.mic, 0.25).unwrap();
    assert_eq!(heard(&s.col, s.canvas, s.mic), (0.25, true));
    // A source's own change does not reach a field the scene sets.
    s.col.set_source_volume(s.music, 0.1).unwrap();
    assert_eq!(heard(&s.col, s.canvas, s.music), (1.0, false));
    s.col.set_program(s.canvas, Some(s.talk)).unwrap();
    assert_eq!(heard(&s.col, s.canvas, s.music), (0.1, false));
}

#[test]
fn level_edits_need_own_levels_and_valid_values() {
    let mut s = show();
    let before = s.col.clone();
    assert_eq!(
        s.col
            .set_scene_level(s.talk, s.mic, Some(SceneLevel::new(Some(0.5), None))),
        Err(SceneError::SharedLevels(s.talk))
    );
    s.col
        .set_scene_own_levels(s.talk, SceneAudio::default())
        .unwrap();
    for bad in [1.5, -0.1, f32::NAN] {
        assert_eq!(
            s.col
                .set_scene_level(s.talk, s.mic, Some(SceneLevel::new(Some(bad), None))),
            Err(SceneError::InvalidVolume)
        );
    }
    assert_eq!(
        s.col
            .set_scene_level(s.talk, SourceId::from_raw(99), Some(SceneLevel::default())),
        Err(SceneError::UnknownSource(SourceId::from_raw(99)))
    );
    s.col.set_scene_shared_levels(s.talk).unwrap();
    assert_eq!(s.col, before);
}

#[test]
fn live_level_edits_never_make_the_mode_records_conflict() {
    let mut s = show();
    let own = s
        .col
        .set_scene_own_levels(s.talk, s.col.heard_levels(s.canvas, s.talk))
        .unwrap();
    // A live edit after the mode change: its record is dropped, as the host does.
    let _ = s
        .col
        .set_scene_level(s.talk, s.music, Some(SceneLevel::new(Some(0.9), None)))
        .unwrap();
    // Undo of Own levels goes back to Shared and its redo restores the live value.
    let redo = s.col.undo(own).unwrap();
    assert!(!s.col.scene(s.talk).unwrap().own_levels());
    s.col.undo(redo).unwrap();
    assert_eq!(heard(&s.col, s.canvas, s.music), (0.9, false));
    // Back to Shared and undone: the entries come back as they were.
    let shared = s.col.set_scene_shared_levels(s.talk).unwrap();
    assert_eq!(heard(&s.col, s.canvas, s.music), (0.5, false));
    s.col.undo(shared).unwrap();
    assert_eq!(heard(&s.col, s.canvas, s.music), (0.9, false));
}

#[test]
fn deleting_a_source_takes_its_entries_and_undo_puts_them_back() {
    let mut s = show();
    let mut levels = BTreeMap::new();
    levels.insert(s.music, SceneLevel::new(Some(0.2), Some(true)));
    s.col
        .set_scene_own_levels(s.brb, SceneAudio::new(levels.clone()))
        .unwrap();
    s.col
        .set_scene_own_levels(s.talk, SceneAudio::new(levels))
        .unwrap();
    let delete = s.col.delete_source(s.music).unwrap();
    assert!(
        s.col
            .scene(s.brb)
            .unwrap()
            .audio
            .as_ref()
            .unwrap()
            .levels
            .is_empty()
    );
    // Own levels with no entries stay Own levels.
    assert!(s.col.scene(s.brb).unwrap().own_levels());
    // A scene that went back to Shared levels in between (its record no longer on
    // the stack) stays in Shared levels when the deletion is undone.
    let _ = s.col.set_scene_shared_levels(s.talk).unwrap();
    s.col.undo(delete).unwrap();
    assert_eq!(
        s.col
            .scene(s.brb)
            .unwrap()
            .audio
            .as_ref()
            .unwrap()
            .level(s.music),
        Some(&SceneLevel::new(Some(0.2), Some(true)))
    );
    assert!(!s.col.scene(s.talk).unwrap().own_levels());
}

#[test]
fn reset_source_removes_one_entry_as_one_step() {
    let mut s = show();
    s.col
        .set_scene_own_levels(s.talk, s.col.heard_levels(s.canvas, s.talk))
        .unwrap();
    let undo = s.col.set_scene_level(s.talk, s.mic, None).unwrap();
    let audio = s.col.scene(s.talk).unwrap().audio.clone().unwrap();
    assert!(audio.level(s.mic).is_none());
    assert!(audio.level(s.music).is_some());
    s.col.undo(undo).unwrap();
    assert!(
        s.col
            .scene(s.talk)
            .unwrap()
            .audio
            .as_ref()
            .unwrap()
            .level(s.mic)
            .is_some()
    );
}

#[test]
fn a_global_scene_has_no_levels_and_globals_are_copied() {
    let mut s = show();
    let (globals, _) = s
        .col
        .add_scene("Globals", SceneScope::Canvas(s.canvas))
        .unwrap();
    let ty = NodeTypeId::new("org.rbs.source.alert".to_owned());
    let (alert, _) = s.col.add_source("Alert", ty, Settings::new()).unwrap();
    s.col
        .add_item(
            globals,
            ItemContent::Source(alert),
            Layout::new([0.0, 0.0], [5.0, 5.0]),
            None,
        )
        .unwrap();
    s.col.set_globals(s.canvas, Some(globals)).unwrap();
    assert_eq!(
        s.col.set_scene_own_levels(globals, SceneAudio::default()),
        Err(SceneError::GlobalScene(globals))
    );
    let levels = s.col.heard_levels(s.canvas, s.talk);
    assert!(levels.level(alert).is_some(), "global sources are copied");
}

#[test]
fn duplicating_a_scene_copies_its_levels_and_nested_sources_are_covered() {
    let mut s = show();
    s.col
        .set_scene_own_levels(s.talk, s.col.heard_levels(s.canvas, s.talk))
        .unwrap();
    let (copy, _) = s.col.duplicate_scene(s.talk, "Talking 2").unwrap();
    assert_eq!(
        s.col.scene(copy).unwrap().audio,
        s.col.scene(s.talk).unwrap().audio
    );
    let (outer, _) = s.col.add_scene("Outer", SceneScope::Shared).unwrap();
    s.col
        .add_item(
            outer,
            ItemContent::Scene(s.brb),
            Layout::new([0.0, 0.0], [5.0, 5.0]),
            None,
        )
        .unwrap();
    let placed = s.col.placed_sources(s.canvas, outer);
    assert!(placed.contains(&s.mic) && placed.contains(&s.music));
}
