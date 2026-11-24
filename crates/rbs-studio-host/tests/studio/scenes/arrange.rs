//! `item.align_items` and `item.distribute` refusals and edge cases (protocol 1.106,
//! ADR-0182), beside the happy path in `groups.rs`.

use super::groups::rows;
use super::*;

fn setup(c: &mut Client, names: &[&str]) -> (Vec<u64>, Vec<u64>, u64) {
    let sources: Vec<u64> = names.iter().map(|n| add(c, "screen", n)).collect();
    let scene = scenes(c)["program"].as_u64().unwrap();
    let items = sources
        .iter()
        .map(|s| {
            rows(c, scene)
                .iter()
                .find(|r| r["id"] == *s)
                .and_then(|r| r["item"].as_u64())
                .unwrap()
        })
        .collect();
    (sources, items, scene)
}

fn set_box(c: &mut Client, source: u64, pos: [f64; 2], size: [f64; 2]) {
    ok(&c.call(
        "item.set_box",
        json!({"source": source, "position": pos, "size": size}),
    ));
}

fn at(c: &mut Client, scene: u64, item: u64) -> (f64, f64) {
    let rows = rows(c, scene);
    let p = &rows.iter().find(|r| r["item"] == item).unwrap()["layout"]["position"];
    (p[0].as_f64().unwrap(), p[1].as_f64().unwrap())
}

#[test]
fn align_to_the_canvas_takes_one_or_more_items_and_refuses_bad_lists() {
    let Some((host, _updates, _api, mut c)) = start() else {
        return;
    };
    let (sources, items, scene) = setup(&mut c, &["A", "B"]);
    set_box(&mut c, sources[0], [40.0, 30.0], [100.0, 100.0]);
    set_box(&mut c, sources[1], [300.0, 200.0], [100.0, 100.0]);
    ok(&c.call(
        "item.align_items",
        json!({"items": items, "edge": "left", "to": "canvas"}),
    ));
    assert_eq!(at(&mut c, scene, items[0]).0, 0.0);
    assert_eq!(at(&mut c, scene, items[1]).0, 0.0);
    ok(&c.call(
        "item.align_items",
        json!({"items": [items[1]], "edge": "top", "to": "canvas"}),
    ));
    assert_eq!(at(&mut c, scene, items[1]).1, 0.0);
    // Empty list, repeated item, one item to the selection.
    for params in [
        json!({"items": [], "edge": "top", "to": "canvas"}),
        json!({"items": [items[0], items[0]], "edge": "top"}),
        json!({"items": [items[0]], "edge": "top"}),
    ] {
        assert_eq!(kind(&c.call("item.align_items", params)), "invalid_params");
    }
    drop(host);
}

#[test]
fn a_locked_item_conflicts_even_behind_an_unknown_one_and_nothing_moves() {
    let Some((host, _updates, _api, mut c)) = start() else {
        return;
    };
    let (sources, items, scene) = setup(&mut c, &["A", "B"]);
    set_box(&mut c, sources[0], [40.0, 30.0], [100.0, 100.0]);
    set_box(&mut c, sources[1], [300.0, 200.0], [100.0, 100.0]);
    ok(&c.call("item.set_locked", json!({"item": items[1], "locked": true})));
    let before = (at(&mut c, scene, items[0]), at(&mut c, scene, items[1]));
    assert_eq!(
        kind(&c.call("item.align_items", json!({"items": items, "edge": "left"}))),
        "conflict"
    );
    assert_eq!(
        kind(&c.call(
            "item.align_items",
            json!({"items": [items[0], 99999], "edge": "left"})
        )),
        "not_found"
    );
    assert_eq!(
        before,
        (at(&mut c, scene, items[0]), at(&mut c, scene, items[1]))
    );
    drop(host);
}

#[test]
fn a_rotated_item_is_measured_by_the_box_it_draws() {
    let Some((host, _updates, _api, mut c)) = start() else {
        return;
    };
    let (sources, items, scene) = setup(&mut c, &["A", "B"]);
    // 200 x 100 at (0, 0) turned 90 degrees draws 100 x 200 around (100, 50): left edge 50.
    set_box(&mut c, sources[0], [0.0, 0.0], [200.0, 100.0]);
    ok(&c.call(
        "item.set_rotation",
        json!({"item": items[0], "degrees": 90.0}),
    ));
    set_box(&mut c, sources[1], [300.0, 300.0], [100.0, 100.0]);
    ok(&c.call("item.align_items", json!({"items": items, "edge": "left"})));
    assert_eq!(at(&mut c, scene, items[0]).0, 0.0);
    assert_eq!(at(&mut c, scene, items[1]).0, 50.0);
    drop(host);
}
