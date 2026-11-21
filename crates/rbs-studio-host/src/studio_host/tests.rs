#[test]
fn only_a_scene_and_its_held_copy_show_the_same_picture() {
    let id = SceneId::from_raw;
    let (x, y, copy, next) = (id(3), id(4), id(HELD_IDS), id(HELD_IDS_NEXT));
    // Studio mode entered (the hold starts) and left (the hold ends).
    assert!(same_picture(Some((x, x)), Some((copy, x))));
    assert!(same_picture(Some((copy, x)), Some((x, x))));
    // A transition of the scene to itself, another scene, or nothing new.
    assert!(!same_picture(Some((copy, x)), Some((next, x))));
    assert!(!same_picture(Some((copy, x)), Some((y, y))));
    assert!(!same_picture(Some((x, x)), Some((x, x))));
    assert!(!same_picture(None, Some((x, x))));
}

use super::*;

#[allow(clippy::unwrap_used)] // Tests fail by panicking.
#[test]
fn browser_input_from_the_api_is_checked_and_becomes_wire_events() {
    use rbs_api::request::{BrowserInputEvent as E, BrowserModifiers, BrowserMouseButton};
    use rbs_browser_ipc::input::{self as wire, InputEvent, Key, NamedKey};
    let shift = BrowserModifiers {
        shift: true,
        ..BrowserModifiers::default()
    };
    let click = |clicks| E::Button {
        x: 3,
        y: 4,
        button: BrowserMouseButton::Right,
        down: true,
        clicks,
        modifiers: shift,
    };
    assert_eq!(
        browser_input_event(&click(2)).unwrap(),
        InputEvent::Button {
            x: 3,
            y: 4,
            button: wire::Button::Right,
            down: true,
            clicks: 2,
            modifiers: wire::SHIFT,
        }
    );
    assert!(browser_input_event(&click(0)).is_err());
    assert!(browser_input_event(&click(4)).is_err());
    let key = |name: &str| E::Key {
        key: name.to_owned(),
        down: true,
        modifiers: BrowserModifiers::default(),
    };
    assert_eq!(
        browser_input_event(&key("Enter")).unwrap(),
        InputEvent::Key {
            key: Key::Named(NamedKey::Enter),
            down: true,
            modifiers: 0,
        }
    );
    assert!(browser_input_event(&key("NoSuchKey")).is_err());
    let text = |t: String| E::Text { text: t };
    assert_eq!(
        browser_input_event(&text("hi".into())).unwrap(),
        InputEvent::Text("hi".into())
    );
    assert!(browser_input_event(&text("x".repeat(257))).is_err());
}

/// While an output runs or a switch waits, the round keeps the short sleep. Idle
/// does not use that cap: with no deadline the control thread blocks on the next
/// message (F-08).
#[test]
fn the_control_thread_sleeps_longer_while_nothing_runs() {
    let mut outs = [CanvasOutputs::default(), CanvasOutputs::default()];
    assert_eq!(round_cap(&outs, false), IDLE_WAIT);
    assert_eq!(round_cap(&outs, true), MAX_WAIT);
    outs[1].ending = true;
    assert_eq!(round_cap(&outs, false), MAX_WAIT);
}

#[allow(clippy::unwrap_used)] // Tests fail by panicking.
#[test]
fn a_reopened_window_is_found_by_application_and_title_only_when_unique() {
    let window = |id: &str, name: &str, app: Option<&str>| Target {
        name: name.to_owned(),
        adapter: "a".to_owned(),
        target: id.to_owned(),
        is_default: false,
        size: None,
        app: app.map(str::to_owned),
        recent: None,
        picker: false,
        mic: None,
        sound: false,
    };
    let find = |listed: Vec<Target>, app: &str| match_by_hints(listed, "a", "old", app, "zsh");
    // One match, the application in another case.
    let one = vec![
        window("new", "zsh", Some("TERMINAL")),
        window("x", "vim", Some("Terminal")),
    ];
    assert_eq!(find(one, "Terminal").unwrap().target, "new");
    // Two windows alike: nothing is guessed.
    let two = vec![
        window("n1", "zsh", Some("Terminal")),
        window("n2", "zsh", Some("Terminal")),
    ];
    assert!(find(two, "Terminal").is_none());
    // Another application, no application known, a different title.
    assert!(find(vec![window("n", "zsh", Some("iTerm"))], "Terminal").is_none());
    assert!(find(vec![window("n", "zsh", None)], "Terminal").is_none());
    assert!(find(vec![window("n", "zsh2", Some("Terminal"))], "Terminal").is_none());
    // The stored window is still there: it is used as it is.
    let still = vec![
        window("old", "zsh", Some("Terminal")),
        window("new", "zsh", Some("Terminal")),
    ];
    assert!(find(still, "Terminal").is_none());
}

#[test]
fn hints_name_the_target_and_its_application() {
    let target = |name: &str, app: Option<&str>| Target {
        name: name.to_owned(),
        adapter: String::new(),
        target: String::new(),
        is_default: false,
        size: None,
        app: app.map(str::to_owned),
        recent: None,
        picker: false,
        mic: None,
        sound: false,
    };
    let h = hints_of(&target("zsh - Terminal", Some("Terminal")));
    assert_eq!(
        h.get(HINT_TITLE).map(String::as_str),
        Some("zsh - Terminal")
    );
    assert_eq!(h.get(HINT_APP).map(String::as_str), Some("Terminal"));
    // No application known: the title alone, so Reload by hints cannot match.
    assert_eq!(hints_of(&target("Display 1", None)).len(), 1);
    assert!(hints_of(&target("", Some(""))).is_empty());
}

/// A collection with a program scene holding one audio item, in studio mode with
/// the program scene in preview too.
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
fn held_collection() -> (SceneCollection, CanvasId, SceneId, SourceId) {
    let d = HostConfig::desktop();
    let mut col = SceneCollection::new();
    let (canvas, scene) = shown_scene(&mut col, d.size, d.fps).unwrap();
    let (src, _) = col
        .add_source("Mic", SourceKind::AudioInput.type_id(), Settings::new())
        .unwrap();
    col.add_item(
        scene,
        ItemContent::Source(src),
        Layout::new([0.0, 0.0], [10.0, 10.0]),
        None,
    )
    .unwrap();
    col.set_preview(canvas, Some(scene)).unwrap();
    (col, canvas, scene, src)
}

#[allow(clippy::unwrap_used)] // Tests fail by panicking.
#[test]
fn a_held_program_runs_with_the_collections_transition_and_glide() {
    // Studio mode with the program held (ADR-0063): the run collection must keep
    // the collection's transition, or every change runs as the default Move.
    let (mut col, canvas, scene, _) = held_collection();
    let mut t = rbs_scene::CollectionTransition {
        kind: rbs_scene::TransitionKind::Swipe,
        ..rbs_scene::CollectionTransition::default()
    };
    t.swipe.duration_ms = 700;
    col.set_transition(t).unwrap();
    let glide = rbs_scene::CollectionGlide {
        duration_ms: 900,
        ..col.glide()
    };
    col.set_glide(glide).unwrap();
    col.set_preview(canvas, Some(scene)).unwrap();
    let held = held_copy(&col, canvas, None).unwrap();
    let run = run_copy(&col, canvas, &held).unwrap();
    assert_eq!(run.transition(), t);
    assert_eq!(run.glide(), glide);
}

#[test]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
fn a_held_program_keeps_the_scene_as_it_was_and_is_never_listed() {
    let (mut col, canvas, scene, _) = held_collection();
    // Outside studio mode, or with another scene in preview: nothing is held.
    col.set_preview(canvas, None).unwrap();
    assert!(held_copy(&col, canvas, None).is_none());
    let (other, _) = col.add_scene("Other", SceneScope::Shared).unwrap();
    col.set_preview(canvas, Some(other)).unwrap();
    assert!(held_copy(&col, canvas, None).is_none());

    // The program scene in preview too: the program shows a copy of it.
    col.set_preview(canvas, Some(scene)).unwrap();
    let held = held_copy(&col, canvas, None).unwrap();
    assert_eq!(held.from, scene);
    assert_eq!(held.scene.items.len(), 1);
    // An edit made after the hold reaches the scene, not the copy on air.
    let item = col.scene(scene).unwrap().items[0].id;
    assert_eq!(
        held.scene.items[0].id.get(),
        HELD_IDS + item.get(),
        "an item's copy keeps its id, offset"
    );
    col.remove_item(item).unwrap();
    let held = next_held(Some(held), &col, canvas, None).unwrap();
    let run = run_copy(&col, canvas, &held).unwrap();
    let program = run.canvas(canvas).unwrap().program.unwrap();
    assert_eq!(program, held.scene.id);
    assert_eq!(run.scene(program).unwrap().items.len(), 1, "as it was");
    assert_eq!(
        run.scene(scene).unwrap().items.len(),
        0,
        "the edit, in preview"
    );
    assert_eq!(run.canvas(canvas).unwrap().preview, Some(scene));
    // The user's collection never holds the copy.
    assert!(col.scene(held.scene.id).is_none());
    assert_eq!(col.canvas(canvas).unwrap().program, Some(scene));
    assert_eq!(col.scenes().count(), 2);
    // The same scene id for every hold, so the program keeps its nodes.
    assert_eq!(
        held_copy(&col, canvas, None).unwrap().scene.id,
        held.scene.id
    );
    // Leaving studio mode lets the copy go.
    col.set_preview(canvas, None).unwrap();
    assert!(next_held(Some(held), &col, canvas, None).is_none());
}

#[test]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
fn previewing_another_scene_keeps_the_program_held() {
    // As after a transition that keeps the preview scene (ADR-0098): the program
    // scene is in preview and held, and its items are edited.
    let (mut col, canvas, scene, _) = held_collection();
    let held = held_copy(&col, canvas, None).unwrap();
    let item = col.scene(scene).unwrap().items[0].id;
    col.remove_item(item).unwrap();
    // Another scene goes to preview: the edit still waits for a transition.
    let (other, _) = col.add_scene("Other", SceneScope::Shared).unwrap();
    col.set_preview(canvas, Some(other)).unwrap();
    let held = next_held(Some(held), &col, canvas, None).unwrap();
    assert_eq!(held.from, scene);
    let run = run_copy(&col, canvas, &held).unwrap();
    let program = run.canvas(canvas).unwrap().program.unwrap();
    assert_eq!(run.scene(program).unwrap().items.len(), 1, "as it was");
    assert_eq!(run.canvas(canvas).unwrap().preview, Some(other));
    // A copy that must be taken again (it no longer fits) is of the held scene, also
    // with another scene in preview.
    let again = copy_of(&col, held.from, None).unwrap();
    assert_eq!(again.from, scene);
    assert!(run_copy(&col, canvas, &again).is_some());
    // Without a hold, another scene in preview starts none.
    assert!(next_held(None, &col, canvas, None).is_none());
    // The other scene on air (a transition): the copy goes; it is in preview too,
    // so the hold starts again with it.
    col.set_program(canvas, Some(other)).unwrap();
    let after = Some(held.scene.id);
    let next = next_held(Some(held), &col, canvas, after).unwrap();
    assert_eq!(next.from, other);
    // The copy after a copy has the other id, so the studio sees a new program
    // and runs the transition; the one after that has the first id again.
    assert_ne!(Some(next.scene.id), after);
    let third = copy_of(&col, other, Some(next.scene.id)).unwrap();
    assert_eq!(Some(third.scene.id), after);
}

#[test]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
fn a_held_copy_never_outlives_its_scene_or_its_sources() {
    let (mut col, canvas, scene, src) = held_collection();
    let held = held_copy(&col, canvas, None).unwrap();
    // A source deleted while held leaves the copy, which still fits.
    let item = col.scene(scene).unwrap().items[0].id;
    col.remove_item(item).unwrap();
    col.delete_source(src).unwrap();
    let kept = next_held(Some(held.clone()), &col, canvas, None).unwrap();
    assert!(kept.scene.items.is_empty());
    assert!(run_copy(&col, canvas, &kept).is_some());
    // Another scene on air in studio mode with itself in preview (as after an
    // undo removed the held scene): a new copy of that scene.
    let (other, _) = col.add_scene("Other", SceneScope::Shared).unwrap();
    col.set_program(canvas, Some(other)).unwrap();
    col.set_preview(canvas, Some(other)).unwrap();
    let next = next_held(Some(held), &col, canvas, None).unwrap();
    assert_eq!(next.from, other);
}

#[test]
fn previews_keep_the_canvas_aspect() {
    let d = preview_desc(Size::new(3440, 1440), FrameRate::FPS_30);
    assert_eq!((d.width, d.height), (1280, 534));
}

#[test]
fn preview_ring_covers_the_layer_hold_at_30_and_60_fps() {
    let surfaces = |fps| preview_desc(Size::new(1920, 1080), fps).surfaces;
    let n30 = surfaces(FrameRate::FPS_30);
    let n60 = surfaces(FrameRate::FPS_60);
    // A ring of 3 is shown + written + in flight, with nothing held after a
    // replace. The layer's default hold needs more than that at both rates
    // (geometry::ring_needed; ADR-0034, ADR-0036), so 3 drops frames.
    assert!(n30 > 3, "30 fps needs {n30}");
    assert!(n60 > 3, "60 fps needs {n60}");
    assert!(n30 <= MAX_PREVIEW_SURFACES && n60 <= MAX_PREVIEW_SURFACES);
    assert_eq!(n30, preview_surfaces(FrameRate::FPS_30));
    assert_eq!(n60, preview_surfaces(FrameRate::FPS_60));
    // macOS: 34 ms → 5 at 30 fps, 6 at 60 fps. Windows: one frame + 4 ms → 5 and 5.
    // Linux uses the macOS hold; it does not publish a preview.
    #[cfg(not(windows))]
    {
        assert_eq!((n30, n60), (5, 6));
    }
    #[cfg(windows)]
    {
        assert_eq!((n30, n60), (5, 5));
    }
    // Far above the cap: 2000 fps still stays within MAX_PREVIEW_SURFACES.
    let fast = match FrameRate::new(2_000, 1) {
        Some(rate) => rate,
        None => FrameRate::FPS_60,
    };
    assert_eq!(surfaces(fast), MAX_PREVIEW_SURFACES);
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn host_preview_desc_follows_the_canvas_rate() {
    let at = |fps| {
        HostConfig::canvas(Size::new(1920, 1080), fps)
            .preview
            .map(|p| p.surfaces)
    };
    assert_eq!(
        at(FrameRate::FPS_30),
        Some(preview_surfaces(FrameRate::FPS_30))
    );
    assert_eq!(
        at(FrameRate::FPS_60),
        Some(preview_surfaces(FrameRate::FPS_60))
    );
    let desktop = HostConfig::desktop().preview.map(|p| p.surfaces);
    assert_eq!(desktop, Some(preview_surfaces(FrameRate::FPS_30)));
}
