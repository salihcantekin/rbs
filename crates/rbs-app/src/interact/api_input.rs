//! An Interact window's input as `browser.input` (ADR-0168 step 4): one wire event
//! becomes the call's events, held buttons included (protocol 1.101).

use rbs_api::request::{
    BrowserHeld, BrowserInput, BrowserInputEvent as E, BrowserModifiers, BrowserMouseButton,
};
use rbs_browser_ipc::input::{self as wire, Button, InputEvent, Key, NamedKey};
use rbs_scene::model::SourceId;

/// `browser.input` carrying `event` for `source`'s page; the window sends it quiet,
/// since a page that stopped meanwhile is not worth an error.
pub fn call(source: SourceId, event: InputEvent) -> rbs_api::Command {
    rbs_api::Command::BrowserInput(BrowserInput {
        source: source.get(),
        events: events(event),
    })
}

fn modifiers(bits: u8) -> BrowserModifiers {
    BrowserModifiers {
        shift: bits & wire::SHIFT != 0,
        ctrl: bits & wire::CTRL != 0,
        alt: bits & wire::ALT != 0,
        meta: bits & wire::META != 0,
    }
}

fn held(bits: u8) -> BrowserHeld {
    BrowserHeld {
        left: bits & wire::LEFT_HELD != 0,
        middle: bits & wire::MIDDLE_HELD != 0,
        right: bits & wire::RIGHT_HELD != 0,
    }
}

/// The DOM name of `key`, as `browser.input` takes it.
fn key_name(key: Key) -> String {
    match key {
        Key::Char(c) => c.to_string(),
        Key::Named(NamedKey::F(n)) => format!("F{n}"),
        Key::Named(k) => wire::NAMED_KEYS
            .iter()
            .find(|(named, _)| *named == k)
            .map_or_else(String::new, |(_, name)| (*name).to_owned()),
    }
}

fn events(event: InputEvent) -> Vec<E> {
    let one = match event {
        InputEvent::Move { x, y, modifiers: m } => E::Move {
            x,
            y,
            modifiers: modifiers(m),
            held: held(m),
        },
        InputEvent::Leave { modifiers: m } => E::Leave {
            modifiers: modifiers(m),
            held: held(m),
        },
        InputEvent::Button {
            x,
            y,
            button,
            down,
            clicks,
            modifiers: m,
        } => E::Button {
            x,
            y,
            button: match button {
                Button::Left => BrowserMouseButton::Left,
                Button::Middle => BrowserMouseButton::Middle,
                Button::Right => BrowserMouseButton::Right,
            },
            down,
            // The page counts a fourth click as a third, as `browser.input` allows.
            clicks: clicks.clamp(1, 3),
            modifiers: modifiers(m),
        },
        InputEvent::Wheel {
            x,
            y,
            dx,
            dy,
            modifiers: m,
        } => E::Wheel {
            x,
            y,
            dx,
            dy,
            modifiers: modifiers(m),
        },
        InputEvent::Key {
            key,
            down,
            modifiers: m,
        } => E::Key {
            key: key_name(key),
            down,
            modifiers: modifiers(m),
        },
        // One text event carries at most `MAX_TEXT` characters: longer input
        // method text goes as several, in order.
        InputEvent::Text(text) => {
            let chars: Vec<char> = text.chars().collect();
            return chars
                .chunks(BrowserInput::MAX_TEXT)
                .map(|c| E::Text {
                    text: c.iter().collect(),
                })
                .collect();
        }
    };
    vec![one]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use super::*;

    /// Every bit the window's event carries reaches the call: modifiers, held
    /// buttons, the key's DOM name (ADR-0168: nothing the page sees changes).
    #[test]
    fn the_window_event_keeps_its_buttons_modifiers_and_keys() {
        let moved = events(InputEvent::Move {
            x: 1,
            y: 2,
            modifiers: wire::SHIFT | wire::LEFT_HELD,
        });
        let E::Move {
            modifiers, held, ..
        } = &moved[0]
        else {
            panic!("{moved:?}")
        };
        assert!(modifiers.shift && !modifiers.ctrl && held.left && !held.right);
        let left = events(InputEvent::Leave {
            modifiers: wire::CTRL | wire::RIGHT_HELD,
        });
        let E::Leave { modifiers, held } = &left[0] else {
            panic!("{left:?}")
        };
        assert!(modifiers.ctrl && held.right && !held.left);
        let click = events(InputEvent::Button {
            x: 3,
            y: 4,
            button: Button::Middle,
            down: true,
            clicks: 4,
            modifiers: wire::META,
        });
        assert!(matches!(
            click[0],
            E::Button {
                button: BrowserMouseButton::Middle,
                clicks: 3,
                ..
            }
        ));
        for (key, name) in [
            (Key::Named(NamedKey::F(5)), "F5"),
            (Key::Named(NamedKey::ArrowLeft), "ArrowLeft"),
            (Key::Char('a'), "a"),
        ] {
            let e = events(InputEvent::Key {
                key,
                down: true,
                modifiers: 0,
            });
            assert!(matches!(&e[0], E::Key { key, .. } if key == name), "{e:?}");
            assert_eq!(Key::parse(name), Some(key));
        }
        let long = "é".repeat(BrowserInput::MAX_TEXT + 1);
        let parts = events(InputEvent::Text(long));
        assert_eq!(parts.len(), 2);
    }
}
