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
