# Spec: Shortcuts

- Status: Draft
- Owner crate(s): `rbs-ui` (`shortcut`), `rbs-user-data` (shortcuts document), `rbs-app` (key mapping, dispatch, global adapter, Settings › Shortcuts)
- Related ADRs: [ADR-0060](../adr/0060-shortcuts-in-app-and-global.md), ADR-0050, ADR-0051, ADR-0013, ADR-0056

## Purpose

Bind any command of the command registry to a key chord that works inside RBS (app scope) or whatever app has the focus (global scope), keep the user's changes across releases, and find conflicts before they take effect.

## Scope

In scope: the chord grammar, defaults, the keymap, conflicts and reserved chords, the shortcuts document, app and global dispatch on macOS and Windows, Settings › Shortcuts.

Out of scope (follow-ups, ADR-0060): menus showing chords, the control API for shortcuts, the command palette, Linux global shortcuts, hold-to-act bindings, macOS layout mapping of global chords.

## Contract

### Chords (`rbs_ui::shortcut::Chord`)

```rust
pub struct Mods { pub ctrl: bool, pub alt: bool, pub shift: bool, pub meta: bool }
pub enum Key { Letter(u8 /* b'A'..=b'Z' */), Digit(u8 /* 0..=9 */), F(u8 /* 1..=24 */), Named(Named) }
pub struct Chord { pub mods: Mods, pub key: Key }
impl Chord {
    pub fn parse(s: &str) -> Result<Chord, ChordError>;   // case-insensitive, any modifier order
    pub fn label(&self, platform: Platform) -> String;      // "⌘⇧Z" on macOS, "Ctrl+Shift+Z" elsewhere
}
impl Display for Chord { /* canonical text: Ctrl+Alt+Shift+Meta+Key */ }
```

- Text: modifiers then one key, joined by `+`. Canonical order `Ctrl`, `Alt`, `Shift`, `Meta`; `parse(c.to_string()) == c` for every chord.
- `Ctrl` is the primary modifier (Command on macOS), `Meta` is Control on macOS and the Windows key on Windows, `Alt` is Option.
- Named keys: `Space`, `Enter`, `Escape`, `Tab`, `Backspace`, `Delete`, `Insert`, `Home`, `End`, `PageUp`, `PageDown`, `Up`, `Down`, `Left`, `Right`, `Minus`, `Equal`, `BracketLeft`, `BracketRight`, `Backslash`, `Semicolon`, `Quote`, `Comma`, `Period`, `Slash`, `Backquote`.
- Refused (`ChordError` with the reason): empty text, a repeated modifier, no key or two keys, unknown key names, `Shift` with a digit or a punctuation key, text longer than 64 bytes.
- Labels: macOS uses the symbols ⌘ (Ctrl), ⌥ (Alt), ⇧ (Shift), ⌃ (Meta) in the order ⌃⌥⇧⌘ (Apple's order) followed by the key; elsewhere the canonical text with `Win` for `Meta`. Labels are for display only; files and the API use the canonical text.

### Scopes and bindings

```rust
pub enum Scope { App, Global }
pub struct Binding { pub command: String /* a CommandId, canonical */, pub chord: Chord, pub scope: Scope }
pub enum Platform { MacOs, Windows, Linux }
```

- A command id is any text of the `CommandId` grammar (ADR-0050 decision 2), stored canonically; it need not resolve (unknown commands are kept).
- At most 4 bindings per command and 512 in total.

### Defaults

- Every registry entry whose `shortcut` is set gives a default app binding of its plain id.
- `rbs_ui::shortcut::DEFAULT_SHORTCUTS: &[(&str, &str)]` gives the rest, as `(command id, chord)`: `edit.redo` `Ctrl+Y` (Windows convention, also on macOS where it does nothing else), and `scene.position?n=1` … `n=9` → `Ctrl+1` … `Ctrl+9`.
- No default is global, none uses `Ctrl+Alt`, none is reserved on any platform, and no two defaults share a chord (tested).

### Keymap (`rbs_ui::shortcut::Keymap`)

```rust
impl Keymap {
    pub fn resolve(doc: &ShortcutsDoc, platform: Platform) -> Keymap;
    pub fn bindings(&self) -> &[Active];          // in force, in a stable order
    pub fn problems(&self) -> &[Problem];         // bindings not in force, with why
    pub fn app(&self, chord: &Chord) -> Option<&str>;    // the command, app scope
    pub fn global(&self) -> impl Iterator<Item = (&Chord, &str)>;
    pub fn check(&self, chord: &Chord, scope: Scope, platform: Platform) -> Check; // for the editor
}
pub enum Check { Free, Taken { command: String }, Refused(String) }
```

Resolution, in order:

1. Start from the defaults, drop those listed in `removed` (matched by command and chord).
2. Add user bindings in file order. A user binding whose chord a default holds replaces that default (the default is dropped, not listed as a problem). A user binding whose chord an earlier user binding holds is a problem (`Conflict`), as are reserved chords (`Reserved`), global chords that need a modifier (`NeedsModifier`), and bindings past the limits (`TooMany`).
3. Bindings of a command with more than 4 are kept in order; the rest are `TooMany`.

`app(chord)` answers only for app-scope bindings; a chord bound globally is never looked up in app scope. `check` answers for a chord being recorded: `Refused` with the reason for reserved chords and global chords without a modifier, `Taken` when a binding in force holds it, else `Free`.

### Reserved chords

| Platform | Reserved in every scope |
| --- | --- |
| macOS | Ctrl+Q, Ctrl+W, Ctrl+H, Ctrl+M, Ctrl+Comma, Ctrl+Tab, Ctrl+Space, Ctrl+Backquote, Ctrl+Alt+Escape (as Cmd) |
| Windows | Alt+F4, Alt+Tab, Ctrl+Escape, every chord with Meta |
| Linux | Alt+F4, Alt+Tab, every chord with Meta |

Global chords also need `Ctrl`, `Alt` or `Meta`, unless the key is `F13`–`F24`.

### Shortcuts document (`shortcuts.toml`, `kind = "shortcuts"`, format 1)

```toml
format = 1
kind = "shortcuts"

[[binding]]
command = "scene.select?id=7"
keys = "Ctrl+Alt+7"
scope = "global"          # "app" (default) or "global"

[[removed]]
command = "edit.redo"
keys = "Ctrl+Y"
```

- At most 64 KiB and 512 `binding` plus `removed` entries; unknown keys are errors (ADR-0051). `keys` must parse; an entry whose `keys` does not parse fails the whole file (it is a hand edit; the user fixes it or resets).
- A missing file is the defaults. A file that cannot be read gives the defaults and a note; RBS does not write the file until the user changes a shortcut, so the hand-edited file survives until then.
- `UserData::shortcuts() -> (ShortcutsDoc, Option<DataError>)`, `UserData::set_shortcuts(&WriterLock, &ShortcutsDoc)`.
- Editing operations on `ShortcutsDoc` keep it minimal: adding a binding equal to a removed default deletes the `removed` entry instead; removing a user binding deletes it; removing a default adds a `removed` entry; Reset of one command deletes its `binding` and `removed` entries; Reset all empties both lists.

### App scope (`rbs-app`)

- Main and floating windows forward key presses that no control took to `Shortcuts.key(text, ctrl, alt, shift, meta, repeat) -> bool`. The shell maps the toolkit's text to a `Key` (letters upper-cased, the toolkit's special key codes to named keys and F-keys), builds the chord, and runs `app.run_command(id)` when `Keymap::app` holds it. Repeats are accepted but run nothing. Keys that map to no chord are rejected so the window handles them as before.
- The keymap's app table is rebuilt only when the keymap changes; a key press is one hash lookup.
- The Settings window, dialogs and projectors do not run shortcuts.

### Global scope (`rbs-app`)

```rust
pub trait GlobalKeys {
    /// Registers exactly `chords` (ids are positions); returns the ones the OS refused.
    fn set(&mut self, chords: &[Chord]) -> Vec<(usize, String)>;
}
```

- The app keeps a `GlobalKeys` only while the keymap has global bindings; otherwise none exists and nothing is registered.
- On macOS and Windows the implementation wraps `global-hotkey`: one `GlobalHotKeyManager`, an event handler that forwards pressed events to the UI thread, which runs the command through the control-surface path (no local confirmation, ADR-0060 decision 7). Released events are ignored.
- Refused chords become problems (`TakenByOtherApp`) shown on the page.
- While the page records a chord, `set(&[])` is called, and the keymap's chords are set again when recording ends.
- Linux: no implementation; global bindings are listed with "Global shortcuts are not available on Linux yet".

### Settings › Shortcuts (`rbs-app`)

Section after Appearance in the Settings list. Layout, top to bottom:

1. A hint line: "Shortcuts run commands. Global shortcuts also work while another app is in front." On macOS, add "System shortcuts win over RBS."
2. A search field (filters rows by title, group or chord label) and Reset all (secondary button; asks "Reset every shortcut to its default?" with Reset all and Cancel).
3. Groups by menu, in menu bar order, each a caption heading. Rows: the command's title (picker choices as "Picker: Choice", for example "Scene: Game", "Workspace: Live"), its chord chips (label, a "Global" tag on global ones, a remove button with the accessible label "Remove <chord>"), an Add button, and Reset when the row differs from its defaults. Problem text under the row in the error style.
4. Add turns the row into a recorder: "Press the keys. Escape cancels.", a checkbox "Works when RBS is in the background" (on Linux disabled with the reason), then the chord label and the result of `check`: nothing for `Free` (saved at once), "<label> is used by <title>." with Replace and Cancel for `Taken`, the reason for `Refused`. Escape, or focus leaving the row, cancels.
5. Global bindings of `stream.toggle` and `record.toggle` show the note "Ends the stream without asking." / "Stops the recording without asking."

Rows for picker choices list the current choices (scenes of the canvas, workspaces, output profiles, scene collections, panels) and every bound choice that no longer exists ("Scene not found").

## Errors and failure behaviour

- A bad `shortcuts.toml`: defaults are used, the error bar says why once, the file is not overwritten until a change.
- A write that fails: the change stays in effect for this session and the error bar says it was not saved.
- An OS refusal for a global chord: that binding is inactive and listed; the others work.
- `global-hotkey` failing to start: every global binding is listed as inactive with the reason.

## Performance requirements

- No global bindings: no hotkey manager, no hidden window, no event handler (P13).
- A key press in app scope: one chord build and one hash lookup on the UI thread; no allocation in the lookup for chords that are not bound beyond the toolkit's own event.
- Nothing polls; the OS calls back on the UI thread.

## Conformance tests

- `Chord`: parse and display round trip for every key and modifier set (property test); every refusal case; macOS and Windows labels.
- Defaults: unique chords, none reserved on any platform, none global, none `Ctrl+Alt`, every id resolves in the registry (or names a scene command).
- Keymap: removed defaults, user replacing a default, conflicts kept in file order, reserved per platform, global modifier rule, limits; `check` for each outcome; resolution of an arbitrary document never panics and never puts one chord in force twice (property test).
- Document: round trip, unknown keys refused, bad chord refused, size limit, minimal editing operations.
- App: key mapping of letters, digits, F-keys and named keys from Slint text; repeats run nothing; a globally bound chord is not run in app scope; undo and redo defaults reach the studio.
- Global: with a fake `GlobalKeys`, nothing is created without global bindings; refused chords become problems; recording unregisters and restores.

## Open questions

- Hold-to-act bindings (push-to-talk, push-to-mute) need key release events and, on macOS, an event tap; separate ADR when needed.
