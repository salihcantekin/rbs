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
    pub fn new(mods: Mods, key: Key) -> Result<Chord, ChordError>;    // refuses Shift + digit or punctuation
    pub fn parse(s: &str) -> Result<Chord, ChordError>;               // case-insensitive, any modifier order
    pub fn from_typed(c: char, mods: Mods) -> Result<Chord, &'static str>; // a key press's character
    pub fn label(&self, platform: Platform) -> String;                // "⇧⌘Z" on macOS, "Ctrl+Shift+Z" elsewhere
    pub fn reserved(&self, platform: Platform) -> Option<&'static str>;
    pub fn global_problem(&self, platform: Platform) -> Option<&'static str>;
    pub fn app_problem(&self, platform: Platform) -> Option<&'static str>;
    pub fn common_in_other_apps(&self) -> bool;
}
impl Display for Chord { /* canonical text: Ctrl+Alt+Shift+Meta+Key */ }
```

- Text: modifiers then one key, joined by `+`. Canonical order `Ctrl`, `Alt`, `Shift`, `Meta`; `parse(c.to_string()) == c` for every chord.
- `Ctrl` is the primary modifier (Command on macOS), `Meta` is Control on macOS and the Windows key on Windows, `Alt` is Option.
- Named keys: `Space`, `Enter`, `Escape`, `Tab`, `Backspace`, `Delete`, `Insert`, `Home`, `End`, `PageUp`, `PageDown`, `Up`, `Down`, `Left`, `Right`, `Minus`, `Equal`, `BracketLeft`, `BracketRight`, `Backslash`, `Semicolon`, `Quote`, `Comma`, `Period`, `Slash`, `Backquote`.
- Refused (`ChordError` with the reason): empty text, a repeated modifier, no key or two keys, unknown key names, `Shift` with a digit or a punctuation key, text longer than 64 bytes.
- `from_typed`: ASCII letters (upper-cased; `ı`, `İ` and other letters are refused), digits and known punctuation characters with Shift dropped (AZERTY's Shift+`&` is the digit 1), a space as `Space`; anything else is refused with the reason.
- Labels: macOS uses the symbols ⌘ (Ctrl), ⌥ (Alt), ⇧ (Shift), ⌃ (Meta) in the order ⌃⌥⇧⌘ (Apple's order) followed by the key; elsewhere the canonical text with `Win` for `Meta`. Labels are for display only; files and the API use the canonical text.

### Scopes and bindings

```rust
pub enum Scope { App, Global }
pub struct Binding { pub command: String /* a CommandId, canonical */, pub chord: Chord, pub scope: Scope }
pub enum Platform { MacOs, Windows, Linux }
```

- A command id is any text of the `CommandId` grammar (ADR-0050 decision 2), stored canonically; it need not resolve (unknown commands are kept).
- At most 4 bindings per command.

### Defaults

- Every registry entry whose `shortcut` is set gives a default app binding of its plain id.
- `rbs_ui::shortcut::DEFAULT_SHORTCUTS: &[(&str, &str)]` gives the rest, as `(command id, chord)`: `edit.redo` `Ctrl+Y` (Windows convention, also on macOS where it does nothing else), and `scene.position?n=1` … `n=9` → `Ctrl+1` … `Ctrl+9`.
- No default is global, none uses `Ctrl+Alt`, none is reserved or has an app-scope problem on any platform, and no two defaults share a chord (tested). A default whose command is not in the registry of this build is left out.

### Keymap (`rbs_ui::shortcut::Keymap`)

```rust
impl Keymap {
    pub fn resolve(doc: &ShortcutsDoc, platform: Platform) -> Keymap;
    pub fn bindings(&self) -> &[Active];          // in force, in a stable order
    pub fn problems(&self) -> &[Problem];         // bindings not in force, with why
    pub fn app(&self, chord: &Chord) -> Option<&str>;    // the command, app scope
    pub fn global(&self) -> impl Iterator<Item = (&Chord, &str)>;
    pub fn holder(&self, chord: &Chord) -> Option<&Active>;
    pub fn of(&self, command: &str) -> impl Iterator<Item = &Active>;
    pub fn check(&self, chord: &Chord, scope: Scope) -> Check; // for the editor
}
pub enum Check { Free, Taken { command: String, scope: Scope }, Refused(String), Warn(String) }
```

Resolution, in order:

1. Start from the defaults, drop those listed in `removed` (matched by command and chord).
2. Add user bindings in file order. A user binding whose chord a default holds replaces that default (the default is dropped, not listed as a problem). Problems, in this order: reserved chords (`Reserved`), global bindings where the platform has none (`NoGlobal`), a scope rule (`Scope`: `global_problem` for global, `app_problem` for app), a chord an earlier user binding holds (`Conflict`), a fifth binding of one command (`TooMany`).
3. Defaults whose chord is free, not removed, not reserved and without an app problem are added while their command has fewer than 4 bindings.

`app(chord)` answers only for app-scope bindings; a chord bound globally is never looked up in app scope. `check` answers for a chord being recorded: `Refused` with the reason for reserved chords, global scope on a platform without it and scope rules; `Taken` when a binding in force holds it; `Warn` for a free global chord that other apps commonly use (`Ctrl` alone with C, V, X, Z, A, S, F, N, O, P, T or W); else `Free`.

### Reserved chords

Refused in every scope (as RBS chord text; on macOS `Ctrl` is Command and `Meta` is Control):

| Platform | Reserved |
| --- | --- |
| macOS | Ctrl+Q, Ctrl+W, Ctrl+H, Ctrl+M, Ctrl+Comma, Ctrl+Tab, Ctrl+Space, Ctrl+Backquote, Ctrl+Alt+Escape, Ctrl+Meta+Q, Meta+Space, Meta+Left, Meta+Right, Meta+Up, Meta+Down |
| Windows | every chord with Meta, Alt+F4, Alt+Tab, Alt+Space, Ctrl+Escape, Ctrl+Shift+Escape, F10, Shift+F10 |
| Linux | every chord with Meta, Alt+F4, Alt+Tab, Alt+Space |

macOS screenshot keys (Cmd+Shift+3, 4, 5) cannot be written at all (Shift with a digit).

### Scope rules

- Global on macOS: needs `Ctrl` (Command) or `Meta` (Control), or is F13 to F20 alone; F21 to F24 cannot be global.
- Global on Windows (and Linux, for when it exists): `Ctrl` without `Alt`, or `Alt` with `Shift` and without `Ctrl`, or F13 to F24 alone. `Ctrl+Alt` is AltGr on many layouts.
- App on macOS: `Alt` (Option) without `Ctrl` or `Meta` on a letter, digit, punctuation key or Space is refused: the toolkit sees the character Option makes, not the key.

### Shortcuts document (`shortcuts.toml`, `kind = "shortcuts"`, format 1)

```toml
format = 1
kind = "shortcuts"

[[binding]]
command = "scene.select?collection=3f2a9c&scene=7"
keys = "Ctrl+Shift+F7"
scope = "global"          # "app" (default) or "global"

[[removed]]
command = "edit.redo"
keys = "Ctrl+Y"
```

- At most 64 KiB and 512 `binding` plus `removed` entries; unknown keys are errors (ADR-0051). `keys` must parse and `command` must have the command id grammar. A file that breaks any of these fails as a whole (it is a hand edit; the user fixes it or resets).
- A missing file is the defaults. A file that cannot be read gives the defaults and a note; RBS does not write the file until the user changes a shortcut, so the hand-edited file survives until then.
- `UserData::shortcuts() -> (ShortcutsDoc, Option<DataError>)`, `UserData::set_shortcuts(&WriterLock, &ShortcutsDoc)`.
- Editing operations on `ShortcutsDoc` keep it minimal: adding a binding equal to a removed default deletes the `removed` entry instead; removing a user binding deletes it; removing a default adds a `removed` entry; Reset of one command deletes its `binding` and `removed` entries; Reset all empties both lists.

### App scope (`rbs-app`)

- Main and floating windows forward key presses that no control took to `Shortcuts.press(text, ctrl, alt, shift, meta, repeat) -> bool` from their root `FocusScope`, which has the focus when the window opens (`forward-focus`) and whenever no control has it. The root scope has no `KeyBinding`s. The main window handles Cmd+W itself on macOS (close, ADR-0056).
- `rbs_app::hotkeys::read_key(text, mods) -> Read` maps the toolkit's text: its special key codes to named keys and F-keys (Backtab is Shift+Tab), modifier keys alone to `Read::Modifier`, everything else through `Chord::from_typed`. The shell runs `app.run_command(id)` when `Keymap::app` holds the chord. Repeats are accepted but run nothing. Keys that map to no bound chord are rejected so the window handles them as before.
- The keymap's app table is rebuilt only when the keymap changes; a key press is one hash lookup.
- The Settings window, dialogs and projectors do not run shortcuts.

### Global scope (`rbs-app`)

```rust
pub enum Refusal { TakenByOtherApp, Unsupported, Other(String) }
pub trait GlobalKeys {
    /// Registers exactly `chords`, one at a time: for each, the id its presses report,
    /// or why the OS refused it.
    fn set(&mut self, chords: &[Chord]) -> Vec<Result<u32, Refusal>>;
}
pub fn system(on_event: fn(u32 /* id */, bool /* pressed */)) -> Result<Box<dyn GlobalKeys>, String>;
```

- The app keeps a `GlobalKeys` only while the keymap has global bindings; otherwise none exists and nothing is registered. It is created and dropped on the UI thread; dropping unregisters every chord.
- macOS and Windows: `system` wraps `global-hotkey`. RBS `Ctrl` is the crate's `META` on macOS and `CONTROL` on Windows, `Meta` the other one. The crate's process-global event handler is set once, before the first manager, and hands `(id, pressed)` to the UI thread. There a press runs the command through the control-surface path (`App::run_command_as(id, false)`: the enabled check, no local confirmation, ADR-0060 decision 7); a second press of an id before its release is ignored (`Presses`).
- Refusals become problems on the page: `TakenByOtherApp` ("Another app uses this shortcut."), `Unsupported`, `Other(why)`. `system` failing lists every global binding with its reason.
- While the page records a chord, `set(&[])` is called, and the keymap's chords are set again when recording ends (Add, Cancel, Escape, leaving the section or closing the window).
- Linux: `system` fails; global bindings are listed with "Global shortcuts are not available on this system yet."

### Scene commands (`rbs_ui::command`)

Scene menu, acting on the canvas the main window shows, switching the program scene as the Scenes panel does:

| Id | Kind | Parameters | Runs |
| --- | --- | --- | --- |
| `scene.select` | picker over the canvas's scenes in panel order; the Scene menu lists them | `scene` (scene id, the picker's choice), `collection` (collection id, added from `CommandState::context` when a choice is bound) | `scene.switch {scene}` when `collection` is absent or the one in use; else disabled: "In another scene collection." and the page lists the row as "In another scene collection: runs nothing until it is in use." |
| `scene.position` | action, in no menu (`NOT_IN_MENU`) | `n`, one of `1`…`9` | the scene at that position; disabled when there is none ("No scene is at this position.") |
| `scene.next`, `scene.previous` | action | none | the scene after or before the program scene, wrapping; disabled with one scene ("There is only one scene.") |

### Settings › Shortcuts (`rbs-app`)

Section after Appearance in the Settings list. Layout, top to bottom:

1. A hint: what the two scopes do and that global ones start and stop outputs without asking; on macOS also that system shortcuts win, that a password field in another app pauses global shortcuts (Secure Input), and that letters of global shortcuts are the keys of a US keyboard; on Linux that global shortcuts are not available yet.
2. A search field (filters rows by title, group, chord label or chord text; the row being recorded always stays) and Reset all (secondary button; asks "Reset every shortcut to its default?" inline with Reset all and Cancel).
3. Groups by menu, in menu bar order, each a caption heading in capitals. Rows: the command's title (picker choices as "Picker: Choice", for example "Scene: Game", "Workspace: Live"; one row per value of a required parameter with fixed values, for example "Left area"), its chord chips (KeyboardShortcutHint: the label, a "Global" tag on global ones; a remove button with the accessible label "Remove <chord>"), then in a column of their own Reset (when the row differs from its defaults) and Add (disabled at 4 shortcuts). Problems under the row in the error style, each prefixed by its chord label.
4. Add opens a recorder under the row (role group, name "New shortcut for <title>", focus moves into it): "Press the keys. Escape cancels." Pressing keys fills four modifier checkboxes (named Command, Option, Shift, Control on macOS; Ctrl, Alt, Shift, Win on Windows) and a key picker, which can also be set by hand; Tab moves to them. A checkbox "Works when RBS is in the background (global)" (disabled where global shortcuts do not exist). The chord label and the result of `check` show at once: nothing for `Free`, the reason for `Refused` (error style), the warning for `Warn`, "<label> is used by <title>. Replace moves it here." for `Taken`. Buttons: Add (Replace for `Taken`), disabled while nothing valid is chosen, and Cancel. Escape cancels. A chord the toolkit cannot read (Option chords on macOS) says so and points to the pickers.
5. Global bindings of `stream.toggle` and `record.toggle` show the note "Its global shortcut starts or ends the stream without asking." / "Its global shortcut starts or stops the recording without asking."

Rows for picker choices list the current choices (scenes of the canvas, workspaces, output profiles, scene collections, panels) and every bound choice that no longer exists ("Scene not found").

## Errors and failure behaviour

- A bad `shortcuts.toml`: defaults are used, the error bar says why once, the file is not overwritten until a change.
- A write that fails: the change stays in effect for this session and the error bar says it was not saved.
- An OS refusal for a global chord: that binding is inactive and listed; the others work.
- `global-hotkey` failing to start: every global binding is listed as inactive with the reason.

## Performance requirements

- No global bindings: no hotkey manager, no hidden window, no registered key (P13). The crate's event handler, once set, does nothing until an event comes.
- A key press in app scope: one chord build and one hash lookup on the UI thread; no allocation in the lookup for chords that are not bound beyond the toolkit's own event.
- Nothing polls while no bound global chord is pressed. On Windows the crate checks a pressed global chord every 50 ms on a short-lived thread until it is released (to report the release); that cost exists only while a bound key is held.

## Conformance tests

- `Chord`: parse and display round trip for every key and modifier set (property test); every refusal case; macOS and Windows labels.
- Defaults: unique chords, none reserved on any platform, none global, none `Ctrl+Alt`, every id resolves in the registry (or names a scene command).
- Keymap: removed defaults, user replacing a default, conflicts kept in file order, reserved per platform, global modifier rule, limits; `check` for each outcome; resolution of an arbitrary document never panics and never puts one chord in force twice (property test).
- Document: round trip, unknown keys refused, bad chord refused, size limit, minimal editing operations.
- App: key mapping of letters, digits, F-keys, named keys and modifier keys from Slint text; repeats run nothing; a globally bound chord is not run in app scope; undo and redo defaults reach the studio.
- Global: with a fake `GlobalKeys`, nothing is created without global bindings; refused chords become problems; recording unregisters and restores; the manager is dropped with the last global binding; a held key runs once (`Presses`).
- Page: rows for commands, picker choices, fixed parameter values, missing choices and unknown commands, grouped in menu order; the recorder's states (taken, warning, refused, unusable key).

## Open questions

- Hold-to-act bindings (push-to-talk, push-to-mute) need key release events and, on macOS, an event tap; separate ADR when needed.
