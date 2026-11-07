//! Shortcuts in the shell (ADR-0060, `docs/specs/shortcuts.md`): reading the toolkit's
//! key events as chords, and global shortcuts through the OS.
//!
//! - [`read_key`]: a Slint key press as a [`Chord`] (app scope and the recorder).
//! - [`GlobalKeys`]: chords registered with the OS; [`system`] is the macOS and Windows
//!   implementation (the `global-hotkey` crate), tests use a fake.
//! - [`Presses`]: a held global chord runs its command once.

use rbs_ui::shortcut::{Chord, Key, Mods, Named};
use slint::platform::Key as SKey;

/// What a key press is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// A modifier key alone (the recorder keeps waiting).
    Modifier,
    /// A chord.
    Chord(Chord),
    /// A key that cannot be in a chord, with the reason.
    Unusable(&'static str),
}

const MODIFIER_KEYS: [SKey; 9] = [
    SKey::Shift,
    SKey::ShiftR,
    SKey::Control,
    SKey::ControlR,
    SKey::Alt,
    SKey::AltGr,
    SKey::Meta,
    SKey::MetaR,
    SKey::CapsLock,
];

fn special(c: char) -> Option<Key> {
    let named = [
        (SKey::Backspace, Named::Backspace),
        (SKey::Tab, Named::Tab),
        (SKey::Return, Named::Enter),
        (SKey::Escape, Named::Escape),
        (SKey::Delete, Named::Delete),
        (SKey::Space, Named::Space),
        (SKey::UpArrow, Named::Up),
        (SKey::DownArrow, Named::Down),
        (SKey::LeftArrow, Named::Left),
        (SKey::RightArrow, Named::Right),
        (SKey::Insert, Named::Insert),
        (SKey::Home, Named::Home),
        (SKey::End, Named::End),
        (SKey::PageUp, Named::PageUp),
        (SKey::PageDown, Named::PageDown),
    ];
    if let Some((_, n)) = named.iter().find(|(k, _)| char::from(k.clone()) == c) {
        return Some(Key::Named(*n));
    }
    let f1 = u32::from(char::from(SKey::F1));
    let n = u32::from(c).checked_sub(f1)?;
    (n < 24).then(|| Key::F(n as u8 + 1))
}

/// Reads a key press: the toolkit's `text` for the key and the modifiers as Slint
/// reports them (Command as `ctrl` and Control as `meta` on macOS).
pub fn read_key(text: &str, mods: Mods) -> Read {
    let mut chars = text.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return Read::Unusable("This key cannot be used in a shortcut");
    };
    if MODIFIER_KEYS.iter().any(|k| char::from(k.clone()) == c) {
        return Read::Modifier;
    }
    if char::from(SKey::Backtab) == c {
        // Shift+Tab.
        return Read::Chord(Chord {
            mods,
            key: Key::Named(Named::Tab),
        });
    }
    if let Some(key) = special(c) {
        return Read::Chord(Chord { mods, key });
    }
    match Chord::from_typed(c, mods) {
        Ok(chord) => Read::Chord(chord),
        Err(why) => Read::Unusable(why),
    }
}

/// Why the OS did not register a global chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Another app holds it.
    TakenByOtherApp,
    /// The OS has no key for it here.
    Unsupported,
    /// Another error, in words.
    Other(String),
}

impl Refusal {
    /// The text the Settings page shows.
    pub fn text(&self) -> String {
        match self {
            Refusal::TakenByOtherApp => "Another app uses this shortcut.".into(),
            Refusal::Unsupported => "This key cannot be a global shortcut here.".into(),
            Refusal::Other(why) => format!("The system refused this shortcut: {why}."),
        }
    }
}

/// Chords registered with the OS as global shortcuts.
pub trait GlobalKeys {
    /// Registers exactly `chords`, one at a time (one refusal does not stop the
    /// others): for each, the id its presses report, or why it was refused.
    fn set(&mut self, chords: &[Chord]) -> Vec<Result<u32, Refusal>>;
}

/// Turns the OS's press and release events of global chords into one run per press: a
/// second press of an id before its release is ignored.
#[derive(Debug, Default)]
pub struct Presses {
    down: Vec<u32>,
}

impl Presses {
    /// A press of `id`; true when it should run.
    pub fn pressed(&mut self, id: u32) -> bool {
        if self.down.contains(&id) {
            return false;
        }
        self.down.push(id);
        true
    }

    /// A release of `id`.
    pub fn released(&mut self, id: u32) {
        self.down.retain(|d| *d != id);
    }

    /// Forget every held key (the chords were registered again).
    pub fn clear(&mut self) {
        self.down.clear();
    }
}

#[cfg(any(target_os = "macos", windows))]
mod os {
    use std::sync::OnceLock;

    use global_hotkey::hotkey::{Code, HotKey, Modifiers};
    use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
    use rbs_ui::shortcut::{Chord, Key, Named};

    use super::{GlobalKeys, Refusal};

    fn code(key: Key) -> Option<Code> {
        use Code::*;
        Some(match key {
            Key::Letter(c) => [
                KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN,
                KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
            ]
            .get(usize::from(c.checked_sub(b'A')?))?
            .clone(),
            Key::Digit(d) => [
                Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
            ]
            .get(usize::from(d))?
            .clone(),
            Key::F(n) => [
                F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16, F17, F18,
                F19, F20, F21, F22, F23, F24,
            ]
            .get(usize::from(n.checked_sub(1)?))?
            .clone(),
            Key::Named(n) => match n {
                Named::Space => Space,
                Named::Enter => Enter,
                Named::Escape => Escape,
                Named::Tab => Tab,
                Named::Backspace => Backspace,
                Named::Delete => Delete,
                Named::Insert => Insert,
                Named::Home => Home,
                Named::End => End,
                Named::PageUp => PageUp,
                Named::PageDown => PageDown,
                Named::Up => ArrowUp,
                Named::Down => ArrowDown,
                Named::Left => ArrowLeft,
                Named::Right => ArrowRight,
                Named::Minus => Minus,
                Named::Equal => Equal,
                Named::BracketLeft => BracketLeft,
                Named::BracketRight => BracketRight,
                Named::Backslash => Backslash,
                Named::Semicolon => Semicolon,
                Named::Quote => Quote,
                Named::Comma => Comma,
                Named::Period => Period,
                Named::Slash => Slash,
                Named::Backquote => Backquote,
            },
        })
    }

    /// The crate's hotkey for `chord`. RBS `Ctrl` is Command (`SUPER`) on macOS and
    /// Control on Windows; `Meta` is the other one (ADR-0060 decision 7).
    fn hotkey(chord: &Chord) -> Option<HotKey> {
        let m = chord.mods;
        let (primary, other) = if cfg!(target_os = "macos") {
            (Modifiers::SUPER, Modifiers::CONTROL)
        } else {
            (Modifiers::CONTROL, Modifiers::SUPER)
        };
        let mut mods = Modifiers::empty();
        for (on, flag) in [
            (m.ctrl, primary),
            (m.meta, other),
            (m.alt, Modifiers::ALT),
            (m.shift, Modifiers::SHIFT),
        ] {
            if on {
                mods |= flag;
            }
        }
        Some(HotKey::new(Some(mods), code(chord.key)?))
    }

    /// The OS's global shortcuts. Create and drop on the UI thread: on Windows the
    /// crate's hidden window belongs to the thread that creates it.
    pub struct System {
        manager: GlobalHotKeyManager,
        registered: Vec<HotKey>,
    }

    impl System {
        fn unregister_all(&mut self) {
            for h in self.registered.drain(..) {
                let _ = self.manager.unregister(h);
            }
        }
    }

    impl Drop for System {
        fn drop(&mut self) {
            // Windows' `Drop` only destroys the hidden window.
            self.unregister_all();
        }
    }

    impl GlobalKeys for System {
        fn set(&mut self, chords: &[Chord]) -> Vec<Result<u32, Refusal>> {
            self.unregister_all();
            chords
                .iter()
                .map(|chord| {
                    let Some(h) = hotkey(chord) else {
                        return Err(Refusal::Unsupported);
                    };
                    match self.manager.register(h) {
                        Ok(()) => {
                            self.registered.push(h);
                            Ok(h.id())
                        }
                        Err(global_hotkey::Error::AlreadyRegistered(_)) => {
                            Err(Refusal::TakenByOtherApp)
                        }
                        Err(global_hotkey::Error::FailedToRegister(why))
                            if why.contains("Unknown scancode") =>
                        {
                            Err(Refusal::Unsupported)
                        }
                        Err(e) => Err(Refusal::Other(e.to_string())),
                    }
                })
                .collect()
        }
    }

    /// Where events go: set once for the process (the crate's handler cannot be
    /// replaced), before the first manager exists.
    static SINK: OnceLock<fn(u32, bool)> = OnceLock::new();

    /// The OS's global shortcuts; `on_event(id, pressed)` gets every press and release
    /// on whatever thread the OS reports it (hand it to the UI thread). Only the first
    /// `on_event` of the process is used.
    pub fn system(on_event: fn(u32, bool)) -> Result<Box<dyn GlobalKeys>, String> {
        if SINK.set(on_event).is_ok() {
            GlobalHotKeyEvent::set_event_handler(Some(|e: GlobalHotKeyEvent| {
                if let Some(sink) = SINK.get() {
                    sink(e.id(), e.state() == HotKeyState::Pressed);
                }
            }));
        }
        let manager = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
        Ok(Box::new(System {
            manager,
            registered: Vec::new(),
        }))
    }
}

#[cfg(any(target_os = "macos", windows))]
pub use os::system;

/// No global shortcuts on this platform yet (ADR-0060).
#[cfg(not(any(target_os = "macos", windows)))]
pub fn system(_on_event: fn(u32, bool)) -> Result<Box<dyn GlobalKeys>, String> {
    Err("Global shortcuts are not available on this system yet".into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn chord(s: &str) -> Read {
        Read::Chord(Chord::parse(s).unwrap())
    }

    fn text(k: SKey) -> String {
        char::from(k).to_string()
    }

    #[test]
    fn slint_keys_read_as_chords() {
        assert_eq!(read_key("z", Mods::CTRL), chord("Ctrl+Z"));
        assert_eq!(read_key(&text(SKey::F5), Mods::NONE), chord("F5"));
        assert_eq!(read_key(&text(SKey::F24), Mods::CTRL), chord("Ctrl+F24"));
        assert_eq!(
            read_key(&text(SKey::Return), Mods::CTRL),
            chord("Ctrl+Enter")
        );
        assert_eq!(read_key(&text(SKey::Escape), Mods::NONE), chord("Escape"));
        assert_eq!(
            read_key(&text(SKey::PageDown), Mods::NONE),
            chord("PageDown")
        );
        assert_eq!(read_key(&text(SKey::UpArrow), Mods::NONE), chord("Up"));
        assert_eq!(read_key(" ", Mods::NONE), chord("Space"));
        let shift = Mods {
            shift: true,
            ..Mods::NONE
        };
        assert_eq!(read_key(&text(SKey::Backtab), shift), chord("Shift+Tab"));
        assert_eq!(read_key(&text(SKey::Shift), shift), Read::Modifier);
        assert_eq!(read_key(&text(SKey::Control), Mods::CTRL), Read::Modifier);
        assert!(matches!(read_key("!", shift), Read::Unusable(_)));
        assert!(matches!(read_key("", Mods::NONE), Read::Unusable(_)));
        assert!(matches!(read_key("ab", Mods::NONE), Read::Unusable(_)));
    }

    #[test]
    fn a_held_global_key_runs_once() {
        let mut p = Presses::default();
        assert!(p.pressed(7));
        assert!(!p.pressed(7));
        assert!(p.pressed(8));
        p.released(7);
        assert!(p.pressed(7));
        p.clear();
        assert!(p.pressed(8));
    }
}
