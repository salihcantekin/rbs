//! Shortcuts (ADR-0060): key chords bound to command registry ids, in the app or
//! globally, the defaults, the user's changes as a document, and the keymap in force
//! with its conflicts.
//!
//! Spec: `docs/specs/shortcuts.md`.
//!
//! Everything here is toolkit-free: the shell maps its key events to [`Chord`]s and
//! registers global chords with the OS. The platform is a parameter, never a `cfg`, so
//! the rules of every platform are tested everywhere.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::command::{CommandId, ENTRIES};

/// Longest chord text in bytes.
pub const MAX_CHORD_LEN: usize = 64;
/// Most bindings one command may have, over both scopes.
pub const MAX_PER_COMMAND: usize = 4;
/// Most `binding` plus `removed` entries in a shortcuts document.
pub const MAX_ENTRIES: usize = 512;

/// The platform whose rules apply (reserved chords, labels).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Platform {
    /// macOS.
    MacOs,
    /// Windows.
    Windows,
    /// Linux and other desktops.
    Linux,
}

impl Platform {
    /// The platform this build runs on.
    pub fn current() -> Platform {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Linux
        }
    }

    /// Global shortcuts exist on this platform (ADR-0060 decision 7).
    pub fn has_global(self) -> bool {
        matches!(self, Platform::MacOs | Platform::Windows)
    }
}

/// Modifier keys. `ctrl` is the primary modifier (Command on macOS), `meta` is Control
/// on macOS and the Windows key on Windows, `alt` is Option on macOS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Mods {
    /// Ctrl, Command on macOS.
    pub ctrl: bool,
    /// Alt, Option on macOS.
    pub alt: bool,
    /// Shift.
    pub shift: bool,
    /// Control on macOS, the Windows key on Windows.
    pub meta: bool,
}

impl Mods {
    /// No modifier.
    pub const NONE: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
        meta: false,
    };
    /// Ctrl only.
    pub const CTRL: Mods = Mods {
        ctrl: true,
        alt: false,
        shift: false,
        meta: false,
    };
}

/// Keys with a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Named {
    /// Space bar.
    Space,
    /// Enter or Return.
    Enter,
    /// Escape.
    Escape,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Delete (forward).
    Delete,
    /// Insert.
    Insert,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// `-`.
    Minus,
    /// `=`.
    Equal,
    /// `[`.
    BracketLeft,
    /// `]`.
    BracketRight,
    /// `\`.
    Backslash,
    /// `;`.
    Semicolon,
    /// `'`.
    Quote,
    /// `,`.
    Comma,
    /// `.`.
    Period,
    /// `/`.
    Slash,
    /// `` ` ``.
    Backquote,
}

impl Named {
    /// Every named key.
    pub const ALL: [Named; 26] = [
        Named::Space,
        Named::Enter,
        Named::Escape,
        Named::Tab,
        Named::Backspace,
        Named::Delete,
        Named::Insert,
        Named::Home,
        Named::End,
        Named::PageUp,
        Named::PageDown,
        Named::Up,
        Named::Down,
        Named::Left,
        Named::Right,
        Named::Minus,
        Named::Equal,
        Named::BracketLeft,
        Named::BracketRight,
        Named::Backslash,
        Named::Semicolon,
        Named::Quote,
        Named::Comma,
        Named::Period,
        Named::Slash,
        Named::Backquote,
    ];

    /// Its name in chord text.
    pub fn name(self) -> &'static str {
        match self {
            Named::Space => "Space",
            Named::Enter => "Enter",
            Named::Escape => "Escape",
            Named::Tab => "Tab",
            Named::Backspace => "Backspace",
            Named::Delete => "Delete",
            Named::Insert => "Insert",
            Named::Home => "Home",
            Named::End => "End",
            Named::PageUp => "PageUp",
            Named::PageDown => "PageDown",
            Named::Up => "Up",
            Named::Down => "Down",
            Named::Left => "Left",
            Named::Right => "Right",
            Named::Minus => "Minus",
            Named::Equal => "Equal",
            Named::BracketLeft => "BracketLeft",
            Named::BracketRight => "BracketRight",
            Named::Backslash => "Backslash",
            Named::Semicolon => "Semicolon",
            Named::Quote => "Quote",
            Named::Comma => "Comma",
            Named::Period => "Period",
            Named::Slash => "Slash",
            Named::Backquote => "Backquote",
        }
    }

    /// The character the key types on a US layout without Shift, for punctuation.
    pub fn punctuation(self) -> Option<char> {
        Some(match self {
            Named::Minus => '-',
            Named::Equal => '=',
            Named::BracketLeft => '[',
            Named::BracketRight => ']',
            Named::Backslash => '\\',
            Named::Semicolon => ';',
            Named::Quote => '\'',
            Named::Comma => ',',
            Named::Period => '.',
            Named::Slash => '/',
            Named::Backquote => '`',
            _ => return None,
        })
    }

    /// The punctuation key that types `c` without Shift.
    pub fn from_punctuation(c: char) -> Option<Named> {
        Named::ALL.into_iter().find(|n| n.punctuation() == Some(c))
    }

    fn label(self, platform: Platform) -> &'static str {
        let mac = platform == Platform::MacOs;
        match self {
            Named::Space => "Space",
            Named::Enter if mac => "↩",
            Named::Enter => "Enter",
            Named::Escape if mac => "⎋",
            Named::Escape => "Esc",
            Named::Tab if mac => "⇥",
            Named::Tab => "Tab",
            Named::Backspace if mac => "⌫",
            Named::Backspace => "Backspace",
            Named::Delete if mac => "⌦",
            Named::Delete => "Delete",
            Named::Insert => "Insert",
            Named::Home => "Home",
            Named::End => "End",
            Named::PageUp => "Page Up",
            Named::PageDown => "Page Down",
            Named::Up => "↑",
            Named::Down => "↓",
            Named::Left => "←",
            Named::Right => "→",
            Named::Minus => "-",
            Named::Equal => "=",
            Named::BracketLeft => "[",
            Named::BracketRight => "]",
            Named::Backslash => "\\",
            Named::Semicolon => ";",
            Named::Quote => "'",
            Named::Comma => ",",
            Named::Period => ".",
            Named::Slash => "/",
            Named::Backquote => "`",
        }
    }
}

/// The key of a chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Key {
    /// `A`..=`Z` (the ASCII capital).
    Letter(u8),
    /// `0`..=`9` (the digit's value).
    Digit(u8),
    /// `F1`..=`F24` (the number).
    F(u8),
    /// A named key.
    Named(Named),
}

impl Key {
    /// A letter key; `None` outside ASCII letters.
    pub fn letter(c: char) -> Option<Key> {
        c.is_ascii_alphabetic()
            .then(|| Key::Letter(c.to_ascii_uppercase() as u8))
    }

    /// Digit or punctuation: what Shift does with them depends on the layout.
    fn layout_dependent(self) -> bool {
        match self {
            Key::Digit(_) => true,
            Key::Named(n) => n.punctuation().is_some(),
            _ => false,
        }
    }

    fn parse(s: &str) -> Option<Key> {
        let mut chars = s.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if c.is_ascii_alphabetic() {
                return Key::letter(c);
            }
            if let Some(d) = c.to_digit(10) {
                return Some(Key::Digit(d as u8));
            }
        }
        if let Some(n) = s
            .strip_prefix('F')
            .or_else(|| s.strip_prefix('f'))
            .filter(|n| {
                !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && !n.starts_with('0')
            })
            .and_then(|n| n.parse::<u8>().ok())
        {
            return (1..=24).contains(&n).then_some(Key::F(n));
        }
        Named::ALL
            .into_iter()
            .find(|n| n.name().eq_ignore_ascii_case(s))
            .map(Key::Named)
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Letter(c) => write!(f, "{}", char::from(*c)),
            Key::Digit(d) => write!(f, "{d}"),
            Key::F(n) => write!(f, "F{n}"),
            Key::Named(n) => f.write_str(n.name()),
        }
    }
}

/// Why chord text is not a chord.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChordError(pub String);

impl fmt::Display for ChordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ChordError {}

/// A key with modifiers. Its text (`Display`) is canonical: `Ctrl`, `Alt`, `Shift`,
/// `Meta` in that order, then the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Chord {
    /// The modifiers.
    pub mods: Mods,
    /// The key.
    pub key: Key,
}

impl Chord {
    /// A chord, checked: `Shift` with a digit or punctuation key is refused.
    pub fn new(mods: Mods, key: Key) -> Result<Chord, ChordError> {
        if mods.shift && key.layout_dependent() {
            return Err(ChordError(
                "Shift with this key types something else on other keyboard layouts; use another key"
                    .into(),
            ));
        }
        Ok(Chord { mods, key })
    }

    /// The chord of a key press that typed `c` (the toolkit's text for the key) with
    /// `mods` (ADR-0060 decision 2). Letters are ASCII only and upper-cased; a digit or
    /// known punctuation character drops Shift, so AZERTY's Shift+`&` (which types `1`)
    /// reads as the digit key. Anything else is not a chord, with the reason.
    pub fn from_typed(c: char, mods: Mods) -> Result<Chord, &'static str> {
        if let Some(k) = Key::letter(c) {
            return Ok(Chord { mods, key: k });
        }
        let no_shift = Mods {
            shift: false,
            ..mods
        };
        if let Some(d) = c.to_digit(10).filter(|_| c.is_ascii_digit()) {
            return Ok(Chord {
                mods: no_shift,
                key: Key::Digit(d as u8),
            });
        }
        if let Some(n) = Named::from_punctuation(c) {
            return Ok(Chord {
                mods: no_shift,
                key: Key::Named(n),
            });
        }
        if c == ' ' {
            return Ok(Chord {
                mods,
                key: Key::Named(Named::Space),
            });
        }
        Err("This key cannot be used in a shortcut; use A to Z, 0 to 9, F1 to F24 or a named key")
    }

    /// Parses chord text: modifiers in any order and case, then one key.
    pub fn parse(s: &str) -> Result<Chord, ChordError> {
        let err = |m: &str| Err(ChordError(format!("shortcut {s:?}: {m}")));
        if s.is_empty() {
            return err("empty");
        }
        if s.len() > MAX_CHORD_LEN {
            return err("longer than 64 bytes");
        }
        let mut mods = Mods::NONE;
        let mut key = None;
        for part in s.split('+') {
            let part = part.trim();
            if part.is_empty() {
                return err("an empty part between `+`");
            }
            let m = match part.to_ascii_lowercase().as_str() {
                "ctrl" => Some(&mut mods.ctrl),
                "alt" => Some(&mut mods.alt),
                "shift" => Some(&mut mods.shift),
                "meta" => Some(&mut mods.meta),
                _ => None,
            };
            if let Some(m) = m {
                if key.is_some() {
                    return err("modifiers come before the key");
                }
                if *m {
                    return err("a modifier appears twice");
                }
                *m = true;
                continue;
            }
            if key.is_some() {
                return err("more than one key");
            }
            match Key::parse(part) {
                Some(k) => key = Some(k),
                None => return err(&format!("unknown key {part:?}")),
            }
        }
        let Some(key) = key else {
            return err("no key, only modifiers");
        };
        Chord::new(mods, key).or_else(|e| err(&e.0))
    }

    /// Text to show: symbols in Apple's order on macOS (⌃⌥⇧⌘), else the canonical text
    /// with `Win` for `Meta` on Windows.
    pub fn label(&self, platform: Platform) -> String {
        let key = match self.key {
            Key::Named(n) => n.label(platform).to_owned(),
            k => k.to_string(),
        };
        let m = self.mods;
        if platform == Platform::MacOs {
            let mut s = String::new();
            for (on, sym) in [(m.meta, '⌃'), (m.alt, '⌥'), (m.shift, '⇧'), (m.ctrl, '⌘')] {
                if on {
                    s.push(sym);
                }
            }
            s.push_str(&key);
            return s;
        }
        let meta = if platform == Platform::Windows {
            "Win"
        } else {
            "Super"
        };
        let mut parts: Vec<&str> = Vec::new();
        for (on, name) in [
            (m.ctrl, "Ctrl"),
            (m.alt, "Alt"),
            (m.shift, "Shift"),
            (m.meta, meta),
        ] {
            if on {
                parts.push(name);
            }
        }
        parts.push(&key);
        parts.join("+")
    }

    /// Why this chord can never be bound on `platform` (ADR-0060 decision 5), if so.
    pub fn reserved(&self, platform: Platform) -> Option<&'static str> {
        let m = self.mods;
        let only = |ctrl, alt, shift, meta| {
            m == Mods {
                ctrl,
                alt,
                shift,
                meta,
            }
        };
        let named = |n| self.key == Key::Named(n);
        let letter = |c: u8| self.key == Key::Letter(c);
        let arrow = matches!(
            self.key,
            Key::Named(Named::Left | Named::Right | Named::Up | Named::Down)
        );
        match platform {
            Platform::MacOs => {
                let cmd = only(true, false, false, false);
                let reserved = (cmd
                    && (letter(b'Q')
                        || letter(b'W')
                        || letter(b'H')
                        || letter(b'M')
                        || named(Named::Comma)
                        || named(Named::Tab)
                        || named(Named::Space)
                        || named(Named::Backquote)))
                    || (only(true, true, false, false) && named(Named::Escape))
                    || (only(true, false, false, true) && letter(b'Q'))
                    || (only(false, false, false, true) && (named(Named::Space) || arrow));
                reserved.then_some("macOS uses this shortcut")
            }
            Platform::Windows | Platform::Linux => {
                let win = platform == Platform::Windows;
                if m.meta {
                    return Some(if win {
                        "Windows keeps shortcuts with the Windows key"
                    } else {
                        "The desktop keeps shortcuts with the Super key"
                    });
                }
                let alt = only(false, true, false, false);
                let reserved = (alt
                    && (self.key == Key::F(4) || named(Named::Tab) || named(Named::Space)))
                    || (win
                        && ((only(true, false, false, false) && named(Named::Escape))
                            || (only(true, false, true, false) && named(Named::Escape))
                            || ((only(false, false, false, false)
                                || only(false, false, true, false))
                                && self.key == Key::F(10))));
                reserved.then_some(if win {
                    "Windows uses this shortcut"
                } else {
                    "The desktop uses this shortcut"
                })
            }
        }
    }

    /// Why this chord cannot be global on `platform`: it would take typing away from
    /// every app (ADR-0060 decision 5). `None` when it can.
    pub fn global_problem(&self, platform: Platform) -> Option<&'static str> {
        let m = self.mods;
        let bare_f =
            |max| m == Mods::NONE && matches!(self.key, Key::F(n) if (13..=max).contains(&n));
        match platform {
            Platform::MacOs => {
                if matches!(self.key, Key::F(21..=24)) {
                    return Some("macOS has no global shortcuts for F21 to F24");
                }
                (!(m.ctrl || m.meta || bare_f(20)))
                    .then_some("A global shortcut needs Command or Control (or F13 to F20 alone)")
            }
            Platform::Windows | Platform::Linux => (!((m.ctrl && !m.alt)
                || (m.alt && m.shift && !m.ctrl)
                || bare_f(24)))
            .then_some(
                "A global shortcut needs Ctrl without Alt, or Alt with Shift (or F13 to F24 alone); Ctrl+Alt types characters on many layouts",
            ),
        }
    }

    /// Why this chord cannot work in app scope on `platform`, if so: on macOS, Option
    /// without Command or Control changes the character the key types.
    pub fn app_problem(&self, platform: Platform) -> Option<&'static str> {
        let m = self.mods;
        let typed = matches!(self.key, Key::Letter(_) | Key::Digit(_))
            || matches!(self.key, Key::Named(n) if n.punctuation().is_some() || n == Named::Space);
        (platform == Platform::MacOs && m.alt && !m.ctrl && !m.meta && typed).then_some(
            "Option with this key types a character; add Command or Control, or make it global",
        )
    }

    /// A global chord other apps commonly use: allowed, with a warning.
    pub fn common_in_other_apps(&self) -> bool {
        self.mods == Mods::CTRL
            && matches!(self.key, Key::Letter(c) if b"CVXZASFNOPTW".contains(&c))
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.mods;
        for (on, name) in [
            (m.ctrl, "Ctrl+"),
            (m.alt, "Alt+"),
            (m.shift, "Shift+"),
            (m.meta, "Meta+"),
        ] {
            if on {
                f.write_str(name)?;
            }
        }
        write!(f, "{}", self.key)
    }
}

/// Where a binding works.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// While an RBS window that runs commands has the focus.
    #[default]
    App,
    /// Whatever app has the focus (macOS and Windows).
    Global,
}

/// Default bindings of ids with parameters, and second defaults of plain ids, as
/// `(command id, chord)`. Plain ids' first defaults are the registry's `shortcut`.
pub const DEFAULT_SHORTCUTS: &[(&str, &str)] = &[
    ("edit.redo", "Ctrl+Y"),
    ("scene.position?n=1", "Ctrl+1"),
    ("scene.position?n=2", "Ctrl+2"),
    ("scene.position?n=3", "Ctrl+3"),
    ("scene.position?n=4", "Ctrl+4"),
    ("scene.position?n=5", "Ctrl+5"),
    ("scene.position?n=6", "Ctrl+6"),
    ("scene.position?n=7", "Ctrl+7"),
    ("scene.position?n=8", "Ctrl+8"),
    ("scene.position?n=9", "Ctrl+9"),
];

/// Every default binding, app scope, in a stable order: registry entries first, then
/// [`DEFAULT_SHORTCUTS`]. Ids whose entry is not in the registry are left out, so a
/// default never names a command this build does not have.
pub fn defaults() -> Vec<(String, Chord)> {
    let mut out = Vec::new();
    for e in ENTRIES {
        if let Some(text) = e.shortcut {
            if let Ok(c) = Chord::parse(text) {
                out.push((e.id.to_owned(), c));
            }
        }
    }
    for (id, text) in DEFAULT_SHORTCUTS {
        let Ok(id) = CommandId::parse(id) else {
            continue;
        };
        if !matches!(id.resolve(), Ok(Some(_))) {
            continue;
        }
        if let Ok(c) = Chord::parse(text) {
            out.push((id.to_string(), c));
        }
    }
    out
}

/// A binding the user added (`[[binding]]`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserBinding {
    /// The command id, canonical.
    pub command: String,
    /// The chord's canonical text.
    pub keys: String,
    /// Where it works.
    #[serde(default)]
    pub scope: Scope,
}

/// A default binding the user removed (`[[removed]]`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Removed {
    /// The command id, canonical.
    pub command: String,
    /// The chord's canonical text.
    pub keys: String,
}

/// The user's changes to the defaults: the body of `shortcuts.toml`
/// (`kind = "shortcuts"`, format 1).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShortcutsDoc {
    /// Bindings the user added, in order.
    #[serde(default, rename = "binding", skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<UserBinding>,
    /// Default bindings the user removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<Removed>,
}

/// Canonical command id text, or the text as it is when it is not an id.
fn canonical_id(text: &str) -> String {
    CommandId::parse(text)
        .map(|id| id.to_string())
        .unwrap_or_else(|_| text.to_owned())
}

impl ShortcutsDoc {
    /// Checks what a file may hold: limits, command ids and chords. Called when the
    /// document is read; a file that fails is not used (spec).
    pub fn validate(&self) -> Result<(), String> {
        if self.bindings.len() + self.removed.len() > MAX_ENTRIES {
            return Err(format!("more than {MAX_ENTRIES} entries"));
        }
        for (command, keys) in self
            .bindings
            .iter()
            .map(|b| (&b.command, &b.keys))
            .chain(self.removed.iter().map(|r| (&r.command, &r.keys)))
        {
            CommandId::parse(command).map_err(|e| e.to_string())?;
            Chord::parse(keys).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Adds a binding. Adding a default the user had removed takes the removal back
    /// instead, so the document stays minimal.
    pub fn add(&mut self, command: &str, chord: Chord, scope: Scope) {
        let command = canonical_id(command);
        let keys = chord.to_string();
        if scope == Scope::App && defaults().iter().any(|(c, k)| *c == command && *k == chord) {
            let before = self.removed.len();
            self.removed
                .retain(|r| !(canonical_id(&r.command) == command && same_keys(&r.keys, &chord)));
            if self.removed.len() != before {
                return;
            }
        }
        if self.bindings.iter().any(|b| {
            canonical_id(&b.command) == command && same_keys(&b.keys, &chord) && b.scope == scope
        }) {
            return;
        }
        self.bindings.push(UserBinding {
            command,
            keys,
            scope,
        });
    }

    /// Removes the binding of `command` on `chord`: the user's, or the default (recorded
    /// in `removed`).
    pub fn remove(&mut self, command: &str, chord: Chord) {
        let command = canonical_id(command);
        let before = self.bindings.len();
        self.bindings
            .retain(|b| !(canonical_id(&b.command) == command && same_keys(&b.keys, &chord)));
        if self.bindings.len() != before {
            return;
        }
        let is_default = defaults().iter().any(|(c, k)| *c == command && *k == chord);
        let already = self
            .removed
            .iter()
            .any(|r| canonical_id(&r.command) == command && same_keys(&r.keys, &chord));
        if is_default && !already {
            self.removed.push(Removed {
                command,
                keys: chord.to_string(),
            });
        }
    }

    /// Removes whatever binding is in force on `chord` in `keymap`, then adds this one
    /// (Replace in the editor).
    pub fn replace(&mut self, keymap: &Keymap, command: &str, chord: Chord, scope: Scope) {
        if let Some(holder) = keymap.holder(&chord) {
            let holder = holder.command.clone();
            self.remove(&holder, chord);
        }
        self.add(command, chord, scope);
    }

    /// Back to the defaults for one command.
    pub fn reset(&mut self, command: &str) {
        let command = canonical_id(command);
        self.bindings
            .retain(|b| canonical_id(&b.command) != command);
        self.removed.retain(|r| canonical_id(&r.command) != command);
    }

    /// Back to the defaults for every command.
    pub fn reset_all(&mut self) {
        self.bindings.clear();
        self.removed.clear();
    }

    /// The user changed bindings of `command`.
    pub fn changed(&self, command: &str) -> bool {
        let command = canonical_id(command);
        self.bindings
            .iter()
            .any(|b| canonical_id(&b.command) == command)
            || self
                .removed
                .iter()
                .any(|r| canonical_id(&r.command) == command)
    }
}

fn same_keys(text: &str, chord: &Chord) -> bool {
    Chord::parse(text).is_ok_and(|c| c == *chord)
}

/// A binding in force.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Active {
    /// The command id, canonical.
    pub command: String,
    /// The chord.
    pub chord: Chord,
    /// Where it works.
    pub scope: Scope,
    /// It is a default (not in the user's file).
    pub default: bool,
}

/// Why a binding is not in force.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProblemKind {
    /// An earlier binding holds the chord; it names that binding's command.
    Conflict(String),
    /// The platform keeps the chord; the reason.
    Reserved(&'static str),
    /// Unsafe or impossible in its scope on this platform; the reason.
    Scope(&'static str),
    /// Global shortcuts do not exist on this platform.
    NoGlobal,
    /// The command has more than [`MAX_PER_COMMAND`] bindings.
    TooMany,
}

impl ProblemKind {
    /// The text the Settings page shows; `title` gives a command's title.
    pub fn text(&self, title: &dyn Fn(&str) -> String) -> String {
        match self {
            ProblemKind::Conflict(c) => {
                format!("Also bound to {}; this one does nothing.", title(c))
            }
            ProblemKind::Reserved(why) => format!("{why}."),
            ProblemKind::Scope(why) => format!("{why}."),
            ProblemKind::NoGlobal => {
                "Global shortcuts are not available on this system yet.".to_owned()
            }
            ProblemKind::TooMany => format!("A command has at most {MAX_PER_COMMAND} shortcuts."),
        }
    }
}

/// A binding from the user's file that is not in force.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// The command id, canonical.
    pub command: String,
    /// The chord.
    pub chord: Chord,
    /// Where it was meant to work.
    pub scope: Scope,
    /// Why it is not in force.
    pub kind: ProblemKind,
}

/// What recording a chord would do (the editor's live check).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// Nothing holds it.
    Free,
    /// A binding in force holds it.
    Taken {
        /// Its command.
        command: String,
        /// Its scope.
        scope: Scope,
    },
    /// It cannot be bound in this scope; the reason.
    Refused(String),
    /// It can be bound, but the user should know this.
    Warn(String),
}

fn scope_problem(chord: &Chord, scope: Scope, platform: Platform) -> Option<&'static str> {
    match scope {
        Scope::App => chord.app_problem(platform),
        Scope::Global => chord.global_problem(platform),
    }
}

/// The bindings in force: defaults with the user's changes (spec: Keymap).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Keymap {
    platform: Option<Platform>,
    bindings: Vec<Active>,
    problems: Vec<Problem>,
    by_chord: HashMap<Chord, usize>,
}

impl Keymap {
    /// The keymap of `doc` on `platform`.
    pub fn resolve(doc: &ShortcutsDoc, platform: Platform) -> Keymap {
        let removed: Vec<(String, Chord)> = doc
            .removed
            .iter()
            .filter_map(|r| Some((canonical_id(&r.command), Chord::parse(&r.keys).ok()?)))
            .collect();
        let mut user: Vec<(String, Chord, Scope)> = Vec::new();
        for b in &doc.bindings {
            if let Ok(c) = Chord::parse(&b.keys) {
                user.push((canonical_id(&b.command), c, b.scope));
            }
        }
        let mut k = Keymap {
            platform: Some(platform),
            ..Keymap::default()
        };
        let mut problems = Vec::new();
        // User bindings first, in file order: they win over defaults on their chord.
        for (command, chord, scope) in user {
            let kind = if let Some(why) = chord.reserved(platform) {
                Some(ProblemKind::Reserved(why))
            } else if scope == Scope::Global && !platform.has_global() {
                Some(ProblemKind::NoGlobal)
            } else if let Some(why) = scope_problem(&chord, scope, platform) {
                Some(ProblemKind::Scope(why))
            } else if let Some(&i) = k.by_chord.get(&chord) {
                Some(ProblemKind::Conflict(k.bindings[i].command.clone()))
            } else if k.count(&command) >= MAX_PER_COMMAND {
                Some(ProblemKind::TooMany)
            } else {
                None
            };
            match kind {
                Some(kind) => problems.push(Problem {
                    command,
                    chord,
                    scope,
                    kind,
                }),
                None => k.push(Active {
                    command,
                    chord,
                    scope,
                    default: false,
                }),
            }
        }
        // Defaults on the chords no user binding took, minus removed ones.
        let mut with_defaults = Vec::new();
        for (command, chord) in defaults() {
            if removed.iter().any(|(c, ch)| *c == command && *ch == chord)
                || k.by_chord.contains_key(&chord)
                || chord.reserved(platform).is_some()
                || chord.app_problem(platform).is_some()
                || k.count(&command)
                    + with_defaults
                        .iter()
                        .filter(|b: &&Active| b.command == command)
                        .count()
                    >= MAX_PER_COMMAND
            {
                continue;
            }
            with_defaults.push(Active {
                command,
                chord,
                scope: Scope::App,
                default: true,
            });
            // Only marks the chord as taken; the table is rebuilt below.
            k.by_chord.insert(chord, usize::MAX);
        }
        // Stable order: defaults first (registry order), then the user's.
        let user_bindings = std::mem::take(&mut k.bindings);
        k.bindings = with_defaults;
        k.bindings.extend(user_bindings);
        k.by_chord = k
            .bindings
            .iter()
            .enumerate()
            .map(|(i, b)| (b.chord, i))
            .collect();
        k.problems = problems;
        k
    }

    fn push(&mut self, a: Active) {
        self.by_chord.insert(a.chord, self.bindings.len());
        self.bindings.push(a);
    }

    fn count(&self, command: &str) -> usize {
        self.bindings
            .iter()
            .filter(|b| b.command == command)
            .count()
    }

    /// The bindings in force: defaults in registry order, then the user's in file order.
    pub fn bindings(&self) -> &[Active] {
        &self.bindings
    }

    /// The user's bindings that are not in force, with why.
    pub fn problems(&self) -> &[Problem] {
        &self.problems
    }

    /// The binding in force on `chord`.
    pub fn holder(&self, chord: &Chord) -> Option<&Active> {
        self.by_chord.get(chord).and_then(|&i| self.bindings.get(i))
    }

    /// The command an app-scope key press on `chord` runs. A chord bound globally is
    /// never run in app scope: the OS delivers it once.
    pub fn app(&self, chord: &Chord) -> Option<&str> {
        self.holder(chord)
            .filter(|b| b.scope == Scope::App)
            .map(|b| b.command.as_str())
    }

    /// The global bindings in force.
    pub fn global(&self) -> impl Iterator<Item = &Active> {
        self.bindings.iter().filter(|b| b.scope == Scope::Global)
    }

    /// Bindings in force of `command`.
    pub fn of<'a>(&'a self, command: &'a str) -> impl Iterator<Item = &'a Active> + 'a {
        self.bindings.iter().filter(move |b| b.command == command)
    }

    /// What recording `chord` for `scope` would do.
    pub fn check(&self, chord: &Chord, scope: Scope) -> Check {
        let platform = self.platform.unwrap_or(Platform::current());
        if let Some(why) = chord.reserved(platform) {
            return Check::Refused(format!("{why}."));
        }
        if scope == Scope::Global && !platform.has_global() {
            return Check::Refused(ProblemKind::NoGlobal.text(&|c| c.to_owned()));
        }
        if let Some(why) = scope_problem(chord, scope, platform) {
            return Check::Refused(format!("{why}."));
        }
        match self.holder(chord) {
            Some(b) => Check::Taken {
                command: b.command.clone(),
                scope: b.scope,
            },
            None if scope == Scope::Global && chord.common_in_other_apps() => Check::Warn(
                "Other apps use this shortcut; while it is global they will not get it.".into(),
            ),
            None => Check::Free,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    const PLATFORMS: [Platform; 3] = [Platform::MacOs, Platform::Windows, Platform::Linux];

    fn c(s: &str) -> Chord {
        Chord::parse(s).unwrap()
    }

    fn every_key() -> Vec<Key> {
        let mut keys: Vec<Key> = (b'A'..=b'Z').map(Key::Letter).collect();
        keys.extend((0..=9).map(Key::Digit));
        keys.extend((1..=24).map(Key::F));
        keys.extend(Named::ALL.map(Key::Named));
        keys
    }

    #[test]
    fn every_chord_round_trips() {
        for key in every_key() {
            for bits in 0..16u8 {
                let mods = Mods {
                    ctrl: bits & 1 != 0,
                    alt: bits & 2 != 0,
                    shift: bits & 4 != 0,
                    meta: bits & 8 != 0,
                };
                match Chord::new(mods, key) {
                    Ok(chord) => {
                        let text = chord.to_string();
                        assert_eq!(Chord::parse(&text), Ok(chord), "{text}");
                        assert!(text.len() <= MAX_CHORD_LEN);
                        for p in PLATFORMS {
                            assert!(!chord.label(p).is_empty());
                        }
                    }
                    Err(_) => assert!(mods.shift && key.layout_dependent()),
                }
            }
        }
    }

    #[test]
    fn parsing_is_lenient_in_order_and_case_and_canonical_out() {
        assert_eq!(c("shift+ctrl+z").to_string(), "Ctrl+Shift+Z");
        assert_eq!(c("Meta+Alt+f13").to_string(), "Alt+Meta+F13");
        assert_eq!(c("ctrl+pageup").to_string(), "Ctrl+PageUp");
        assert_eq!(c("Ctrl + 1").to_string(), "Ctrl+1");
        for bad in [
            "",
            "Ctrl",
            "Ctrl+",
            "Ctrl+Ctrl+A",
            "A+B",
            "A+Ctrl",
            "Ctrl+F0",
            "Ctrl+F25",
            "Ctrl+F01",
            "Ctrl+Ş",
            "Ctrl+Shift+1",
            "Shift+Minus",
            "Hyper+A",
        ] {
            assert!(Chord::parse(bad).is_err(), "{bad}");
        }
        assert!(Chord::parse(&format!("Ctrl+{}", "A".repeat(70))).is_err());
    }

    #[test]
    fn typed_keys_make_chords() {
        let shift_ctrl = Mods {
            ctrl: true,
            shift: true,
            ..Mods::NONE
        };
        assert_eq!(Chord::from_typed('a', Mods::CTRL), Ok(c("Ctrl+A")));
        assert_eq!(Chord::from_typed('A', shift_ctrl), Ok(c("Ctrl+Shift+A")));
        // AZERTY: Shift on the top row types the digit.
        assert_eq!(Chord::from_typed('1', shift_ctrl), Ok(c("Ctrl+1")));
        assert_eq!(Chord::from_typed('/', shift_ctrl), Ok(c("Ctrl+Slash")));
        assert_eq!(Chord::from_typed(' ', Mods::CTRL), Ok(c("Ctrl+Space")));
        // ASCII only: Turkish dotless i is not I.
        for bad in ['ı', 'İ', 'ş', '!', 'å', '¡'] {
            assert!(Chord::from_typed(bad, Mods::CTRL).is_err(), "{bad}");
        }
    }

    #[test]
    fn labels_follow_the_platform() {
        assert_eq!(c("Ctrl+Shift+Z").label(Platform::MacOs), "⇧⌘Z");
        assert_eq!(c("Ctrl+Alt+Shift+Meta+Up").label(Platform::MacOs), "⌃⌥⇧⌘↑");
        assert_eq!(c("Ctrl+Shift+Z").label(Platform::Windows), "Ctrl+Shift+Z");
        assert_eq!(c("Meta+Escape").label(Platform::Windows), "Win+Esc");
        assert_eq!(c("Alt+PageDown").label(Platform::Linux), "Alt+Page Down");
    }

    #[test]
    fn reserved_and_global_rules() {
        assert!(c("Ctrl+Q").reserved(Platform::MacOs).is_some());
        assert!(c("Ctrl+Q").reserved(Platform::Windows).is_none());
        assert!(c("Ctrl+Alt+Escape").reserved(Platform::MacOs).is_some());
        assert!(c("Alt+F4").reserved(Platform::Windows).is_some());
        assert!(c("Meta+D").reserved(Platform::Windows).is_some());
        assert!(c("Meta+D").reserved(Platform::MacOs).is_none());
        assert!(c("Ctrl+Escape").reserved(Platform::Windows).is_some());
        assert!(c("Ctrl+Shift+Q").reserved(Platform::MacOs).is_none());
        assert!(c("Ctrl+Shift+Escape").reserved(Platform::Windows).is_some());
        assert!(c("F10").reserved(Platform::Windows).is_some());
        assert!(c("Alt+Space").reserved(Platform::Windows).is_some());
        assert!(c("Meta+Space").reserved(Platform::MacOs).is_some());
        assert!(c("Meta+Left").reserved(Platform::MacOs).is_some());
        assert!(c("Ctrl+Meta+Q").reserved(Platform::MacOs).is_some());
        for p in PLATFORMS {
            assert!(c("Ctrl+1").global_problem(p).is_none(), "{p:?}");
            assert!(c("F13").global_problem(p).is_none(), "{p:?}");
            assert!(c("F12").global_problem(p).is_some(), "{p:?}");
            assert!(c("Shift+A").global_problem(p).is_some(), "{p:?}");
        }
        // Option-only types characters on macOS; Ctrl+Alt is AltGr on Windows.
        assert!(c("Alt+Q").global_problem(Platform::MacOs).is_some());
        assert!(c("Meta+Alt+Q").global_problem(Platform::MacOs).is_none());
        assert!(c("Ctrl+Alt+7").global_problem(Platform::Windows).is_some());
        assert!(c("Ctrl+Alt+7").global_problem(Platform::MacOs).is_none());
        assert!(c("Alt+Shift+K").global_problem(Platform::Windows).is_none());
        assert!(c("Alt+K").global_problem(Platform::Windows).is_some());
        assert!(c("F21").global_problem(Platform::MacOs).is_some());
        assert!(c("F21").global_problem(Platform::Windows).is_none());
        assert!(c("Alt+A").app_problem(Platform::MacOs).is_some());
        assert!(c("Alt+F5").app_problem(Platform::MacOs).is_none());
        assert!(c("Ctrl+Alt+A").app_problem(Platform::MacOs).is_none());
        assert!(c("Alt+A").app_problem(Platform::Windows).is_none());
        assert!(c("Ctrl+S").common_in_other_apps());
    }

    #[test]
    fn defaults_are_sane() {
        let d = defaults();
        assert!(
            d.iter()
                .any(|(id, ch)| id == "edit.undo" && *ch == c("Ctrl+Z"))
        );
        let mut chords: Vec<Chord> = d.iter().map(|(_, ch)| *ch).collect();
        chords.sort();
        let n = chords.len();
        chords.dedup();
        assert_eq!(chords.len(), n, "two defaults share a chord");
        for (id, ch) in &d {
            for p in PLATFORMS {
                assert!(ch.reserved(p).is_none(), "{id} {ch} reserved on {p:?}");
            }
            assert!(
                !(ch.mods.ctrl && ch.mods.alt),
                "{id}: Ctrl+Alt clashes with AltGr"
            );
            assert!(
                matches!(CommandId::parse(id).unwrap().resolve(), Ok(Some(_))),
                "{id}"
            );
        }
        for (id, text) in DEFAULT_SHORTCUTS {
            CommandId::parse(id).unwrap();
            Chord::parse(text).unwrap();
        }
    }

    fn doc(bindings: &[(&str, &str, Scope)], removed: &[(&str, &str)]) -> ShortcutsDoc {
        ShortcutsDoc {
            bindings: bindings
                .iter()
                .map(|(command, keys, scope)| UserBinding {
                    command: (*command).into(),
                    keys: (*keys).into(),
                    scope: *scope,
                })
                .collect(),
            removed: removed
                .iter()
                .map(|(command, keys)| Removed {
                    command: (*command).into(),
                    keys: (*keys).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn user_bindings_replace_defaults_and_removals_apply() {
        let k = Keymap::resolve(&ShortcutsDoc::default(), Platform::MacOs);
        assert_eq!(k.app(&c("Ctrl+Z")), Some("edit.undo"));
        assert!(k.problems().is_empty());

        let d = doc(
            &[("stream.toggle", "Ctrl+Z", Scope::App)],
            &[("edit.redo", "Ctrl+Shift+Z")],
        );
        let k = Keymap::resolve(&d, Platform::MacOs);
        assert_eq!(k.app(&c("Ctrl+Z")), Some("stream.toggle"));
        assert_eq!(k.of("edit.undo").count(), 0);
        assert_eq!(k.app(&c("Ctrl+Shift+Z")), None);
        assert!(k.problems().is_empty());
    }

    #[test]
    fn conflicts_reserved_and_global_problems() {
        let d = doc(
            &[
                ("record.toggle", "Ctrl+Alt+R", Scope::Global),
                ("stream.toggle", "Ctrl+Alt+R", Scope::App),
                ("stream.toggle", "Ctrl+Q", Scope::App),
                ("stream.toggle", "Shift+S", Scope::Global),
            ],
            &[],
        );
        let k = Keymap::resolve(&d, Platform::MacOs);
        // Global chords are not run in app scope.
        assert_eq!(k.app(&c("Ctrl+Alt+R")), None);
        assert_eq!(k.global().count(), 1);
        let kinds: Vec<&ProblemKind> = k.problems().iter().map(|p| &p.kind).collect();
        assert_eq!(kinds.len(), 3);
        assert_eq!(kinds[0], &ProblemKind::Conflict("record.toggle".into()));
        assert!(matches!(kinds[1], ProblemKind::Reserved(_)));
        assert!(matches!(kinds[2], ProblemKind::Scope(_)));

        let k = Keymap::resolve(&d, Platform::Linux);
        assert_eq!(k.problems()[0].kind, ProblemKind::NoGlobal);
        // Ctrl+Q is fine on Linux.
        assert_eq!(k.app(&c("Ctrl+Q")), Some("stream.toggle"));
    }

    #[test]
    fn too_many_bindings_per_command() {
        let d = doc(
            &[
                ("stream.toggle", "Ctrl+F1", Scope::App),
                ("stream.toggle", "Ctrl+F2", Scope::App),
                ("stream.toggle", "Ctrl+F3", Scope::App),
                ("stream.toggle", "Ctrl+F4", Scope::App),
                ("stream.toggle", "Ctrl+F5", Scope::App),
            ],
            &[],
        );
        let k = Keymap::resolve(&d, Platform::Windows);
        assert_eq!(k.of("stream.toggle").count(), MAX_PER_COMMAND);
        assert_eq!(k.problems()[0].kind, ProblemKind::TooMany);
    }

    #[test]
    fn check_answers_for_recording() {
        let k = Keymap::resolve(&ShortcutsDoc::default(), Platform::Windows);
        assert_eq!(k.check(&c("Ctrl+K"), Scope::App), Check::Free);
        assert_eq!(
            k.check(&c("Ctrl+Z"), Scope::Global),
            Check::Taken {
                command: "edit.undo".into(),
                scope: Scope::App
            }
        );
        assert!(matches!(
            k.check(&c("Alt+F4"), Scope::App),
            Check::Refused(_)
        ));
        assert!(matches!(k.check(&c("K"), Scope::Global), Check::Refused(_)));
        assert!(matches!(
            k.check(&c("Ctrl+S"), Scope::Global),
            Check::Warn(_)
        ));
        assert_eq!(k.check(&c("Ctrl+S"), Scope::App), Check::Free);
        let linux = Keymap::resolve(&ShortcutsDoc::default(), Platform::Linux);
        assert!(matches!(
            linux.check(&c("Ctrl+K"), Scope::Global),
            Check::Refused(_)
        ));
    }

    #[test]
    fn editing_keeps_the_document_minimal() {
        let mut d = ShortcutsDoc::default();
        d.remove("edit.undo", c("Ctrl+Z"));
        assert_eq!(d.removed.len(), 1);
        d.remove("edit.undo", c("Ctrl+Z"));
        assert_eq!(d.removed.len(), 1, "removed once");
        d.add("edit.undo", c("Ctrl+Z"), Scope::App);
        assert_eq!(
            d,
            ShortcutsDoc::default(),
            "adding the default back undoes the removal"
        );

        d.add(
            "ui.workspace.select?name=Live%20show",
            c("Ctrl+L"),
            Scope::App,
        );
        assert_eq!(
            d.bindings[0].command,
            "ui.workspace.select?name=Live%20show"
        );
        // Ids are stored canonically.
        d.add(
            "ui.workspace.select?name=Live%20show",
            c("Ctrl+L"),
            Scope::Global,
        );
        d.remove("ui.workspace.select?name=Live%20show", c("Ctrl+L"));
        d.add("x.y?b=1&a=2", c("F5"), Scope::App);
        assert_eq!(d.bindings[0].command, "x.y?a=2&b=1");
        d.reset("x.y?a=2&b=1");
        d.add(
            "ui.workspace.select?name=Live%20show",
            c("Ctrl+L"),
            Scope::App,
        );
        assert_eq!(d.bindings.len(), 1, "no duplicate");
        assert!(d.changed("ui.workspace.select?name=Live%20show"));
        d.remove("ui.workspace.select?name=Live%20show", c("Ctrl+L"));
        assert!(d.bindings.is_empty() && d.removed.is_empty());

        // Replace moves a chord from its holder.
        let k = Keymap::resolve(&d, Platform::MacOs);
        d.replace(&k, "record.toggle", c("Ctrl+Z"), Scope::App);
        let k = Keymap::resolve(&d, Platform::MacOs);
        assert_eq!(k.app(&c("Ctrl+Z")), Some("record.toggle"));
        assert!(k.problems().is_empty());
        d.reset("record.toggle");
        d.reset("edit.undo");
        assert_eq!(d, ShortcutsDoc::default());
        d.add("stream.toggle", c("F13"), Scope::Global);
        d.reset_all();
        assert_eq!(d, ShortcutsDoc::default());
    }

    #[test]
    fn document_text_round_trips() {
        let d = doc(
            &[("scene.select?id=7", "Ctrl+Alt+7", Scope::Global)],
            &[("edit.redo", "Ctrl+Y")],
        );
        let text = toml::to_string(&d).unwrap();
        assert!(text.contains("[[binding]]") && text.contains("scope = \"global\""));
        let back: ShortcutsDoc = toml::from_str(&text).unwrap();
        assert_eq!(back, d);
        d.validate().unwrap();
        let app: ShortcutsDoc =
            toml::from_str("[[binding]]\ncommand = \"a.b\"\nkeys = \"F5\"\n").unwrap();
        assert_eq!(app.bindings[0].scope, Scope::App);
        assert!(toml::from_str::<ShortcutsDoc>("extra = 1\n").is_err());
        assert!(
            doc(&[("a.b", "Ctrl+Shift+1", Scope::App)], &[])
                .validate()
                .is_err()
        );
        assert!(doc(&[("Bad", "F5", Scope::App)], &[]).validate().is_err());
    }

    /// Resolving any document never puts one chord in force twice and never panics
    /// (a deterministic pseudo-random walk over documents).
    #[test]
    fn any_document_resolves_to_unique_chords() {
        let commands = [
            "edit.undo",
            "edit.redo",
            "stream.toggle",
            "record.toggle",
            "ui.area.toggle?area=left",
            "x.unknown",
        ];
        let chords = [
            "Ctrl+Z",
            "Ctrl+Shift+Z",
            "Ctrl+Y",
            "Ctrl+1",
            "F13",
            "Shift+A",
            "Ctrl+Q",
            "Alt+F4",
            "Meta+K",
            "Ctrl+Alt+R",
            "Alt+Q",
            "F21",
        ];
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = |n: usize| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) as usize % n
        };
        for _ in 0..500 {
            let mut d = ShortcutsDoc::default();
            for _ in 0..next(12) {
                let scope = if next(3) == 0 {
                    Scope::Global
                } else {
                    Scope::App
                };
                d.bindings.push(UserBinding {
                    command: commands[next(commands.len())].into(),
                    keys: chords[next(chords.len())].into(),
                    scope,
                });
            }
            for _ in 0..next(3) {
                d.removed.push(Removed {
                    command: commands[next(commands.len())].into(),
                    keys: chords[next(chords.len())].into(),
                });
            }
            for p in PLATFORMS {
                let k = Keymap::resolve(&d, p);
                let mut seen: Vec<Chord> = k.bindings().iter().map(|b| b.chord).collect();
                seen.sort();
                let n = seen.len();
                seen.dedup();
                assert_eq!(seen.len(), n);
                for b in k.bindings() {
                    assert!(b.chord.reserved(p).is_none());
                    assert!(
                        b.scope == Scope::App
                            || (b.chord.global_problem(p).is_none() && p.has_global())
                    );
                    assert!(b.scope == Scope::Global || b.chord.app_problem(p).is_none());
                    assert_eq!(k.holder(&b.chord), Some(b));
                }
                for cmd in commands {
                    assert!(k.of(cmd).count() <= MAX_PER_COMMAND);
                }
            }
        }
    }
}
