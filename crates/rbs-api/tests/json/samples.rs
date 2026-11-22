//! Samples the round-trip tests share that are too long to sit in them.

use super::*;

/// One `browser.input` event of each kind, for `every_request`.
pub(crate) fn browser_events() -> Vec<BrowserInputEvent> {
    vec![
        BrowserInputEvent::Move {
            x: 1,
            y: 2,
            modifiers: BrowserModifiers::default(),
            held: Default::default(),
        },
        BrowserInputEvent::Leave {
            modifiers: BrowserModifiers::default(),
            held: Default::default(),
        },
        BrowserInputEvent::Button {
            x: 1,
            y: 2,
            button: BrowserMouseButton::Left,
            down: true,
            clicks: 2,
            modifiers: BrowserModifiers {
                shift: true,
                ..BrowserModifiers::default()
            },
        },
        BrowserInputEvent::Wheel {
            x: 1,
            y: 2,
            dx: 0,
            dy: -120,
            modifiers: BrowserModifiers::default(),
        },
        BrowserInputEvent::Key {
            key: "Enter".to_owned(),
            down: false,
            modifiers: BrowserModifiers::default(),
        },
        BrowserInputEvent::Text {
            text: "hi".to_owned(),
        },
    ]
}

/// A display as `source.targets` lists it.
pub(crate) fn capture_target() -> CaptureTarget {
    CaptureTarget {
        name: "Display 1".into(),
        adapter: "wgc".into(),
        target: "1".into(),
        is_default: true,
        size: Some(Dimensions {
            width: 2560,
            height: 1440,
        }),
        ..CaptureTarget::default()
    }
}
