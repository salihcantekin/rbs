//! Layer order to a place, the item edits, the voice chain and the stinger file of
//! protocol 1.100 (ADR-0164 decision 6).

use super::*;

fn order(c: &mut Client) -> Vec<String> {
    sources(c)
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect()
}

fn item_of(c: &mut Client, source: u64) -> u64 {
    sources(c)
        .iter()
        .find(|s| s["id"] == source)
        .and_then(|s| s["item"].as_u64())
        .unwrap()
}

/// `item.move` / `scene.move` with `to_item` / `to_scene`: the moved one takes the
/// target's place; both or neither of `direction` and the place is `invalid_params`.
#[test]
fn a_move_to_a_place_takes_its_position_and_needs_one_way() {
    let Some((host, _updates, _api, mut c)) = start() else {
        return;
    };
    let a = add(&mut c, "screen", "A");
    add(&mut c, "screen", "B");
    let c3 = add(&mut c, "screen", "C");
    let before = order(&mut c);
    let (ia, ic) = (item_of(&mut c, a), item_of(&mut c, c3));
    ok(&c.call("item.move", json!({"item": ia, "to_item": ic})));
    let after = order(&mut c);
    assert_ne!(after, before);
    let pos = |list: &[String], n: &str| list.iter().position(|x| x == n).unwrap();
    assert_eq!(
        pos(&after, "A"),
        pos(&before, "C"),
        "{before:?} -> {after:?}"
    );
    for params in [
        json!({"item": ia}),
        json!({"item": ia, "direction": "up", "to_item": ic}),
    ] {
        assert_eq!(kind(&c.call("item.move", params)), "invalid_params");
    }
    // Undo puts the order back.
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(order(&mut c), before);

    let two = ok(&c.call("scene.add", json!({"name": "Two"})))["scene"]
        .as_u64()
        .unwrap();
    let list = scenes(&mut c);
    let first = list["scenes"][0]["id"].as_u64().unwrap();
    ok(&c.call("scene.move", json!({"scene": two, "to_scene": first})));
    assert_eq!(names(&scenes(&mut c))[0], "Two");
    assert_eq!(
        kind(&c.call("scene.move", json!({"scene": two}))),
        "invalid_params"
    );
    drop(host);
}

/// `item.set_filled` is for a colour source; `item.set_shown_region` without a seen
/// picture and `item.fit_text_box` on a screen are refused in words.
#[test]
fn the_type_specific_item_edits_name_their_type() {
    let Some((host, _updates, _api, mut c)) = start() else {
        return;
    };
    let screen = add(&mut c, "screen", "Screen");
    let item = item_of(&mut c, screen);
    let filled = c.call("item.set_filled", json!({"item": item, "on": false}));
    assert_eq!(kind(&filled), "invalid_params", "{filled}");
    let fit = c.call("item.fit_text_box", json!({"item": item}));
    assert_eq!(kind(&fit), "invalid_params", "{fit}");
    let gone = c.call(
        "item.set_shown_region",
        json!({"item": 999, "region": "left_half"}),
    );
    assert_eq!(kind(&gone), "not_found", "{gone}");
    drop(host);
}

/// `filter.add_voice_chain` puts the four voice filters on a sound source in one undo
/// step and refuses a picture source; `collection.set_stinger_file` needs an absolute
/// path and answers the new source.
#[test]
fn the_voice_chain_and_the_stinger_file_are_api_calls() {
    let Some((host, _updates, _api, mut c)) = start() else {
        return;
    };
    let mic = add(&mut c, "audio_input", "Mic");
    let screen = add(&mut c, "screen", "Screen");
    let filters = |c: &mut Client| {
        sources(c)
            .iter()
            .find(|s| s["id"] == mic)
            .and_then(|s| s["filters"].as_array().map(Vec::len))
            .unwrap_or(0)
    };
    ok(&c.call("filter.add_voice_chain", json!({"source": mic})));
    assert_eq!(filters(&mut c), 4);
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(filters(&mut c), 0);
    let picture = c.call("filter.add_voice_chain", json!({"source": screen}));
    assert_eq!(kind(&picture), "invalid_params", "{picture}");

    let relative = c.call("collection.set_stinger_file", json!({"path": "sting.mov"}));
    assert_eq!(kind(&relative), "invalid_params", "{relative}");
    let dir = std::env::temp_dir().join(format!("rbs-stinger-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("sting.mov");
    std::fs::write(&file, b"demo").unwrap();
    let set = c.call(
        "collection.set_stinger_file",
        json!({"path": file.display().to_string()}),
    );
    assert!(ok(&set)["source"].as_u64().is_some(), "{set}");
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `item.set_rotation` (ADR-0166): wrapped into `0..360`, -360 to 360 only, an
/// unchanged angle records nothing, and calls with one gesture are one undo step.
#[test]
fn a_free_angle_wraps_and_a_drag_is_one_undo_step() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    let screen = add(&mut c, "screen", "Screen");
    let item = item_of(&mut c, screen);
    let angle = |c: &mut Client| {
        sources(c)
            .iter()
            .find(|s| s["id"] == screen)
            .and_then(|s| s["layout"]["rotation"].as_f64())
            .unwrap_or(0.0)
    };
    for bad in [361.0, -400.5] {
        let r = c.call("item.set_rotation", json!({"item": item, "degrees": bad}));
        assert_eq!(kind(&r), "invalid_params", "{r}");
    }
    ok(&c.call("item.set_rotation", json!({"item": item, "degrees": -90})));
    assert_eq!(angle(&mut c), 270.0);
    for d in [10, 20, 30] {
        let p = json!({"item": item, "degrees": d, "gesture": 5});
        ok(&c.call("item.set_rotation", p));
    }
    assert_eq!(angle(&mut c), 30.0);
    // The drag is one step: one undo goes back to 270.
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(angle(&mut c), 270.0);
    // A window drag (bit 63) and a client call with the same number stay apart.
    let window = rbs_studio_host::studio_host::WINDOW_GESTURE | 9;
    host.send(rbs_studio_host::studio_host::Command::Api {
        call: Box::new(rbs_api::Command::ItemSetRotation(
            rbs_api::request::SetItemRotation {
                item: item as u32,
                degrees: 45.0,
                canvas: rbs_api::types::OutputCanvas::Main,
                gesture: window,
            },
        )),
        reply: rbs_studio_host::studio_host::ApiReply::Shown,
    });
    next(&updates, |u| match u {
        Update::Sources(r)
            if r.iter()
                .any(|r| r.layout.is_some_and(|l| l.rotation == 45.0)) =>
        {
            Some(())
        }
        _ => None,
    });
    let p = json!({"item": item, "degrees": 60, "gesture": window});
    ok(&c.call("item.set_rotation", p));
    assert_eq!(angle(&mut c), 60.0);
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(
        angle(&mut c),
        45.0,
        "the client's angle was a step of its own"
    );
    let gone = c.call("item.set_rotation", json!({"item": 999, "degrees": 0}));
    assert_eq!(kind(&gone), "not_found", "{gone}");
    drop(host);
}

/// `source.add` with `stamp`, `path` or `url` (ADR-0164 decision 6): each belongs to
/// its kind, a path is absolute and never beside a target, and the source is placed.
#[test]
fn a_shape_a_file_or_a_page_is_added_by_its_kind() {
    let Some((host, _updates, _api, mut c)) = start() else {
        return;
    };
    let shape = c.call(
        "source.add",
        json!({"kind": "color", "stamp": "rounded_rectangle", "name": "Badge"}),
    );
    let id = ok(&shape)["source"].as_u64().unwrap();
    let row = sources(&mut c).into_iter().find(|s| s["id"] == id).unwrap();
    assert_eq!(row["name"], "Badge", "{row}");
    for params in [
        json!({"kind": "screen", "stamp": "circle"}),
        json!({"kind": "color", "stamp": "circle", "global": true}),
        json!({"kind": "image", "path": "picture.png"}),
        json!({"kind": "media", "path": "/a.mov", "adapter": "x", "target": "y"}),
        json!({"kind": "media", "url": "https://example.com/"}),
    ] {
        let r = c.call("source.add", params.clone());
        assert_eq!(kind(&r), "invalid_params", "{params} {r}");
    }
    let dir = std::env::temp_dir().join(format!("rbs-add-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("clip.mov");
    std::fs::write(&file, b"demo").unwrap();
    let path = file.display().to_string();
    let not_png = c.call("source.add", json!({"kind": "image", "path": path}));
    assert_eq!(kind(&not_png), "invalid_params", "{not_png}");
    assert!(!not_png.to_string().contains("clip.mov"), "{not_png}");
    let media = c.call("source.add", json!({"kind": "media", "path": path}));
    let media = ok(&media)["source"].as_u64().unwrap();
    assert!(sources(&mut c).iter().any(|s| s["id"] == media));
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}
