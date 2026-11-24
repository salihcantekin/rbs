//! `group.*` and the `group` field of `scene.sources` rows (protocol 1.104, ADR-0182).

use super::*;

fn rows(c: &mut Client, scene: u64) -> Vec<Value> {
    ok(&c.call("scene.sources", json!({"scene": scene})))["sources"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn groups_are_made_listed_renamed_ungrouped_and_undone_one_step_each() {
    let Some((host, _updates, _api, mut c)) = start() else {
        return;
    };
    let a = add(&mut c, "screen", "A");
    let b = add(&mut c, "screen", "B");
    let third = add(&mut c, "screen", "C");
    let scene = scenes(&mut c)["program"].as_u64().unwrap();
    let item = |c: &mut Client, source: u64| {
        rows(c, scene)
            .iter()
            .find(|r| r["id"] == source)
            .and_then(|r| r["item"].as_u64())
            .unwrap()
    };
    let (ia, ib, ic) = (item(&mut c, a), item(&mut c, b), item(&mut c, third));
    assert!(rows(&mut c, scene).iter().all(|r| r.get("group").is_none()));

    let made = c.call(
        "group.create",
        json!({"scene": scene, "items": [ia, ib], "name": "Pair"}),
    );
    let group = ok(&made)["group"].as_u64().unwrap();
    assert_eq!(ok(&made)["name"], "Pair");
    let in_group = |c: &mut Client| {
        rows(c, scene)
            .iter()
            .filter(|r| r["group"] == group)
            .count()
    };
    assert_eq!(in_group(&mut c), 2);
    let listed = c.call("group.list", json!({"scene": scene}));
    let list = &ok(&listed)["groups"];
    assert_eq!(list[0]["name"], "Pair");
    assert_eq!(list[0]["items"].as_array().unwrap().len(), 2);

    // Refusals leave the scene as it was.
    assert_eq!(
        kind(&c.call("group.create", json!({"scene": scene, "items": [ic]}))),
        "invalid_params"
    );
    assert_eq!(
        kind(&c.call(
            "group.rename",
            json!({"scene": scene, "group": 99, "name": "x"})
        )),
        "not_found"
    );
    assert_eq!(
        kind(&c.call(
            "group.rename",
            json!({"scene": scene, "group": group, "name": " "})
        )),
        "invalid_params"
    );

    ok(&c.call(
        "group.rename",
        json!({"scene": scene, "group": group, "name": "Duo"}),
    ));
    assert_eq!(
        ok(&c.call("group.list", json!({"scene": scene})))["groups"][0]["name"],
        "Duo"
    );
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(
        ok(&c.call("group.list", json!({"scene": scene})))["groups"][0]["name"],
        "Pair"
    );

    ok(&c.call("group.remove", json!({"scene": scene, "group": group})));
    assert_eq!(in_group(&mut c), 0);
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(in_group(&mut c), 2);
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(in_group(&mut c), 0);
    drop(host);
}

/// Hide, lock and remove calls that share a gesture are one undo step (protocol 1.105).
#[test]
fn a_gesture_makes_a_groups_eye_lock_and_removal_one_undo_step() {
    let Some((host, _updates, _api, mut c)) = start() else {
        return;
    };
    let a = add(&mut c, "screen", "A");
    let b = add(&mut c, "screen", "B");
    let scene = scenes(&mut c)["program"].as_u64().unwrap();
    let items: Vec<u64> = [a, b]
        .iter()
        .map(|s| {
            rows(&mut c, scene)
                .iter()
                .find(|r| r["id"] == *s)
                .and_then(|r| r["item"].as_u64())
                .unwrap()
        })
        .collect();
    let hidden = |c: &mut Client| {
        rows(c, scene)
            .iter()
            .filter(|r| r["visible"] == false)
            .count()
    };
    for item in &items {
        ok(&c.call(
            "item.set_visible",
            json!({"item": item, "visible": false, "gesture": 7}),
        ));
    }
    assert_eq!(hidden(&mut c), 2);
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(hidden(&mut c), 0, "one undo shows both again");
    for item in &items {
        ok(&c.call("item.remove", json!({"item": item, "gesture": 8})));
    }
    assert_eq!(rows(&mut c, scene).len(), 0);
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(rows(&mut c, scene).len(), 2, "one undo brings both back");
    drop(host);
}

#[test]
fn items_are_aligned_and_distributed_as_one_undo_step() {
    let Some((host, _updates, _api, mut c)) = start() else {
        return;
    };
    let ids: Vec<u64> = ["A", "B", "C"]
        .iter()
        .map(|n| add(&mut c, "screen", n))
        .collect();
    let scene = scenes(&mut c)["program"].as_u64().unwrap();
    let items: Vec<u64> = ids
        .iter()
        .map(|s| {
            rows(&mut c, scene)
                .iter()
                .find(|r| r["id"] == *s)
                .and_then(|r| r["item"].as_u64())
                .unwrap()
        })
        .collect();
    let place = |c: &mut Client, item: u64, x: f64, y: f64| {
        let source = ids[items.iter().position(|i| *i == item).unwrap()];
        ok(&c.call(
            "item.set_box",
            json!({"source": source, "position": [x, y], "size": [100.0, 100.0]}),
        ));
    };
    place(&mut c, items[0], 0.0, 10.0);
    place(&mut c, items[1], 150.0, 300.0);
    place(&mut c, items[2], 700.0, 50.0);
    let x_of = |c: &mut Client, i: usize| {
        rows(c, scene)
            .iter()
            .find(|r| r["item"] == items[i])
            .unwrap()["layout"]["position"][0]
            .as_f64()
            .unwrap()
    };
    let y_of = |c: &mut Client, i: usize| {
        rows(c, scene)
            .iter()
            .find(|r| r["item"] == items[i])
            .unwrap()["layout"]["position"][1]
            .as_f64()
            .unwrap()
    };
    // Distribute: the outer two stay, the middle one has equal gaps (gap (800 - 300) / 2 = 250).
    ok(&c.call(
        "item.distribute",
        json!({"items": items, "axis": "horizontal"}),
    ));
    assert_eq!(
        (x_of(&mut c, 0), x_of(&mut c, 1), x_of(&mut c, 2)),
        (0.0, 350.0, 700.0)
    );
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(x_of(&mut c, 1), 150.0);
    // Align top: all three on the smallest top, one undo step.
    ok(&c.call("item.align_items", json!({"items": items, "edge": "top"})));
    assert_eq!(
        (y_of(&mut c, 0), y_of(&mut c, 1), y_of(&mut c, 2)),
        (10.0, 10.0, 10.0)
    );
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(
        (y_of(&mut c, 0), y_of(&mut c, 1), y_of(&mut c, 2)),
        (10.0, 300.0, 50.0)
    );
    // Refusals.
    assert_eq!(
        kind(&c.call(
            "item.align_items",
            json!({"items": [items[0]], "edge": "left"})
        )),
        "invalid_params"
    );
    assert_eq!(
        kind(&c.call(
            "item.distribute",
            json!({"items": [items[0], items[1]], "axis": "vertical"})
        )),
        "invalid_params"
    );
    ok(&c.call(
        "item.align_items",
        json!({"items": [items[0]], "edge": "right", "to": "canvas"}),
    ));
    ok(&c.call("item.set_locked", json!({"item": items[1], "locked": true})));
    assert_eq!(
        kind(&c.call("item.align_items", json!({"items": items, "edge": "left"}))),
        "conflict"
    );
    drop(host);
}
