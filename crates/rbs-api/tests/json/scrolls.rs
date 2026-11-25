//! Scrolls on the wire (protocol 1.94, ADR-0150; `on_return` 1.99, ADR-0152).

use super::*;

#[test]
fn scrolls_are_a_minor_addition_with_short_defaults() {
    let plain: ItemScroll = serde_json::from_value(json!({"speed": 80.0})).unwrap();
    assert_eq!(
        plain,
        ItemScroll {
            direction: ScrollDirection::Left,
            speed: 80.0,
            gap: 0.0,
            repeat: ScrollRepeat::Loop,
            start: ScrollStart::Inside,
            fade: 0.0,
            ramp_ms: 300,
            on_return: ScrollReturn::Restart,
        }
    );
    assert_eq!(serde_json::to_value(plain).unwrap(), json!({"speed": 80.0}));
    let all = ItemScroll {
        direction: ScrollDirection::Down,
        repeat: ScrollRepeat::Once,
        start: ScrollStart::Outside,
        ramp_ms: 0,
        ..plain
    };
    assert_eq!(
        serde_json::to_value(all).unwrap(),
        json!({"direction": "down", "speed": 80.0, "repeat": "once",
               "start": "outside", "ramp_ms": 0})
    );
    // Protocol 1.99 (ADR-0152 decision 6): `on_return` `continue`; `restart` left out.
    let goes_on = ItemScroll {
        on_return: ScrollReturn::Continue,
        ..plain
    };
    assert_eq!(
        serde_json::to_value(goes_on).unwrap(),
        json!({"speed": 80.0, "on_return": "continue"})
    );
    assert!(
        serde_json::from_value::<ItemScroll>(json!({"speed": 1.0, "on_return": "resume"})).is_err()
    );
    assert!(serde_json::from_value::<ItemScroll>(json!({})).is_err());
    assert!(serde_json::from_value::<ItemScroll>(json!({"speed": 1.0, "wrap": true})).is_err());
    assert!(
        serde_json::from_value::<ItemScroll>(json!({"speed": 1.0, "direction": "diagonal"}))
            .is_err()
    );
    let layout: ItemLayout = serde_json::from_value(json!({
        "position": [0.0, 0.0], "size": [640.0, 360.0], "rotation": 0.0,
        "crop": [0, 0, 0, 0], "opacity": 1.0, "style": {}
    }))
    .unwrap();
    assert_eq!(layout.scroll, None);
    for method in [
        "item.scroll_pause",
        "item.scroll_resume",
        "item.scroll_restart",
    ] {
        let r: Request =
            serde_json::from_value(json!({"method": method, "params": {"item": 3}})).unwrap();
        assert_eq!(r.scope(), Some(Scope::Control), "{method}");
    }
    let clear: Request = serde_json::from_value(json!({
        "method": "item.set_scroll", "params": {"item": 3, "scroll": null}
    }))
    .unwrap();
    assert_eq!(
        clear,
        Request::Command(Command::ItemSetScroll(SetItemScroll {
            gesture: 0,
            item: 3,
            scroll: None,
            canvas: OutputCanvas::Main
        }))
    );
    assert!(
        serde_json::from_value::<Request>(json!({
            "method": "item.scroll_speed", "params": {"item": 3, "speed": 2.0, "ramp": 1}
        }))
        .is_err()
    );
}

#[test]
fn group_scroll_and_atomic_operations_have_strict_wire_shapes() {
    let set: ApplyGroup = serde_json::from_value(json!({
        "scene": 2, "group": 3, "items": [4, 5],
        "operation": {"type": "set_scroll", "scroll": {"speed": 80.0}}
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(&set).unwrap(),
        json!({
            "scene": 2, "group": 3, "items": [4, 5],
            "operation": {"type": "set_scroll", "scroll": {"speed": 80.0}}
        })
    );
    assert!(
        serde_json::from_value::<GroupOperation>(
            json!({"type": "set_scroll", "scroll": {"speed": 80, "on_return": "continue"}})
        )
        .is_err()
    );
    assert!(serde_json::from_value::<GroupOperation>(json!({"type": "set_scroll"})).is_err());
    assert!(serde_json::from_value::<GroupOperation>(json!({"type": "set_show_for"})).is_err());
    assert_eq!(
        serde_json::from_value::<GroupOperation>(
            json!({"type": "set_show_for", "show_for_ms": null})
        )
        .unwrap(),
        GroupOperation::SetShowFor { show_for_ms: None }
    );
    assert!(
        serde_json::from_value::<GroupOperation>(json!({"type": "show", "extra": true})).is_err()
    );
}

#[test]
fn group_live_actions_are_distinguished_from_saved_edits() {
    for operation in [
        GroupOperation::Show {},
        GroupOperation::ScrollPause {},
        GroupOperation::ScrollResume {},
        GroupOperation::ScrollRestart {},
        GroupOperation::ScrollSpeed { speed: 10.0 },
    ] {
        assert!(operation.is_live());
    }
    assert!(!GroupOperation::SetLocked { locked: true }.is_live());
    assert!(!GroupOperation::SetShowFor { show_for_ms: None }.is_live());
    assert!(!GroupOperation::SetScroll { scroll: None }.is_live());
}

#[test]
fn group_properties_methods_are_registered() {
    assert_eq!(method_effect("group.get"), Some(Effect::Read));
    assert_eq!(method_scope("group.get"), Some(Some(Scope::Read)));
    assert_eq!(method_effect("group.apply"), Some(Effect::Edit));
    assert_eq!(method_scope("group.apply"), Some(Some(Scope::Control)));
    let apply = json!({"method": "group.apply", "params": {
        "scene": 2, "group": 3, "items": [4, 5],
        "operation": {"type": "set_show_for", "show_for_ms": null}
    }});
    let Request::Command(Command::GroupApply(p)) = serde_json::from_value(apply).unwrap() else {
        panic!("group.apply is a command");
    };
    assert_eq!(
        p.operation,
        GroupOperation::SetShowFor { show_for_ms: None }
    );
    assert!(
        serde_json::from_value::<Request>(json!({"method": "group.get", "params": {}})).is_err()
    );
}

#[test]
fn group_state_keeps_explicit_nulls_and_run_identity() {
    let value = json!({
        "scene": 2, "group": 3, "name": "Group", "items": [4],
        "members": [{
            "item": 4, "visible": false, "locked": false, "show_for_ms": null,
            "cued": false, "remaining_ms": null, "has_picture": true
        }],
        "canvas": "main", "bounds": null, "scroll_bounds": null, "scroll": null,
        "run": {"id": 9, "paused": true, "speed": 42.0}, "render_failures": []
    });
    let state: GroupState = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(state).unwrap(), value);
    let request: GetGroup = serde_json::from_value(json!({"scene": 2, "group": 3})).unwrap();
    assert_eq!(request.canvas, OutputCanvas::Main);
    assert!(
        serde_json::from_value::<GetGroup>(json!({
            "scene": 2, "group": 3, "unexpected": true
        }))
        .is_err()
    );
}

#[test]
fn group_saved_operations_have_their_documented_shapes() {
    for (value, operation) in [
        (
            json!({"type": "set_visible", "visible": false}),
            GroupOperation::SetVisible { visible: false },
        ),
        (
            json!({"type": "set_locked", "locked": true}),
            GroupOperation::SetLocked { locked: true },
        ),
        (
            json!({"type": "move", "dx": 1.5, "dy": -2.0}),
            GroupOperation::Move { dx: 1.5, dy: -2.0 },
        ),
        (
            json!({"type": "resize", "x": 0.0, "y": 0.0, "width": 10.0, "height": 5.0}),
            GroupOperation::Resize {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 5.0,
            },
        ),
        (
            json!({"type": "flip", "axis": "vertical"}),
            GroupOperation::Flip {
                axis: Axis::Vertical,
            },
        ),
    ] {
        assert_eq!(
            serde_json::from_value::<GroupOperation>(value.clone()).unwrap(),
            operation
        );
        assert_eq!(serde_json::to_value(&operation).unwrap(), value);
    }
    assert!(serde_json::from_value::<GroupOperation>(json!({"type": "move", "dx": 1.0})).is_err());
}
