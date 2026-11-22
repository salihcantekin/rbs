//! The wire forms protocol 1.101 adds (ADR-0168 decision 3): the target row's
//! fields, `browser.forget_sign_in`'s `restart` and the held mouse buttons of
//! `browser.input`. Each is additive: absent, it reads as before.

#![allow(clippy::unwrap_used)] // Tests fail by panicking.

use rbs_api::request::{BrowserForgetSignIn, BrowserHeld, BrowserInputEvent, BrowserModifiers};
use rbs_api::types::CaptureTarget;
use serde_json::json;

#[test]
fn a_target_row_names_its_app_rank_mic_sound_and_picker_only_when_it_has_them() {
    let plain = CaptureTarget {
        name: "Display 1".into(),
        adapter: "wgc".into(),
        target: "1".into(),
        ..CaptureTarget::default()
    };
    let wire = serde_json::to_value(&plain).unwrap();
    for absent in ["app", "recent", "mic", "sound", "picker"] {
        assert!(wire.get(absent).is_none(), "{absent} in {wire}");
    }
    let full = CaptureTarget {
        app: Some("Editor".into()),
        recent: Some(0),
        mic: Some("Camera mic".into()),
        sound: true,
        picker: true,
        ..plain.clone()
    };
    let wire = serde_json::to_value(&full).unwrap();
    assert_eq!(wire["app"], "Editor");
    assert_eq!(wire["recent"], 0);
    assert_eq!(wire["mic"], "Camera mic");
    assert_eq!(wire["sound"], true);
    assert_eq!(wire["picker"], true);
    assert_eq!(serde_json::from_value::<CaptureTarget>(wire).unwrap(), full);
}

#[test]
fn forget_sign_in_restarts_only_when_asked() {
    let old: BrowserForgetSignIn = serde_json::from_value(json!({"source": 3})).unwrap();
    assert!(!old.restart);
    let new: BrowserForgetSignIn =
        serde_json::from_value(json!({"source": 3, "restart": true})).unwrap();
    assert!(new.restart);
    assert_eq!(
        serde_json::to_value(old).unwrap(),
        json!({"source": 3}),
        "false is left out"
    );
}

#[test]
fn moves_and_leaves_carry_the_held_buttons_and_a_leave_its_modifiers() {
    let e: BrowserInputEvent =
        serde_json::from_value(json!({"type": "move", "x": 1, "y": 2, "held": ["left", "right"]}))
            .unwrap();
    let BrowserInputEvent::Move { held, .. } = e else {
        panic!("{e:?}")
    };
    assert_eq!(
        held,
        BrowserHeld {
            left: true,
            middle: false,
            right: true,
        }
    );
    let wire = serde_json::to_value(&e).unwrap();
    assert_eq!(wire["held"], json!(["left", "right"]));

    // A 1.100 leave still reads, with nothing held.
    let old: BrowserInputEvent = serde_json::from_value(json!({"type": "leave"})).unwrap();
    assert_eq!(
        old,
        BrowserInputEvent::Leave {
            modifiers: BrowserModifiers::default(),
            held: BrowserHeld::default(),
        }
    );
    assert_eq!(
        serde_json::to_value(&old).unwrap(),
        json!({"type": "leave"})
    );
    let leave: BrowserInputEvent = serde_json::from_value(
        json!({"type": "leave", "modifiers": {"shift": true}, "held": ["middle"]}),
    )
    .unwrap();
    let BrowserInputEvent::Leave { modifiers, held } = leave else {
        panic!("{leave:?}")
    };
    assert!(modifiers.shift && held.middle && !held.left);
    assert!(
        serde_json::from_value::<BrowserInputEvent>(
            json!({"type": "move", "x": 1, "y": 2, "held": ["thumb"]})
        )
        .is_err(),
        "an unknown button is refused"
    );
}
