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
