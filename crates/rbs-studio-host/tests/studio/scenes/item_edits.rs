//! Layer order to a place and the item edits of protocol 1.100 (ADR-0164 decision 6).

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
