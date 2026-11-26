//! `scene.set_active`, `scene.step` and `scene.position` (protocol 1.110, ADR-0195).

use super::*;

fn add_scene(c: &mut Client, name: &str) -> u64 {
    ok(&c.call("scene.add", json!({"name": name})))["scene"]
        .as_u64()
        .unwrap()
}

fn active_of(c: &mut Client, scene: u64) -> bool {
    scenes(c)["scenes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == scene)
        .map(|s| s.get("active").is_none_or(|a| a == true))
        .unwrap()
}

#[test]
fn an_inactive_scene_is_skipped_by_step_and_position_and_refused_on_air() {
    let Some((host, _updates, _api, mut c)) = start() else {
        return;
    };
    ok(&c.call("events.subscribe", json!({"topics": ["scene.changed"]})));
    let one = scenes(&mut c)["program"].as_u64().unwrap();
    let two = add_scene(&mut c, "Two");
    let three = add_scene(&mut c, "Three");

    // The program scene may not become inactive; others may.
    assert_eq!(
        kind(&c.call("scene.set_active", json!({"scene": one, "active": false}))),
        "conflict"
    );
    ok(&c.call("scene.set_active", json!({"scene": two, "active": false})));
    c.event("scene.changed", |d| {
        d["scenes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == two && s["active"] == false)
    });
    assert!(!active_of(&mut c, two));

    // Step skips the inactive scene.
    ok(&c.call("scene.step", json!({"step": 1})));
    c.event("scene.changed", |d| d["program"] == three);
    ok(&c.call("scene.step", json!({"step": 1})));
    c.event("scene.changed", |d| d["program"] == one);
    // Slots address the full list: slot 2 is inactive, slot 3 is not.
    assert_eq!(
        kind(&c.call("scene.position", json!({"n": 2}))),
        "conflict"
    );
    ok(&c.call("scene.position", json!({"n": 3})));
    c.event("scene.changed", |d| d["program"] == three);
    assert_eq!(
        kind(&c.call("scene.position", json!({"n": 4}))),
        "not_found"
    );

    // An inactive scene still switches on air by id.
    ok(&c.call("scene.switch", json!({"scene": two})));
    c.event("scene.changed", |d| d["program"] == two);

    // Undo activates it; redoing the deactivation would take the on-air scene off.
    ok(&c.call("edit.undo", json!({})));
    assert!(active_of(&mut c, two));
    assert_eq!(kind(&c.call("edit.redo", json!({}))), "undo_conflict");
    assert!(active_of(&mut c, two));

    // A switch away allows it.
    ok(&c.call("scene.switch", json!({"scene": one})));
    ok(&c.call("edit.redo", json!({})));
    assert!(!active_of(&mut c, two));
    drop(host);
}
