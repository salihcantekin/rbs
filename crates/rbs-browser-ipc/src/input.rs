//! Input for a running page (ADR-0132 decisions 6 and 8): pointer, wheel, key and text
//! events in page pixels, from the Interact window and `browser.input`. The runner
//! hands them to its engine; nothing here knows CEF. Key names follow the DOM
//! `KeyboardEvent.key` list; [`windows_key_code`] gives the code engines take.

/// Modifier bit: Shift.
pub const SHIFT: u8 = 1;
/// Modifier bit: Control.
pub const CTRL: u8 = 2;
/// Modifier bit: Alt (Option).
pub const ALT: u8 = 4;
/// Modifier bit: the Windows key or Command.
pub const META: u8 = 8;
/// Modifier bit: the left button is held (a drag).
pub const LEFT_HELD: u8 = 16;
/// Modifier bit: the middle button is held.
pub const MIDDLE_HELD: u8 = 32;
/// Modifier bit: the right button is held.
pub const RIGHT_HELD: u8 = 64;

/// The most bytes of text one [`InputEvent::Text`] carries.
pub const MAX_TEXT_BYTES: usize = 1024;

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    /// The primary button.
    Left,
    /// The wheel button.
    Middle,
    /// The secondary button.
    Right,
}

/// A key without text, or one character for a shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A named key (see [`NAMED_KEYS`]).
    Named(NamedKey),
    /// A character key, for shortcuts such as Ctrl+A; typed text is
    /// [`InputEvent::Text`].
    Char(char),
}

/// The keys without text a page gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum NamedKey {
    Enter,
    Tab,
    Backspace,
    Escape,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    /// F1 to F12.
    F(u8),
}

/// Every named key with its DOM name, F-keys excepted (`F1`..`F12`).
pub const NAMED_KEYS: [(NamedKey, &str); 14] = [
    (NamedKey::Enter, "Enter"),
    (NamedKey::Tab, "Tab"),
    (NamedKey::Backspace, "Backspace"),
    (NamedKey::Escape, "Escape"),
    (NamedKey::Delete, "Delete"),
    (NamedKey::Insert, "Insert"),
    (NamedKey::Home, "Home"),
    (NamedKey::End, "End"),
    (NamedKey::PageUp, "PageUp"),
    (NamedKey::PageDown, "PageDown"),
    (NamedKey::ArrowLeft, "ArrowLeft"),
    (NamedKey::ArrowRight, "ArrowRight"),
    (NamedKey::ArrowUp, "ArrowUp"),
    (NamedKey::ArrowDown, "ArrowDown"),
];

impl Key {
    /// The key a DOM key name stands for: a named key, `F1`..`F12`, or one character.
    pub fn parse(name: &str) -> Option<Self> {
        if let Some((key, _)) = NAMED_KEYS.iter().find(|(_, n)| *n == name) {
            return Some(Key::Named(*key));
        }
        if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u8>().ok())
            && (1..=12).contains(&n)
        {
            return Some(Key::Named(NamedKey::F(n)));
        }
        let mut chars = name.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_control() => Some(Key::Char(c)),
            _ => None,
        }
    }

    fn code(self) -> u32 {
        match self {
            Key::Char(c) => u32::from(c),
            Key::Named(NamedKey::F(n)) => 0x11_0000 + u32::from(n),
            Key::Named(k) => {
                let at = NAMED_KEYS.iter().position(|(n, _)| *n == k).unwrap_or(0);
                0x12_0000 + u32::try_from(at).unwrap_or(0)
            }
        }
    }

    fn from_code(code: u32) -> Option<Self> {
        match code {
            0x11_0001..=0x11_000c => Some(Key::Named(NamedKey::F(
                u8::try_from(code - 0x11_0000).ok()?,
            ))),
            0x12_0000..=0x12_00ff => NAMED_KEYS
                .get(usize::try_from(code - 0x12_0000).ok()?)
                .map(|(k, _)| Key::Named(*k)),
            _ => char::from_u32(code).filter(|c| !c.is_control()).map(Key::Char),
        }
    }
}

/// The Windows virtual-key code of `key`, which CEF takes on every platform as
/// `windows_key_code`.
pub fn windows_key_code(key: Key) -> i32 {
    match key {
        Key::Named(k) => match k {
            NamedKey::Backspace => 0x08,
            NamedKey::Tab => 0x09,
            NamedKey::Enter => 0x0D,
            NamedKey::Escape => 0x1B,
            NamedKey::PageUp => 0x21,
            NamedKey::PageDown => 0x22,
            NamedKey::End => 0x23,
            NamedKey::Home => 0x24,
            NamedKey::ArrowLeft => 0x25,
            NamedKey::ArrowUp => 0x26,
            NamedKey::ArrowRight => 0x27,
            NamedKey::ArrowDown => 0x28,
            NamedKey::Insert => 0x2D,
            NamedKey::Delete => 0x2E,
            NamedKey::F(n) => 0x6F + i32::from(n),
        },
        Key::Char(c) => {
            let up = c.to_ascii_uppercase();
            if up.is_ascii_alphanumeric() {
                up as i32
            } else if c == ' ' {
                0x20
            } else {
                0
            }
        }
    }
}

/// The character a key also types, for engines that want a char event after the
/// key-down of Enter, Tab and Backspace (and Escape, which pages read as a char).
pub fn key_char(key: Key) -> Option<u16> {
    match key {
        Key::Named(NamedKey::Enter) => Some(0x0D),
        Key::Named(NamedKey::Tab) => Some(0x09),
        Key::Named(NamedKey::Backspace) => Some(0x08),
        Key::Named(NamedKey::Escape) => Some(0x1B),
        _ => None,
    }
}

/// The macOS virtual key code (`kVK_*`) of `key`, which CEF on macOS reads as
/// `native_key_code`; `None` for a character it has no code for.
pub fn mac_key_code(key: Key) -> Option<i32> {
    Some(match key {
        Key::Named(k) => match k {
            NamedKey::Enter => 0x24,
            NamedKey::Tab => 0x30,
            NamedKey::Backspace => 0x33,
            NamedKey::Escape => 0x35,
            NamedKey::Delete => 0x75,
            NamedKey::Insert => 0x72,
            NamedKey::Home => 0x73,
            NamedKey::End => 0x77,
            NamedKey::PageUp => 0x74,
            NamedKey::PageDown => 0x79,
            NamedKey::ArrowLeft => 0x7B,
            NamedKey::ArrowRight => 0x7C,
            NamedKey::ArrowDown => 0x7D,
            NamedKey::ArrowUp => 0x7E,
            NamedKey::F(n) => *[
                0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F,
            ]
            .get(usize::from(n).checked_sub(1)?)?,
        },
        Key::Char(c) => {
            const LETTERS: [i32; 26] = [
                0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E,
                0x2D, 0x1F, 0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06,
            ];
            let lower = c.to_ascii_lowercase();
            if !lower.is_ascii_lowercase() {
                return None;
            }
            LETTERS[usize::from(lower as u8 - b'a')]
        }
    })
}

/// The character macOS reports for a key-down of `key` (`NSEvent.characters`),
/// which CEF on macOS reads as `character`.
pub fn mac_key_char(key: Key) -> Option<u16> {
    Some(match key {
        Key::Named(k) => match k {
            NamedKey::Enter => 0x0D,
            NamedKey::Tab => 0x09,
            NamedKey::Backspace => 0x7F,
            NamedKey::Escape => 0x1B,
            NamedKey::Delete => 0xF728,
            NamedKey::Insert => 0xF727,
            NamedKey::Home => 0xF729,
            NamedKey::End => 0xF72B,
            NamedKey::PageUp => 0xF72C,
            NamedKey::PageDown => 0xF72D,
            NamedKey::ArrowUp => 0xF700,
            NamedKey::ArrowDown => 0xF701,
            NamedKey::ArrowLeft => 0xF702,
            NamedKey::ArrowRight => 0xF703,
            NamedKey::F(n) => 0xF703 + u16::from(n),
        },
        Key::Char(c) => u16::try_from(u32::from(c)).ok()?,
    })
}

/// One input event, in page pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// The pointer moved to `x`, `y`.
    Move {
        /// Page x.
        x: i32,
        /// Page y.
        y: i32,
        /// Modifier and held-button bits.
        modifiers: u8,
    },
    /// The pointer left the page.
    Leave {
        /// Modifier and held-button bits.
        modifiers: u8,
    },
    /// A button went down or up.
    Button {
        /// Page x.
        x: i32,
        /// Page y.
        y: i32,
        /// Which button.
        button: Button,
        /// Pressed (true) or released.
        down: bool,
        /// Clicks in a row: 1, 2 or 3.
        clicks: u8,
        /// Modifier and held-button bits.
        modifiers: u8,
    },
    /// The wheel turned by `dx`, `dy` pixels (positive `dy` scrolls up).
    Wheel {
        /// Page x.
        x: i32,
        /// Page y.
        y: i32,
        /// Horizontal pixels.
        dx: i32,
        /// Vertical pixels.
        dy: i32,
        /// Modifier and held-button bits.
        modifiers: u8,
    },
    /// A key went down or up.
    Key {
        /// The key.
        key: Key,
        /// Pressed (true) or released.
        down: bool,
        /// Modifier bits.
        modifiers: u8,
    },
    /// Typed text, at most [`MAX_TEXT_BYTES`].
    Text(String),
}

const MOVE: u8 = 1;
const LEAVE: u8 = 2;
const BUTTON: u8 = 3;
const WHEEL: u8 = 4;
const KEY: u8 = 5;
const TEXT: u8 = 6;

fn button_code(b: Button) -> u8 {
    match b {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
    }
}

impl InputEvent {
    /// Appends the event's bytes to `body`.
    pub fn encode(&self, body: &mut Vec<u8>) {
        let xy = |body: &mut Vec<u8>, x: i32, y: i32| {
            body.extend_from_slice(&x.to_le_bytes());
            body.extend_from_slice(&y.to_le_bytes());
        };
        match self {
            InputEvent::Move { x, y, modifiers } => {
                body.push(MOVE);
                xy(body, *x, *y);
                body.push(*modifiers);
            }
            InputEvent::Leave { modifiers } => {
                body.push(LEAVE);
                body.push(*modifiers);
            }
            InputEvent::Button {
                x,
                y,
                button,
                down,
                clicks,
                modifiers,
            } => {
                body.push(BUTTON);
                xy(body, *x, *y);
                body.extend_from_slice(&[button_code(*button), u8::from(*down), *clicks, *modifiers]);
            }
            InputEvent::Wheel {
                x,
                y,
                dx,
                dy,
                modifiers,
            } => {
                body.push(WHEEL);
                xy(body, *x, *y);
                xy(body, *dx, *dy);
                body.push(*modifiers);
            }
            InputEvent::Key {
                key,
                down,
                modifiers,
            } => {
                body.push(KEY);
                body.extend_from_slice(&key.code().to_le_bytes());
                body.extend_from_slice(&[u8::from(*down), *modifiers]);
            }
            InputEvent::Text(text) => {
                body.push(TEXT);
                let mut end = text.len().min(MAX_TEXT_BYTES);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                body.extend_from_slice(&text.as_bytes()[..end]);
            }
        }
    }

    /// Decodes [`InputEvent::encode`]'s bytes; `None` when malformed.
    pub fn decode(body: &[u8]) -> Option<Self> {
        let (&kind, rest) = body.split_first()?;
        let i32_at = |at: usize| -> Option<i32> {
            Some(i32::from_le_bytes(rest.get(at..at + 4)?.try_into().ok()?))
        };
        let button = |c: u8| match c {
            0 => Some(Button::Left),
            1 => Some(Button::Middle),
            2 => Some(Button::Right),
            _ => None,
        };
        Some(match kind {
            MOVE if rest.len() == 9 => InputEvent::Move {
                x: i32_at(0)?,
                y: i32_at(4)?,
                modifiers: rest[8],
            },
            LEAVE if rest.len() == 1 => InputEvent::Leave {
                modifiers: rest[0],
            },
            BUTTON if rest.len() == 12 => InputEvent::Button {
                x: i32_at(0)?,
                y: i32_at(4)?,
                button: button(rest[8])?,
                down: rest[9] != 0,
                clicks: rest[10].clamp(1, 3),
                modifiers: rest[11],
            },
            WHEEL if rest.len() == 17 => InputEvent::Wheel {
                x: i32_at(0)?,
                y: i32_at(4)?,
                dx: i32_at(8)?,
                dy: i32_at(12)?,
                modifiers: rest[16],
            },
            KEY if rest.len() == 6 => InputEvent::Key {
                key: Key::from_code(u32::from_le_bytes(rest.get(..4)?.try_into().ok()?))?,
                down: rest[4] != 0,
                modifiers: rest[5],
            },
            TEXT if rest.len() <= MAX_TEXT_BYTES => {
                InputEvent::Text(std::str::from_utf8(rest).ok()?.to_owned())
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn events_round_trip() {
        for e in [
            InputEvent::Move {
                x: -3,
                y: 1079,
                modifiers: SHIFT | LEFT_HELD,
            },
            InputEvent::Leave { modifiers: 0 },
            InputEvent::Button {
                x: 10,
                y: 20,
                button: Button::Right,
                down: true,
                clicks: 2,
                modifiers: CTRL,
            },
            InputEvent::Wheel {
                x: 1,
                y: 2,
                dx: -40,
                dy: 120,
                modifiers: 0,
            },
            InputEvent::Key {
                key: Key::Named(NamedKey::ArrowLeft),
                down: false,
                modifiers: ALT,
            },
            InputEvent::Key {
                key: Key::Named(NamedKey::F(12)),
                down: true,
                modifiers: 0,
            },
            InputEvent::Key {
                key: Key::Char('a'),
                down: true,
                modifiers: META,
            },
            InputEvent::Text("Merhaba, dünya 👋".into()),
        ] {
            let mut body = Vec::new();
            e.encode(&mut body);
            assert_eq!(InputEvent::decode(&body), Some(e));
        }
    }

    #[test]
    fn malformed_events_and_long_text_are_refused_or_cut() {
        assert_eq!(InputEvent::decode(&[]), None);
        assert_eq!(InputEvent::decode(&[MOVE, 1, 2]), None);
        assert_eq!(InputEvent::decode(&[99]), None);
        assert_eq!(InputEvent::decode(&[BUTTON, 0, 0, 0, 0, 0, 0, 0, 0, 7, 1, 1, 0]), None);
        let long = "é".repeat(MAX_TEXT_BYTES);
        let mut body = Vec::new();
        InputEvent::Text(long).encode(&mut body);
        assert!(body.len() <= MAX_TEXT_BYTES + 1);
        assert!(matches!(InputEvent::decode(&body), Some(InputEvent::Text(t)) if t.chars().all(|c| c == 'é')));
    }

    #[test]
    fn key_names_parse_and_map_to_windows_codes() {
        assert_eq!(Key::parse("Enter"), Some(Key::Named(NamedKey::Enter)));
        assert_eq!(Key::parse("F5"), Some(Key::Named(NamedKey::F(5))));
        assert_eq!(Key::parse("F13"), None);
        assert_eq!(Key::parse("a"), Some(Key::Char('a')));
        assert_eq!(Key::parse("ab"), None);
        assert_eq!(Key::parse("\u{7}"), None);
        assert_eq!(windows_key_code(Key::Char('a')), 0x41);
        assert_eq!(windows_key_code(Key::Named(NamedKey::F(1))), 0x70);
        assert_eq!(windows_key_code(Key::Named(NamedKey::ArrowDown)), 0x28);
        assert_eq!(key_char(Key::Named(NamedKey::Enter)), Some(0x0D));
        assert_eq!(key_char(Key::Named(NamedKey::Home)), None);
        assert_eq!(mac_key_code(Key::Char('A')), Some(0x00));
        assert_eq!(mac_key_code(Key::Char('v')), Some(0x09));
        assert_eq!(mac_key_code(Key::Char('1')), None);
        assert_eq!(mac_key_code(Key::Named(NamedKey::F(12))), Some(0x6F));
        assert_eq!(mac_key_char(Key::Named(NamedKey::F(1))), Some(0xF704));
        assert_eq!(mac_key_char(Key::Named(NamedKey::ArrowLeft)), Some(0xF702));
    }
}
