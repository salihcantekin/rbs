//! The browser source's requests: `browser.reload`, `browser.run_script`,
//! `browser.forget_sign_in` and `browser.input` (ADR-0132, ADR-0149, ADR-0168).

use serde::{Deserialize, Serialize};

/// `browser.reload` (protocol 1.91, ADR-0132 decision 5): reloads a browser source's
/// running page without a new process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserReload {
    /// The browser source's id.
    pub source: u32,
    /// Load again from the network, not the cache.
    #[serde(default)]
    pub bypass_cache: bool,
}

/// `browser.run_script` (protocol 1.91, ADR-0149 decision 7): runs `script`, or the
/// saved script when absent, once in the running page, on the saved origin only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserRunScript {
    /// The browser source's id.
    pub source: u32,
    /// The script to run; absent for the saved one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
}

/// `browser.forget_sign_in` (protocol 1.96, ADR-0132 decision 10): deletes the kept
/// profile of a browser source that keeps signed in. Cannot be undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserForgetSignIn {
    /// The browser source's id.
    pub source: u32,
    /// Restart a running page with the new profile instead of answering `busy`
    /// (protocol 1.101, ADR-0168 decision 3).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restart: bool,
}

/// `browser.input` (protocol 1.96, ADR-0132 decisions 6 and 9): pointer, wheel, key
/// and text events for a browser source's running page, in page pixels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserInput {
    /// The browser source's id.
    pub source: u32,
    /// The events, in order; at most [`BrowserInput::MAX_EVENTS`].
    pub events: Vec<BrowserInputEvent>,
}

impl BrowserInput {
    /// The most events one call may carry.
    pub const MAX_EVENTS: usize = 64;
    /// The most characters one `text` event may carry.
    pub const MAX_TEXT: usize = 256;
}

/// A mouse button of [`BrowserInputEvent::Button`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserMouseButton {
    /// The primary button.
    Left,
    /// The wheel button.
    Middle,
    /// The secondary button.
    Right,
}

/// Modifier keys held during a [`BrowserInputEvent`]; absent fields are false.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserModifiers {
    /// Shift.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shift: bool,
    /// Control.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ctrl: bool,
    /// Alt (Option on macOS).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub alt: bool,
    /// The Windows key or Command.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub meta: bool,
}

impl BrowserModifiers {
    /// No modifier is held.
    pub fn is_none(&self) -> bool {
        !(self.shift || self.ctrl || self.alt || self.meta)
    }
}

/// The mouse buttons held during a [`BrowserInputEvent::Move`] or
/// [`BrowserInputEvent::Leave`] (protocol 1.101, ADR-0168 decision 3), on the wire a
/// list of [`BrowserMouseButton`] names; absent when none is held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BrowserHeld {
    /// The primary button.
    pub left: bool,
    /// The wheel button.
    pub middle: bool,
    /// The secondary button.
    pub right: bool,
}

impl BrowserHeld {
    /// No button is held.
    pub fn is_none(&self) -> bool {
        !(self.left || self.middle || self.right)
    }
}

impl Serialize for BrowserHeld {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let held = [
            (self.left, BrowserMouseButton::Left),
            (self.middle, BrowserMouseButton::Middle),
            (self.right, BrowserMouseButton::Right),
        ];
        let mut seq = serializer.serialize_seq(None)?;
        for (_, button) in held.iter().filter(|(on, _)| *on) {
            seq.serialize_element(button)?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for BrowserHeld {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Buttons;
        impl<'de> serde::de::Visitor<'de> for Buttons {
            type Value = BrowserHeld;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a list of mouse buttons")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<BrowserHeld, A::Error> {
                let mut held = BrowserHeld::default();
                while let Some(button) = seq.next_element::<BrowserMouseButton>()? {
                    match button {
                        BrowserMouseButton::Left => held.left = true,
                        BrowserMouseButton::Middle => held.middle = true,
                        BrowserMouseButton::Right => held.right = true,
                    }
                }
                Ok(held)
            }
        }
        deserializer.deserialize_seq(Buttons)
    }
}

/// One event of `browser.input`. Coordinates are page pixels, `0..width` and
/// `0..height` of the source's page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrowserInputEvent {
    /// The pointer moved to `x`, `y`.
    Move {
        /// Page x.
        x: i32,
        /// Page y.
        y: i32,
        /// Held modifiers.
        #[serde(default)]
        modifiers: BrowserModifiers,
        /// Held mouse buttons, which a drag inside the page needs (protocol 1.101).
        #[serde(default, skip_serializing_if = "BrowserHeld::is_none")]
        held: BrowserHeld,
    },
    /// The pointer left the page.
    Leave {
        /// Held modifiers (protocol 1.101).
        #[serde(default, skip_serializing_if = "BrowserModifiers::is_none")]
        modifiers: BrowserModifiers,
        /// Held mouse buttons (protocol 1.101).
        #[serde(default, skip_serializing_if = "BrowserHeld::is_none")]
        held: BrowserHeld,
    },
    /// A button went down or up at `x`, `y`; `clicks` counts a double click as 2.
    Button {
        /// Page x.
        x: i32,
        /// Page y.
        y: i32,
        /// Which button.
        button: BrowserMouseButton,
        /// Pressed (true) or released.
        down: bool,
        /// 1 for a click, 2 for a double click, 3 for a triple click.
        #[serde(default = "one_click")]
        clicks: u8,
        /// Held modifiers.
        #[serde(default)]
        modifiers: BrowserModifiers,
    },
    /// The wheel turned at `x`, `y` by `dx`, `dy` pixels (positive `dy` scrolls up).
    Wheel {
        /// Page x.
        x: i32,
        /// Page y.
        y: i32,
        /// Horizontal pixels.
        #[serde(default)]
        dx: i32,
        /// Vertical pixels.
        #[serde(default)]
        dy: i32,
        /// Held modifiers.
        #[serde(default)]
        modifiers: BrowserModifiers,
    },
    /// A key went down or up. `key` is a key name of the DOM `KeyboardEvent.key`
    /// list that has no text (`Enter`, `Tab`, `Backspace`, `Escape`, `Delete`,
    /// `Insert`, `Home`, `End`, `PageUp`, `PageDown`, `ArrowLeft`, `ArrowRight`,
    /// `ArrowUp`, `ArrowDown`, `F1` to `F12`), or one character for a shortcut
    /// (`a` with `ctrl`). Text is typed with `text`.
    Key {
        /// The key.
        key: String,
        /// Pressed (true) or released.
        down: bool,
        /// Held modifiers.
        #[serde(default)]
        modifiers: BrowserModifiers,
    },
    /// Typed text, as the keyboard and input method produced it.
    Text {
        /// The characters.
        text: String,
    },
}

fn one_click() -> u8 {
    1
}
