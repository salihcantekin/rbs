//! Scene tests, continued (the vertical canvas onwards); split from scenes.rs by size.

use super::*;

#[test]
pub(super) fn a_saved_vertical_canvas_runs_again_when_the_collection_opens() {
    use rbs_core::media::Size;
    use rbs_core::timing::FrameRate;
    use rbs_scene::model::SceneScope;
    use rbs_studio_host::studio_host::Command;
    // What `canvas.add_vertical` leaves in the document: a second canvas that follows
    // the main one and shows a shared scene.
    let mut doc = rbs_scene::SceneCollection::default();
    let fps = FrameRate::new(30, 1).unwrap();
    let (main, _) = doc.add_canvas("Main", Size::new(1280, 720), fps).unwrap();
    let (scene, _) = doc.add_scene("Scene", SceneScope::Shared).unwrap();
    doc.set_program(main, Some(scene)).unwrap();
    let (vertical, _) = doc
        .add_canvas("Vertical", Size::new(1080, 1920), fps)
        .unwrap();
    doc.set_program(vertical, Some(scene)).unwrap();
    doc.set_canvas_follow(vertical, Some(main), Default::default())
        .unwrap();
    let Some((host, updates, _api, _c)) = start_with(Some(doc)) else {
        return;
    };
    // It runs without a new Add: its outputs, panel and Remove are offered again.
    let state = next(&updates, |u| match u {
        Update::Vertical(v) => Some(v),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    })
    .expect("the saved vertical canvas runs");
    assert_eq!(state.canvas, vertical);
    assert_eq!(state.size, Size::new(1080, 1920));
    assert!(state.follow);
    host.send(Command::Api {
        call: Box::new(rbs_api::Command::CanvasRemoveVertical(Default::default())),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    });
    next(&updates, |u| match u {
        Update::Vertical(None) => Some(()),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    drop(host);
}

#[test]
pub(super) fn the_control_api_adds_and_removes_the_vertical_canvas() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    // Nothing to remove yet.
    assert_eq!(
        kind(&c.call("canvas.remove_vertical", json!({}))),
        "not_found"
    );
    // Sizes outside 16..=8192 are refused.
    let bad = c.call("canvas.add_vertical", json!({"width": 32, "height": 1920}));
    assert_eq!(kind(&bad), "invalid_params", "{bad}");
    let odd = c.call("canvas.add_vertical", json!({"width": 721, "height": 1280}));
    assert_eq!(kind(&odd), "invalid_params", "{odd}");
    ok(&c.call(
        "canvas.add_vertical",
        json!({"width": 720, "height": 1280, "layout": "stack"}),
    ));
    next(&updates, |u| match u {
        Update::Vertical(Some(v)) if v.follow => Some(()),
        _ => None,
    });
    // At most one.
    let again = c.call("canvas.add_vertical", json!({}));
    assert_eq!(kind(&again), "conflict", "{again}");
    ok(&c.call("canvas.remove_vertical", json!({})));
    next(&updates, |u| match u {
        Update::Vertical(None) => Some(()),
        _ => None,
    });
    drop(host);
}

#[test]
pub(super) fn style_crop_and_transform_on_the_vertical_canvas_leave_the_main_layout() {
    use rbs_scene::model::{ItemStyle, Shape, SourceId};
    use rbs_studio_host::studio_host::{Command, OutputCanvas};
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let raw = add(&mut c, "screen", "Screen");
    let id = SourceId::from_raw(raw as u32);
    let item = sources(&mut c)
        .iter()
        .find(|r| r["id"] == raw)
        .map(|r| r["item"].as_u64().unwrap() as u32)
        .unwrap();
    host.send(Command::Api {
        call: Box::new(rbs_api::Command::CanvasAddVertical(
            rbs_api::request::AddVerticalCanvas {
                width: None,
                height: None,
                follow: None,
                layout: None,
            },
        )),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    });
    let rows = next(&updates, |u| match u {
        Update::Sources(r) if r.iter().any(|r| r.vertical_layout.is_some()) => Some(r),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    let main = rows[0].layout.unwrap();
    // The vertical layout is the canvas's own from the first change on.
    host.send(crate::set_style(
        item,
        &ItemStyle::new(Shape::Circle),
        OutputCanvas::Vertical,
    ));
    let rows = next(&updates, |u| match u {
        Update::Sources(r)
            if r[0]
                .vertical_layout
                .is_some_and(|l| l.style.shape == Shape::Circle) =>
        {
            Some(r)
        }
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    assert_eq!(rows[0].layout, Some(main));
    host.send(Command::SetSourceCrop {
        seq: 9,
        gesture: 0,
        id,
        crop: [10, 0, 0, 0],
        keep_scale: false,
        canvas: OutputCanvas::Vertical,
    });
    let rows = next(&updates, |u| match u {
        Update::Sources(r) if r[0].vertical_layout.is_some_and(|l| l.crop.left == 10) => Some(r),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    assert_eq!(rows[0].layout, Some(main));
    assert_eq!(
        rows[0].vertical_layout.map(|l| l.style.shape),
        Some(Shape::Circle)
    );
    host.send(Command::Api {
        call: Box::new(rbs_api::Command::ItemFlip(rbs_api::request::FlipItem {
            item,
            axis: rbs_api::request::Axis::Horizontal,
            canvas: OutputCanvas::Vertical,
            gesture: 0,
        })),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    });
    let rows = next(&updates, |u| match u {
        Update::Sources(r)
            if r[0]
                .vertical_layout
                .is_some_and(|l| l.flip_state().horizontal) =>
        {
            Some(r)
        }
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    assert_eq!(rows[0].layout, Some(main));
    // In studio mode the vertical canvas is not edited: the style is refused.
    let program = scenes(&mut c)["program"].clone();
    ok(&c.call("canvas.set_preview", json!({"scene": program})));
    host.send(crate::set_style(
        item,
        &ItemStyle::new(Shape::Ellipse),
        OutputCanvas::Vertical,
    ));
    next(&updates, |u| match u {
        Update::Error(_) => Some(()),
        _ => None,
    });
    drop(host);
}

/// Properties' Scroll (ADR-0150 decision 12): the switch sets the scroll as one undo
/// step, Pause holds the run (the row says so) and Resume lets it go, live; an out of
/// range scroll and a live call without a scroll are refused in words.
#[test]
pub(super) fn properties_scroll_sets_the_layout_and_pauses_the_run() {
    use rbs_api::request::ScrollItem;
    use rbs_scene::model::{Scroll, ScrollDirection};
    use rbs_studio_host::studio_host::{ApiReply, Command, OutputCanvas};
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let raw = add(&mut c, "screen", "Screen");
    let item = sources(&mut c)
        .iter()
        .find(|r| r["id"] == raw)
        .map(|r| r["item"].as_u64().unwrap() as u32)
        .unwrap();
    // As the window's Pause and Resume buttons send them (ADR-0163).
    let live = |call: fn(ScrollItem) -> rbs_api::Command| Command::Api {
        call: Box::new(call(ScrollItem {
            item,
            canvas: OutputCanvas::Main,
        })),
        reply: ApiReply::Shown,
    };
    host.send(live(rbs_api::Command::ItemScrollPause));
    next(&updates, |u| match u {
        Update::Error(e) if e.contains("does not scroll") => Some(()),
        _ => None,
    });
    let ticker = Scroll::new(ScrollDirection::Left, 120.0);
    host.send(crate::set_scroll(item, Some(ticker), OutputCanvas::Main));
    next(&updates, |u| match u {
        Update::Sources(r) if r[0].layout.is_some_and(|l| l.scroll == Some(ticker)) => Some(()),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    host.send(live(rbs_api::Command::ItemScrollPause));
    next(&updates, |u| match u {
        Update::Sources(r) if r[0].scroll_paused == [true, false] => Some(()),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    host.send(live(rbs_api::Command::ItemScrollResume));
    next(&updates, |u| match u {
        Update::Sources(r) if r[0].scroll_paused == [false, false] => Some(()),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    host.send(crate::set_scroll(
        item,
        Some(Scroll::new(ScrollDirection::Left, 0.0)),
        OutputCanvas::Main,
    ));
    next(&updates, |u| match u {
        Update::Error(e) if e.contains("speed") => Some(()),
        _ => None,
    });
    ok(&c.call("edit.undo", json!({})));
    next(&updates, |u| match u {
        Update::Sources(r) if r[0].layout.is_some_and(|l| l.scroll.is_none()) => Some(()),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    drop(host);
}

/// Protocol 1.44: the vertical canvas's layouts and size through the control API.
#[test]
pub(super) fn the_control_api_arranges_and_resizes_the_vertical_canvas() {
    use rbs_core::media::Size;
    use rbs_studio_host::studio_host::Command;
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let screen = add(&mut c, "screen", "Screen");
    // Without a vertical canvas: not_found.
    assert_eq!(
        kind(&c.call(
            "canvas.auto_layout",
            json!({"canvas": "vertical", "mode": "fit"})
        )),
        "not_found"
    );
    assert_eq!(
        kind(&c.call(
            "canvas.set_vertical_size",
            json!({"width": 720, "height": 1280})
        )),
        "not_found"
    );
    host.send(Command::Api {
        call: Box::new(rbs_api::Command::CanvasAddVertical(
            rbs_api::request::AddVerticalCanvas {
                width: None,
                height: None,
                follow: None,
                layout: None,
            },
        )),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    });
    let rows = next(&updates, |u| match u {
        Update::Sources(r) if r.iter().any(|r| r.vertical_layout.is_some()) => Some(r),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    let main = rows[0].layout.unwrap();
    let item = sources(&mut c)
        .iter()
        .find(|r| r["id"] == screen)
        .map(|r| r["item"].as_u64().unwrap())
        .unwrap();
    // The main canvas cannot be arranged in 1.44; a bad size is invalid.
    assert_eq!(
        kind(&c.call(
            "canvas.auto_layout",
            json!({"canvas": "main", "mode": "fit"})
        )),
        "invalid_params"
    );
    assert_eq!(
        kind(&c.call(
            "canvas.set_vertical_size",
            json!({"width": 721, "height": 1280})
        )),
        "invalid_params"
    );
    // A flip on the vertical canvas leaves the main layout.
    ok(&c.call(
        "item.flip",
        json!({"item": item, "axis": "horizontal", "canvas": "vertical"}),
    ));
    let rows = next(&updates, |u| match u {
        Update::Sources(r)
            if r[0]
                .vertical_layout
                .is_some_and(|l| l.flip_state().horizontal) =>
        {
            Some(r)
        }
        _ => None,
    });
    assert_eq!(rows[0].layout, Some(main));
    // A box on the vertical canvas, then the same box again changes nothing.
    let put = json!({"source": screen, "position": [0.0, 0.0], "size": [540.0, 960.0], "canvas": "vertical"});
    assert_eq!(ok(&c.call("item.set_box", put.clone()))["ok"], true);
    assert_eq!(ok(&c.call("item.set_box", put))["ok"], true);
    // Auto layout answers how many layouts it wrote: the screen once its size is known.
    let done = ok(&c.call(
        "canvas.auto_layout",
        json!({"canvas": "vertical", "mode": "stack"}),
    ))
    .clone();
    assert!(done["items"].as_u64().unwrap() <= 1, "{done}");
    // Studio mode does not refuse auto layout; it does refuse an item edit.
    let program = scenes(&mut c)["program"].clone();
    ok(&c.call("canvas.set_preview", json!({"scene": program})));
    ok(&c.call(
        "canvas.auto_layout",
        json!({"canvas": "vertical", "mode": "fit"}),
    ));
    assert_eq!(
        kind(&c.call(
            "item.fit",
            json!({"item": item, "mode": "fit", "canvas": "vertical"})
        )),
        "conflict"
    );
    assert_eq!(
        kind(&c.call(
            "item.set_style",
            json!({"item": item, "style": {"shape": "circle"}, "canvas": "vertical"})
        )),
        "conflict"
    );
    ok(&c.call("canvas.set_preview", json!({"scene": null})));
    // A new size: the canvas runs at it, and the same size again changes nothing.
    ok(&c.call(
        "canvas.set_vertical_size",
        json!({"width": 720, "height": 1280}),
    ));
    next(&updates, |u| match u {
        Update::Vertical(Some(v)) if v.size == Size::new(720, 1280) => Some(()),
        Update::Error(e) => panic!("{e}"),
        _ => None,
    });
    ok(&c.call(
        "canvas.set_vertical_size",
        json!({"width": 720, "height": 1280}),
    ));
    drop(host);
}

/// The source row of `id` in `source.list` (the program scene).
pub(super) fn row(c: &mut Client, id: u64) -> Value {
    sources(c)
        .into_iter()
        .find(|r| r["id"] == id)
        .expect("a row for the source")
}

/// The window's fader and mute (ADR-0091 decision 5): while the program scene keeps
/// its own levels they edit that scene's level, else the source's own, as before.
#[test]
pub(super) fn the_window_fader_edits_the_scene_level_in_own_levels() {
    use rbs_scene::model::SourceId;
    use rbs_studio_host::studio_host::Command;
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let one = scenes(&mut c)["program"].as_u64().unwrap();
    let mic = add(&mut c, "audio_input", "Mic");
    let id = SourceId::from_raw(mic as u32);
    // What the Mixer is told it hears for the mic, after the next change to it: new
    // rows, or new audio for the rows it has (a fader step, F-10).
    let heard_after = |want: f32| {
        next(&updates, |u| match u {
            Update::Heard(rows) => rows
                .iter()
                .find(|r| r.id == id)
                .map(|r| r.heard.unwrap_or(r.audio))
                .filter(|a| (a.volume - want).abs() < 1e-6),
            Update::SourceAudio(patches) => patches
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.heard.unwrap_or(p.audio))
                .filter(|a| (a.volume - want).abs() < 1e-6),
            _ => None,
        })
    };

    // Shared levels: the fader is the source's own volume.
    host.send(Command::SetSourceVolume { id, volume: 0.5 });
    heard_after(0.5);
    assert_eq!(row(&mut c, mic)["volume"], 0.5);
    assert!(row(&mut c, mic).get("heard").is_none());

    // Own levels: the fader and mute edit the scene's entry; the source keeps 0.5.
    ok(&c.call("scene.audio.own", json!({"scene": one})));
    host.send(Command::SetSourceVolume { id, volume: 0.25 });
    let heard = heard_after(0.25);
    assert!(!heard.muted);
    host.send(Command::SetSourceMuted { id, muted: true });
    next(&updates, |u| match u {
        Update::Heard(rows) => rows
            .iter()
            .find(|r| r.id == id && r.heard.is_some_and(|h| h.muted))
            .map(|_| ()),
        _ => None,
    });
    let r = row(&mut c, mic);
    assert_eq!(r["volume"], 0.5, "the source's own volume is kept");
    assert_eq!(r["muted"], false, "the source's own mute is kept");
    assert_eq!(r["heard"], json!({"volume": 0.25, "muted": true}));
    let got = ok(&c.call("scene.audio.get", json!({"scene": one}))).clone();
    assert_eq!(
        got["levels"],
        json!([{"source": mic, "volume": 0.25, "muted": true}])
    );

    // The window's reset removes the entry: the source's own level is heard again.
    host.send(Command::ResetSceneLevel {
        scene: rbs_scene::model::SceneId::from_raw(one as u32),
        source: id,
    });
    heard_after(0.5);
    assert!(row(&mut c, mic).get("heard").is_none());

    // Back to Shared levels from the window: the fader is the source's own again.
    host.send(Command::SetSceneOwnLevels {
        scene: rbs_scene::model::SceneId::from_raw(one as u32),
        own: false,
    });
    next(&updates, |u| match u {
        Update::Scenes(list) => list
            .scenes
            .iter()
            .any(|s| s.id.get() as u64 == one && !s.own_audio)
            .then_some(()),
        _ => None,
    });
    host.send(Command::SetSourceVolume { id, volume: 0.75 });
    heard_after(0.75);
    assert_eq!(row(&mut c, mic)["volume"], 0.75);
    drop(host);
}

#[test]
pub(super) fn a_scene_keeps_its_own_levels_and_they_are_heard_while_it_is_on_air() {
    let Some((host, _updates, _api, mut c)) = start() else {
        return;
    };
    let one = scenes(&mut c)["program"].as_u64().unwrap();
    let mic = add(&mut c, "audio_input", "Mic");
    let brb = ok(&c.call("scene.add", json!({"name": "Be right back"})))["scene"]
        .as_u64()
        .unwrap();
    ok(&c.call("item.add", json!({"source": mic, "scene": brb})));

    // Shared levels by default: nothing new in the answers (ADR-0091).
    let got = ok(&c.call("scene.audio.get", json!({"scene": brb}))).clone();
    assert_eq!(got, json!({"scene": brb, "own": false, "levels": []}));
    assert!(scenes(&mut c)["scenes"][1].get("own_audio").is_none());
    assert!(row(&mut c, mic).get("heard").is_none());

    // A level for a scene in Shared levels turns it to Own levels, copying what is
    // heard, and says so.
    let set = c.call(
        "scene.audio.set",
        json!({"scene": brb, "source": mic, "muted": true}),
    );
    assert_eq!(ok(&set)["turned_own"], true);
    let got = ok(&c.call("scene.audio.get", json!({"scene": brb}))).clone();
    assert_eq!(got["own"], true);
    assert_eq!(
        got["levels"],
        json!([{"source": mic, "volume": 1.0, "muted": true}])
    );
    assert_eq!(scenes(&mut c)["scenes"][1]["own_audio"], true);
    // Not on air: the program scene hears the source's own level.
    assert!(row(&mut c, mic).get("heard").is_none());

    // On air, the row says what is heard; the source's own values stay.
    ok(&c.call("scene.switch", json!({"scene": brb})));
    let r = row(&mut c, mic);
    assert_eq!(r["muted"], false);
    assert_eq!(r["heard"], json!({"volume": 1.0, "muted": true}));
    ok(&c.call("source.set_volume", json!({"source": mic, "volume": 0.5})));
    let r = row(&mut c, mic);
    assert_eq!(r["volume"], 0.5);
    assert_eq!(
        r["heard"]["volume"], 1.0,
        "the scene's entry is still in force"
    );
    // A second set on a scene in Own levels is live, not an undo step.
    let set = c.call(
        "scene.audio.set",
        json!({"scene": brb, "source": mic, "volume": 0.25}),
    );
    assert_eq!(ok(&set)["turned_own"], false);
    assert_eq!(
        row(&mut c, mic)["heard"],
        json!({"volume": 0.25, "muted": true})
    );

    // Reset one entry: one undo step.
    ok(&c.call(
        "scene.audio.reset_source",
        json!({"scene": brb, "source": mic}),
    ));
    assert!(row(&mut c, mic).get("heard").is_none());
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(
        row(&mut c, mic)["heard"],
        json!({"volume": 0.25, "muted": true})
    );

    // Shared levels discard the entries; undo brings them back.
    ok(&c.call("scene.audio.shared", json!({"scene": brb})));
    assert!(row(&mut c, mic).get("heard").is_none());
    assert!(scenes(&mut c)["scenes"][1].get("own_audio").is_none());
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(row(&mut c, mic)["heard"]["muted"], true);

    // Own levels copy what the scene makes heard in Shared levels: the source's own
    // level, not the program scene's entry.
    ok(&c.call("scene.audio.own", json!({"scene": one})));
    let got = ok(&c.call("scene.audio.get", json!({"scene": one}))).clone();
    assert_eq!(
        got["levels"],
        json!([{"source": mic, "volume": 0.5, "muted": false}])
    );
    // Back to the first scene: its entry is heard, which is the source's own level.
    ok(&c.call("scene.switch", json!({"scene": one})));
    assert_eq!(
        row(&mut c, mic)["heard"],
        json!({"volume": 0.5, "muted": false})
    );

    // Refusals.
    for (params, want) in [
        (json!({"scene": brb, "source": mic}), "invalid_params"),
        (
            json!({"scene": brb, "source": mic, "volume": 2.0}),
            "invalid_params",
        ),
        (
            json!({"scene": 99, "source": mic, "muted": true}),
            "not_found",
        ),
        (
            json!({"scene": brb, "source": 99, "muted": true}),
            "not_found",
        ),
    ] {
        assert_eq!(
            kind(&c.call("scene.audio.set", params.clone())),
            want,
            "{params}"
        );
    }
    assert_eq!(
        kind(&c.call("scene.audio.get", json!({"scene": 99}))),
        "not_found"
    );
    drop(host);
}

#[test]
pub(super) fn the_collection_transition_is_set_and_undone_and_a_one_off_cut_is_reported() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    ok(&c.call(
        "events.subscribe",
        json!({"topics": ["scene.changed", "transition.state"]}),
    ));
    let one = scenes(&mut c)["program"].as_u64().unwrap();
    let two = ok(&c.call("scene.add", json!({"name": "Game"})))["scene"]
        .as_u64()
        .unwrap();
    // The default (Move) is left out of the snapshot.
    assert_eq!(scenes(&mut c).get("transition"), None);

    // Only the given parts change, as one undo step.
    ok(&c.call(
        "collection.set_transition",
        json!({"kind": "fade",
               "fade": {"duration_ms": 800, "easing": "linear", "via": "#0a0B0c"}}),
    ));
    let t = scenes(&mut c)["transition"].clone();
    assert_eq!(t["kind"], "fade", "{t}");
    assert_eq!(t["fade"]["duration_ms"], 800);
    assert_eq!(t["fade"]["via"], "#0a0b0c");
    assert_eq!(
        t["swipe"]["duration_ms"], 500,
        "untouched parts keep theirs"
    );
    ok(&c.call("collection.set_transition", json!({"kind": "cut"})));
    let t = scenes(&mut c)["transition"].clone();
    assert_eq!(
        (t["kind"].clone(), t["fade"]["duration_ms"].clone()),
        (json!("cut"), json!(800))
    );
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(scenes(&mut c)["transition"]["kind"], "fade");
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(
        scenes(&mut c).get("transition"),
        None,
        "back to the default"
    );

    // The kind outside studio mode (protocol 1.97, ADR-0153): its own field, one undo
    // step, left out while Move.
    ok(&c.call("collection.set_transition", json!({"direct_kind": "slide"})));
    let t = scenes(&mut c)["transition"].clone();
    assert_eq!(
        (t["kind"].clone(), t["direct_kind"].clone()),
        (json!("move"), json!("slide")),
        "{t}"
    );
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(scenes(&mut c).get("transition"), None);

    // Refusals change nothing.
    for bad in [
        json!({"fade": {"duration_ms": 300, "easing": "linear", "via": "black"}}),
        json!({"fade": {"duration_ms": 5001, "easing": "linear"}}),
    ] {
        assert_eq!(
            kind(&c.call("collection.set_transition", bad)),
            "invalid_params"
        );
    }
    assert_eq!(
        kind(&c.call(
            "scene.switch",
            json!({"scene": two, "kind": "fade", "duration_ms": 5001})
        )),
        "invalid_params"
    );
    assert_eq!(scenes(&mut c)["program"], one);

    // A one-off cut: reported at once, the collection's transition untouched.
    ok(&c.call("scene.switch", json!({"scene": two, "kind": "cut"})));
    let e = c.event("transition.state", |d| d["state"] == "ended");
    let d = &e["params"]["data"];
    assert_eq!(d["canvas"], "main", "{e}");
    assert_eq!(d["kind"], "cut");
    assert_eq!(d["duration_ms"], 0);
    assert_eq!(
        (d["from"].clone(), d["to"].clone()),
        (json!(one), json!(two))
    );
    assert_eq!(scenes(&mut c).get("transition"), None);

    // The same in studio mode through canvas.transition.
    ok(&c.call("canvas.set_preview", json!({"scene": one})));
    ok(&c.call("canvas.transition", json!({"kind": "cut"})));
    // A slow reader may miss `started` (the latest state wins), never `ended`.
    let e = c.event("transition.state", |d| {
        d["to"] == one && d["state"] == "ended"
    });
    assert_eq!(
        e["params"]["data"]["from"], two,
        "the held copy is named by its scene"
    );
    while let Ok(u) = updates.try_recv() {
        if let Update::Error(e) = u {
            panic!("{e}");
        }
    }
    drop(host);
}

#[test]
pub(super) fn a_scene_transition_is_set_listed_cleared_undone_and_refused_when_wrong() {
    let Some((_host, _updates, _api, mut c)) = start() else {
        return;
    };
    let one = scenes(&mut c)["program"].as_u64().unwrap();

    // Absent until set (protocol 1.86, ADR-0135 decision 10).
    assert!(scenes(&mut c)["scenes"][0].get("transition_in").is_none());
    ok(&c.call(
        "scene.set_transition",
        json!({"scene": one, "transition": {"kind": "swipe", "duration_ms": 400}}),
    ));
    assert_eq!(
        scenes(&mut c)["scenes"][0]["transition_in"],
        json!({"kind": "swipe", "duration_ms": 400})
    );
    // Only a duration keeps the collection's kind.
    ok(&c.call(
        "scene.set_transition",
        json!({"scene": one, "transition": {"duration_ms": 900}}),
    ));
    assert_eq!(
        scenes(&mut c)["scenes"][0]["transition_in"],
        json!({"duration_ms": 900})
    );
    // One undo step brings the previous override back.
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(
        scenes(&mut c)["scenes"][0]["transition_in"],
        json!({"kind": "swipe", "duration_ms": 400})
    );
    // null and an empty override both clear it.
    ok(&c.call(
        "scene.set_transition",
        json!({"scene": one, "transition": null}),
    ));
    assert!(scenes(&mut c)["scenes"][0].get("transition_in").is_none());
    ok(&c.call(
        "scene.set_transition",
        json!({"scene": one, "transition": {"kind": "fade"}}),
    ));
    ok(&c.call(
        "scene.set_transition",
        json!({"scene": one, "transition": {}}),
    ));
    assert!(scenes(&mut c)["scenes"][0].get("transition_in").is_none());

    assert_eq!(
        kind(&c.call(
            "scene.set_transition",
            json!({"scene": 99, "transition": {"kind": "cut"}})
        )),
        "not_found"
    );
    assert_eq!(
        kind(&c.call(
            "scene.set_transition",
            json!({"scene": one, "transition": {"duration_ms": 5001}})
        )),
        "invalid_params"
    );
    assert_eq!(
        kind(&c.call(
            "scene.set_transition",
            json!({"scene": one, "transition": {"kind": "spin"}})
        )),
        "invalid_params"
    );
}

#[test]
pub(super) fn a_t_bar_holds_then_puts_the_preview_on_air_and_refuses_when_wrong() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    ok(&c.call(
        "events.subscribe",
        json!({"topics": ["scene.changed", "transition.state"]}),
    ));
    let one = scenes(&mut c)["program"].as_u64().unwrap();
    add(&mut c, "screen", "Main screen");
    let two = ok(&c.call("scene.add", json!({"name": "Game"})))["scene"]
        .as_u64()
        .unwrap();
    ok(&c.call("collection.set_transition", json!({"kind": "fade"})));
    // Outside studio mode there is no T-bar (ADR-0135 decision 1).
    assert_eq!(
        kind(&c.call("canvas.transition_progress", json!({"t": 0.5}))),
        "conflict"
    );
    ok(&c.call("canvas.set_preview", json!({"scene": two})));
    for bad in [json!({"t": 1.5}), json!({"t": -0.1}), json!({})] {
        assert_eq!(
            kind(&c.call("canvas.transition_progress", bad.clone())),
            "invalid_params",
            "{bad}"
        );
    }
    assert_eq!(
        kind(&c.call(
            "canvas.transition_progress",
            json!({"canvas": "vertical", "t": 0.5})
        )),
        "conflict"
    );

    // Held, the program stays. This host draws no program picture (no output, a
    // test clock), so it waits at its ready gate and publishes nothing yet; the
    // studio's own test covers `started` and `holding`.
    ok(&c.call("canvas.transition_progress", json!({"t": 0.4})));
    ok(&c.call("canvas.transition_progress", json!({"t": 0.6})));
    assert_eq!(scenes(&mut c)["program"], one);

    // 1 puts the preview on air without a timed transition.
    ok(&c.call("canvas.transition_progress", json!({"t": 1.0})));
    let e = c.event("transition.state", |d| d["state"] == "ended");
    let d = &e["params"]["data"];
    assert_eq!(d["manual"], true, "{e}");
    assert_eq!(d["kind"], "fade");
    assert_eq!(
        (d["from"].clone(), d["to"].clone()),
        (json!(one), json!(two))
    );
    assert_eq!(scenes(&mut c)["program"], two);

    // Back to 0 cancels: the program stays as it is.
    ok(&c.call("canvas.set_preview", json!({"scene": one})));
    ok(&c.call("canvas.transition_progress", json!({"t": 0.5})));
    ok(&c.call("canvas.transition_progress", json!({"t": 0.0})));
    assert_eq!(scenes(&mut c)["program"], two);
    // A held one is cancelled by a transition, which then runs as today.
    ok(&c.call("canvas.transition_progress", json!({"t": 0.5})));
    ok(&c.call("canvas.transition", json!({"kind": "cut"})));
    let e = c.event("transition.state", |d| {
        d["state"] == "ended" && d["to"] == one
    });
    assert_eq!(e["params"]["data"].get("manual"), None, "{e}");
    assert_eq!(scenes(&mut c)["program"], one);
    while let Ok(u) = updates.try_recv() {
        if let Update::Error(e) = u {
            panic!("{e}");
        }
    }
    drop(host);
}

#[test]
pub(super) fn a_stinger_is_chosen_listed_found_and_refused_when_wrong() {
    // Protocol 1.89, ADR-0140 decisions 1, 3 and 9.
    use rbs_core::media::Size;
    use rbs_core::timing::FrameRate;
    use rbs_graph::node::NodeTypeId;
    use rbs_media_decode_test::{ScriptClip, ScriptFactory, ScriptFrame};
    use rbs_scene::model::{ItemContent, Layout, SceneScope, SettingValue, Settings};
    // A file on disk; the scripted adapter does not read it.
    let dir = std::env::temp_dir().join(format!("rbs-stinger-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("sting.mov");
    std::fs::write(&file, b"sting").unwrap();
    let media = |name: &str, doc: &mut rbs_scene::SceneCollection| {
        let mut settings = Settings::new();
        settings.insert(
            "path".to_owned(),
            SettingValue::Text(file.display().to_string()),
        );
        doc.add_source(name, NodeTypeId::new("org.rbs.source.media"), settings)
            .unwrap()
            .0
    };
    let mut doc = rbs_scene::SceneCollection::default();
    let fps = FrameRate::new(30, 1).unwrap();
    let (main, _) = doc.add_canvas("Main", Size::new(1280, 720), fps).unwrap();
    let (scene, _) = doc.add_scene("Scene", SceneScope::Shared).unwrap();
    doc.set_program(main, Some(scene)).unwrap();
    let sting = media("Sting", &mut doc).get();
    let placed = media("Clip", &mut doc);
    doc.add_item(
        scene,
        ItemContent::Source(placed),
        Layout::new([0.0, 0.0], [10.0, 10.0]),
        None,
    )
    .unwrap();
    // Clear for 4 pictures, then opaque: the cut is found at 400 ms.
    let step = Duration::from_millis(100);
    let mut clip = ScriptClip::silent_frames();
    clip.frames = (0..10)
        .map(|i| {
            let mut f = ScriptFrame::rgba(16, 9, step * i, step, 1);
            if i < 4 {
                f.bytes.chunks_exact_mut(4).for_each(|p| p[3] = 0);
            }
            f
        })
        .collect();
    clip.duration = Some(step * 10);
    let factory = Arc::new(ScriptFactory::new(clip).with_alpha(true));
    let Some((_host, updates, _api, mut c)) = start_with_media(Some(doc), Some(factory)) else {
        return;
    };
    let answer = c.call(
        "collection.set_transition",
        json!({"kind": "stinger", "stinger": {"source": sting}}),
    );
    assert_eq!(ok(&answer), &json!({}));
    let found = next(&updates, |u| match u {
        Update::Scenes(list) => list.stinger_cut_found_ms,
        _ => None,
    });
    assert_eq!(found, 400);
    let listed = scenes(&mut c)["transition"].clone();
    assert_eq!(listed["kind"], "stinger");
    assert_eq!(
        listed["stinger"],
        json!({"source": sting, "cut_found_ms": 400})
    );
    // The stinger is never placed, and a placed source cannot be the stinger.
    assert_eq!(
        kind(&c.call("item.add", json!({"source": sting}))),
        "conflict"
    );
    assert_eq!(
        kind(&c.call(
            "collection.set_transition",
            json!({"stinger": {"source": placed.get()}})
        )),
        "conflict"
    );
    assert_eq!(
        kind(&c.call(
            "collection.set_transition",
            json!({"stinger": {"source": 99}})
        )),
        "not_found"
    );
    // A cut point past the file's end, once known.
    assert_eq!(
        kind(&c.call(
            "collection.set_transition",
            json!({"stinger": {"source": sting, "cut_ms": 5000}})
        )),
        "invalid_params"
    );
    ok(&c.call(
        "collection.set_transition",
        json!({"stinger": {"source": sting, "cut_ms": 700}}),
    ));
    assert_eq!(scenes(&mut c)["transition"]["stinger"]["cut_ms"], 700);
    // Removing the source clears the stinger, and one undo brings both back.
    ok(&c.call("source.delete", json!({"source": sting})));
    let after = scenes(&mut c)["transition"].clone();
    assert_eq!(after["kind"], "stinger");
    assert_eq!(after["stinger"]["source"], Value::Null);
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(scenes(&mut c)["transition"]["stinger"]["source"], sting);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
pub(super) fn a_film_restart_when_shown_setting_is_written_to_its_activity_policy() {
    // ADR-0152 decision 10: the old key is the deprecated spelling of `on_return`.
    use rbs_core::media::Size;
    use rbs_core::timing::FrameRate;
    use rbs_graph::node::NodeTypeId;
    use rbs_scene::model::{SceneScope, SettingValue, Settings};
    let dir = std::env::temp_dir().join(format!("rbs-film-return-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("film.mov");
    std::fs::write(&file, b"film").unwrap();
    let mut settings = Settings::new();
    settings.insert(
        "path".to_owned(),
        SettingValue::Text(file.display().to_string()),
    );
    let mut doc = rbs_scene::SceneCollection::default();
    let fps = FrameRate::new(30, 1).unwrap();
    let (main, _) = doc.add_canvas("Main", Size::new(1280, 720), fps).unwrap();
    let (scene, _) = doc.add_scene("Scene", SceneScope::Shared).unwrap();
    doc.set_program(main, Some(scene)).unwrap();
    let film = doc
        .add_source("Film", NodeTypeId::new("org.rbs.source.media"), settings)
        .unwrap()
        .0
        .get();
    let Some((_host, _updates, _api, mut c)) = start_with_media(Some(doc), None) else {
        return;
    };
    let set = |c: &mut Client, value: Value| {
        c.call(
            "source.set_settings",
            json!({"source": film, "settings": {"restart_when_shown": value}}),
        )
    };
    let on_return = |c: &mut Client| {
        c.call("source.get_activity", json!({"source": film}))["result"]["on_return"].clone()
    };
    let shown_key = |c: &mut Client| {
        c.call("source.get_settings", json!({"source": film}))["result"]["settings"]
            ["restart_when_shown"]
            .clone()
    };
    assert_eq!(shown_key(&mut c), json!(true));
    ok(&set(&mut c, json!(false)));
    assert_eq!(on_return(&mut c), json!("resume"));
    assert_eq!(shown_key(&mut c), json!(false));
    ok(&set(&mut c, json!(true)));
    assert_eq!(on_return(&mut c), json!("restart"));
    ok(&set(&mut c, Value::Null));
    assert_eq!(on_return(&mut c), Value::Null);
    assert_eq!(shown_key(&mut c), json!(true));
    assert_eq!(kind(&set(&mut c, json!(1))), "invalid_params");
    // One undo step brings the policy back.
    ok(&set(&mut c, json!(false)));
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(on_return(&mut c), Value::Null);
}

#[test]
pub(super) fn properties_activity_choice_is_stored_without_type_defaults_and_shown_in_the_rows() {
    // ADR-0152 phase 2: Properties' "Runs" and "When it comes back" send the policy
    // in force; the type's defaults are stored as absent and the UI rows follow.
    use rbs_core::media::Size;
    use rbs_core::timing::FrameRate;
    use rbs_graph::node::NodeTypeId;
    use rbs_scene::activity::{OnReturn, Run};
    use rbs_scene::model::{ItemContent, Layout, SceneScope, SettingValue, Settings};
    use rbs_studio_host::studio_host::Command;
    struct Want {
        run: Run,
        on_return: Option<OnReturn>,
        linger_s: u16,
    }
    let dir = std::env::temp_dir().join(format!("rbs-film-choice-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("film.mov");
    std::fs::write(&file, b"film").unwrap();
    let mut settings = Settings::new();
    settings.insert(
        "path".to_owned(),
        SettingValue::Text(file.display().to_string()),
    );
    let mut doc = rbs_scene::SceneCollection::default();
    let fps = FrameRate::new(30, 1).unwrap();
    let (main, _) = doc.add_canvas("Main", Size::new(1280, 720), fps).unwrap();
    let (scene, _) = doc.add_scene("Scene", SceneScope::Shared).unwrap();
    doc.set_program(main, Some(scene)).unwrap();
    let film = doc
        .add_source("Film", NodeTypeId::new("org.rbs.source.media"), settings)
        .unwrap()
        .0;
    doc.add_item(
        scene,
        ItemContent::Source(film),
        Layout::new([0.0, 0.0], [640.0, 360.0]),
        None,
    )
    .unwrap();
    let Some((host, updates, _api, mut c)) = start_with_media(Some(doc), None) else {
        return;
    };
    let activity = |c: &mut Client| {
        c.call("source.get_activity", json!({"source": film.get()}))["result"].clone()
    };
    let row_shows = |want: Want| {
        next(&updates, |u| match u {
            Update::Sources(r)
                if r.iter().any(|r| {
                    r.id == film
                        && r.activity.is_some_and(|a| {
                            (a.run, a.on_return, a.linger_s)
                                == (want.run, want.on_return, want.linger_s)
                        })
                }) =>
            {
                Some(())
            }
            _ => None,
        })
    };
    // Only `on_return`: nothing else in the row changes, so the row must say it.
    host.send(Command::Api {
        call: Box::new(rbs_api::Command::SourceSetActivity(
            rbs_api::request::SetSourceActivity {
                source: film.get(),
                run: Some(rbs_api::types::ActivityRun::Shown),
                on_return: Some(rbs_api::types::ActivityReturn::Resume),
                linger_s: Some(0),
            },
        )),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    });
    row_shows(Want {
        run: Run::Shown,
        on_return: Some(OnReturn::Resume),
        linger_s: 0,
    });
    let a = activity(&mut c);
    assert_eq!(a.get("run"), None);
    assert_eq!(a["on_return"], json!("resume"));
    host.send(Command::Api {
        call: Box::new(rbs_api::Command::SourceSetActivity(
            rbs_api::request::SetSourceActivity {
                source: film.get(),
                run: Some(rbs_api::types::ActivityRun::OnAir),
                on_return: Some(rbs_api::types::ActivityReturn::Resume),
                linger_s: Some(30),
            },
        )),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    });
    row_shows(Want {
        run: Run::OnAir,
        on_return: Some(OnReturn::Resume),
        linger_s: 30,
    });
    let a = activity(&mut c);
    assert_eq!(a["run"], json!("on_air"));
    assert_eq!(a["linger_s"], json!(30));
    // Back to the type's defaults: nothing is stored.
    host.send(Command::Api {
        call: Box::new(rbs_api::Command::SourceSetActivity(
            rbs_api::request::SetSourceActivity {
                source: film.get(),
                run: Some(rbs_api::types::ActivityRun::Shown),
                on_return: Some(rbs_api::types::ActivityReturn::Restart),
                linger_s: Some(0),
            },
        )),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    });
    row_shows(Want {
        run: Run::Shown,
        on_return: Some(OnReturn::Restart),
        linger_s: 0,
    });
    let a = activity(&mut c);
    assert_eq!(a.get("run"), None);
    assert_eq!(a.get("on_return"), None);
    assert_eq!(a.get("linger_s"), None);
    // One undo step brings the last choice back.
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(activity(&mut c)["run"], json!("on_air"));
}

#[test]
pub(super) fn a_page_keep_running_setting_is_written_to_its_activity_policy() {
    // ADR-0152 decision 10: `keep_running` is the deprecated spelling of `run: placed`.
    use rbs_core::media::Size;
    use rbs_core::timing::FrameRate;
    use rbs_graph::node::NodeTypeId;
    use rbs_scene::model::{SceneScope, SettingValue, Settings};
    let mut settings = Settings::new();
    settings.insert(
        "url".to_owned(),
        SettingValue::Text("https://example.com/".into()),
    );
    let mut doc = rbs_scene::SceneCollection::default();
    let fps = FrameRate::new(30, 1).unwrap();
    let (main, _) = doc.add_canvas("Main", Size::new(1280, 720), fps).unwrap();
    let (scene, _) = doc.add_scene("Scene", SceneScope::Shared).unwrap();
    doc.set_program(main, Some(scene)).unwrap();
    let page = doc
        .add_source("Page", NodeTypeId::new("org.rbs.source.browser"), settings)
        .unwrap()
        .0
        .get();
    let Some((_host, _updates, _api, mut c)) = start_with_media(Some(doc), None) else {
        return;
    };
    let set = |c: &mut Client, value: Value| {
        c.call(
            "source.set_settings",
            json!({"source": page, "settings": {"keep_running": value}}),
        )
    };
    let run = |c: &mut Client| {
        c.call("source.get_activity", json!({"source": page}))["result"]["run"].clone()
    };
    let kept = |c: &mut Client| {
        c.call("source.get_settings", json!({"source": page}))["result"]["settings"]["keep_running"]
            .clone()
    };
    assert_eq!(kept(&mut c), json!(false));
    ok(&set(&mut c, json!(true)));
    assert_eq!(run(&mut c), json!("placed"));
    assert_eq!(kept(&mut c), json!(true));
    ok(&set(&mut c, json!(false)));
    assert_eq!(run(&mut c), Value::Null);
    // `false` leaves another `run` alone.
    ok(&c.call(
        "source.set_activity",
        json!({"source": page, "run": "on_air"}),
    ));
    ok(&set(&mut c, json!(false)));
    assert_eq!(run(&mut c), json!("on_air"));
    assert_eq!(kept(&mut c), json!(false));
    assert_eq!(kind(&set(&mut c, json!(1))), "invalid_params");
    // The policy takes `placed` directly too.
    ok(&c.call(
        "source.set_activity",
        json!({"source": page, "run": "placed"}),
    ));
    assert_eq!(kept(&mut c), json!(true));
}

/// ADR-0164 decision 2: a client's gesture never joins a window step, even with the
/// window's own number: the server clears the window's bit from it.
#[test]
pub(super) fn a_client_gesture_never_joins_a_window_step() {
    use rbs_scene::model::SourceId;
    use rbs_studio_host::studio_host::{Command, OutputCanvas, WINDOW_GESTURE};
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let raw = add(&mut c, "screen", "Screen");
    let item = sources(&mut c)
        .iter()
        .find(|r| r["id"] == raw)
        .map(|r| r["item"].as_u64().unwrap())
        .unwrap();
    let right = |want: u32| {
        move |u: Update| match u {
            Update::Sources(r) if r[0].layout.is_some_and(|l| l.crop.right == want) => Some(()),
            Update::Error(e) => panic!("{e}"),
            _ => None,
        }
    };
    let gesture = WINDOW_GESTURE | 7;
    host.send(Command::SetSourceCrop {
        seq: 1,
        gesture,
        id: SourceId::from_raw(raw as u32),
        crop: [0, 0, 10, 0],
        keep_scale: false,
        canvas: OutputCanvas::Main,
    });
    next(&updates, right(10));
    ok(&c.call(
        "item.set_crop",
        json!({"item": item, "crop": [0, 0, 20, 0], "keep_scale": false, "gesture": gesture}),
    ));
    next(&updates, right(20));
    ok(&c.call("edit.undo", json!({})));
    let after = next(&updates, |u| match u {
        Update::Sources(r) => r[0].layout.map(|l| l.crop.right).filter(|&x| x != 20),
        _ => None,
    });
    assert_eq!(after, 10, "the client's crop was a step of its own");
    drop(host);
}
