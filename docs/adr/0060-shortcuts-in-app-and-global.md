# ADR-0060: Shortcuts bind registry commands to key chords, in the app or globally; Settings › Shortcuts edits them

- Status: Proposed
- Date: 2026-11-07
- Deciders: Salih (owner)
- Builds on: ADR-0050 (command registry), ADR-0051 (user data documents), ADR-0013 (zero cost when unused), ADR-0056 (keep running after the main window closes), ADR-0057 (Settings › Appearance)
- Spec: [shortcuts](../specs/shortcuts.md)

## Context

The owner wants shortcuts that work both inside RBS and while it is in the background (for example switching scenes while recording a game that has the focus), an engine that runs them, and a Settings page to edit them.

What exists:

- The command registry (ADR-0050) lists every UI command once, with ids that may carry parameters (`ui.area.toggle?area=left`, picker choices such as `ui.workspace.select?name=Live`). Each entry has an optional default shortcut text (`Ctrl+Z` for Undo, `Ctrl+Shift+Z` for Redo), which nothing reads yet. `App::run_command(id)` runs any registry id from the menu bar, with the menu's enabled check and confirmations.
- The main and floating windows hard-code Cmd/Ctrl+Z, Shift+Z and Y (scene undo and redo) and Cmd+W (close on macOS) as Slint `KeyBinding`s in a root `FocusScope`. Text fields take their keys first, so their own undo keeps working.
- U7 asks for "a complete, rebindable shortcut map"; the design system spec says "All shortcuts are rebindable; global hotkeys are opt-in" and that Ctrl+Alt combinations must not be bound by default (AltGr). The glossary names the term: "Shortcut", and "global shortcut" when it works outside RBS.
- Slint 1.18.1 gives key events as text plus modifiers (Command reported as `control` and Control as `meta` on macOS) with a `repeat` flag. Its `MenuItem.shortcut` installs a real menu accelerator on macOS and Windows, which would take Cmd+Z from a focused text field.
- The multi-scene work (ADR-0059) adds scene switching to the studio (`SwitchScene(SceneId)`, stable scene ids never reused, `scene.switch` in the control API). It does not add registry entries: scene commands are added here.
- No Linux work for now (owner, 2026-11-04).

## Decision

1. **A shortcut binds a registry command id to a key chord, in one of two scopes.** A binding is `(command id with parameters, chord, scope)`. *App* scope works while an RBS window that runs commands has the focus (main and floating windows; not the Settings window, dialogs or projectors). *Global* scope works whatever app has the focus, also while RBS has no window shown (ADR-0056), on macOS and Windows. Any registry id can be bound, so every picker choice is a command of its own (`scene.select?id=7`, `ui.workspace.select?name=Live`). A command may have several bindings (at most 4); a chord has at most one binding across both scopes.

2. **Chords are layout-independent text with a fixed grammar.** Modifiers in the order `Ctrl`, `Alt`, `Shift`, `Meta`, then one key, joined by `+` (`Ctrl+Shift+F5`). `Ctrl` is Command on macOS and Control elsewhere (the primary modifier, as Slint and the registry already use it); `Meta` is Control on macOS and the Windows key on Windows; `Alt` is Option. Keys are `A`–`Z`, `0`–`9`, `F1`–`F24`, `Space`, `Enter`, `Escape`, `Tab`, `Backspace`, `Delete`, `Insert`, `Home`, `End`, `PageUp`, `PageDown`, `Up`, `Down`, `Left`, `Right` and the punctuation keys `Minus`, `Equal`, `BracketLeft`, `BracketRight`, `Backslash`, `Semicolon`, `Quote`, `Comma`, `Period`, `Slash`, `Backquote`. A chord is stored canonically, so one chord has one text. Not accepted, each with the reason shown: modifiers alone; letters outside A–Z (they exist on some layouts only); `Shift` with a digit or punctuation key (what it types depends on the layout); numpad keys as separate keys (the toolkit does not tell them apart).

3. **Defaults come from the registry and are opt-out.** An entry's `shortcut` is its default app binding; parameterised ids get defaults from a `DEFAULT_SHORTCUTS` table in `rbs_ui::shortcut` (Ctrl+1…9 select the scene at position 1…9 through `scene.position?n=1`…`9`). No default is global, and no default uses Ctrl+Alt (AltGr). Defaults are part of each release, not of the user's file, so later defaults reach users who did not change them.

4. **The user's changes are a document.** `shortcuts.toml` (`kind = "shortcuts"`, format 1) in the config directory, read and written through `rbs-user-data` like `appearance.toml` (ADR-0051 writer and lock), holds only differences from the defaults: `[[binding]]` entries the user added (`command`, `keys`, `scope = "app" | "global"`) and `[[removed]]` default bindings (`command`, `keys`). The *keymap* in force is: defaults, minus removed ones, minus defaults whose chord a user binding takes, plus user bindings. A file that cannot be read means the defaults, with a note in the error bar; the file is kept and never overwritten until the user saves a change. Limits: 512 bindings, 64 KiB. Unknown commands (a missing plugin, a deleted scene) are kept and listed as unavailable, as the toolbar does (ADR-0050 decision 5); a scene that is deleted leaves its binding listed as "Scene not found" until the user removes it.

5. **Conflicts are found before they take effect.**
   - *Same chord twice:* the editor never saves two bindings with one chord; recording a chord in use shows "Ctrl+2 is used by Scene: Game" with Replace (moves it) and Cancel. A hand-edited file with a repeated chord keeps the first in file order; the others are listed as conflicts and do nothing.
   - *Reserved chords* are refused in every scope, per platform: macOS Cmd+Q, Cmd+W, Cmd+H, Cmd+M, Cmd+Comma, Cmd+Tab, Cmd+Space, Cmd+Backquote, Cmd+Alt+Escape; Windows Alt+F4, Alt+Tab, Ctrl+Escape and every `Meta` chord (the Windows key belongs to the shell). Text editing keys (Ctrl+C, V, X, A, Z) are not reserved: a focused text field takes them first (decision 6).
   - *Global chords must not swallow typing:* a global chord has `Ctrl`, `Alt` or `Meta`, or is `F13`–`F24` alone.
   - *Another app owns it:* when the OS refuses to register a global chord (Windows `RegisterHotKey` fails when another app holds it), the binding is listed as "Another app uses this shortcut" and is inactive; nothing else is affected. macOS does not report this for every app, so the page says that system shortcuts win.
   The model checks all of these for any platform (the platform is a parameter, not a `cfg`), so both lists are tested everywhere.

6. **App scope runs through the window's key handling.** Each main and floating window has one root `FocusScope`; keys that a focused control does not take (text fields keep their own editing keys, U7) reach it, and the shell looks the chord up in a table built when the keymap changes. A match runs `run_command` exactly as the menu item does: the same enabled check, and the same confirmations (End stream asks while live). Repeated key events (a key held down) do not run commands again. A chord bound globally is not looked up in app scope: the OS delivers it once. The hard-coded undo and redo `KeyBinding`s become the defaults `edit.undo` Ctrl+Z, `edit.redo` Ctrl+Shift+Z and Ctrl+Y; Cmd+W stays a window key (reserved, decision 5).

7. **Global scope uses the OS hotkey APIs through `global-hotkey`, only while used.** macOS: Carbon `RegisterEventHotKey` (no Accessibility or Input Monitoring permission needed; no media keys, which would need an event tap). Windows: `RegisterHotKey` with `MOD_NOREPEAT`. Both deliver on the UI thread's event loop, which Slint already runs. The `global-hotkey` crate (tauri-apps, MIT or Apache-2.0, from the same project as `muda` and `tray-icon`, which RBS already builds) is a dependency of `rbs-app` only, for macOS and Windows targets only, behind a small `GlobalKeys` trait so tests use a fake. The manager is created when the first global binding exists and dropped when the last one goes (P13): with no global bindings nothing is registered and nothing runs. While Settings › Shortcuts records a chord, global chords are unregistered, so a chord bound globally can be recorded again, and registered again afterwards.
   **A global shortcut acts like a control surface**: it runs the command through the same path as `ui.command.run` (ADR-0050 decision 2), without the local confirmation, because the user pressed a key they bound for exactly that and the window may be hidden. So a global shortcut on Go live / End stream ends the stream without asking. The editor says so beside every global binding of a command whose off half asks ("Ends the stream without asking."), and no default is global.
   Known limitation on macOS: letter and punctuation keys of global chords are registered by their position on a US keyboard (Carbon takes key codes); on QWERTY-like layouts, including Turkish Q, letters match. Windows maps letters by the active layout. Mapping through the active layout on macOS is a follow-up.

8. **Settings › Shortcuts.** A Settings section after Appearance, in the same visual language (the Appearance page and the mockup's key chips):
   - A search field that matches command titles and chord text, and Reset all (asks first: "Reset every shortcut to its default?").
   - Commands grouped by menu (File, Edit, View, Scene, Output, Window), one row per command and per picker choice ("Scene: Game"), each with its chords as key chips; a global binding's chip carries a "Global" tag. Per row: Add, a remove button per chip, and Reset when the row differs from its defaults.
   - Add records a chord inline: "Press the keys. Escape cancels." with a "Works when RBS is in the background" checkbox for global scope. The chord is checked at once (decision 5) and the reason is shown beside it; Replace and Cancel appear for a chord in use.
   - Rows show problems in the error style: conflicts, unavailable commands, chords another app uses, reserved chords from a hand-edited file.
   - Every change is saved at once (one write) and applied at once; there is no Save button, as in General.

9. **Layers.** The chord, the keymap, defaults, conflicts, reserved lists and the document body are toolkit-free in `rbs_ui::shortcut` (`rbs-ui` keeps depending only on `serde` and `toml`). `rbs-user-data` implements `Document` for the shortcuts document and `UserData::shortcuts()` / `set_shortcuts()`. `rbs-app` maps Slint key events to chords, runs commands, owns the global hotkey adapter and the page. No core or port crate changes. Public contract added: the chord grammar, the shortcuts document and the scene commands below (command ids are already public, ADR-0050 decision 2).

10. **Scene commands join the registry** (Scene menu): `scene.select` (picker over the canvas's scenes in panel order, parameter `id`, the stable scene id; runs `scene.switch`), `scene.position` (action, parameter `n` from 1, the scene at that position; for position defaults), `scene.next` and `scene.previous` (around the program scene, wrapping). All are live switches, as in the Scenes panel.

## Consequences

- Every command, including each scene, workspace, profile and panel choice, can get a key in the app or globally; Stream Deck-like setups work without extra software.
- A global shortcut can end a stream without asking. That is the explicit, documented price of hotkeys that work while a game has the focus; the editor shows it on each such binding.
- `rbs-app` gains `global-hotkey` 0.9 and, through it, `keyboard-types` 0.8, `crossbeam-channel`, `once_cell` and `thiserror` 2 on macOS and Windows. Its `unsafe` code is the crate's own; RBS adds none.
- Menus do not show shortcuts yet: Slint's `MenuItem.shortcut` is a real accelerator that would take Cmd+Z from a focused text field. Showing them needs a display-only menu text or a toolkit change; that is a follow-up.
- New work: `rbs_ui::shortcut` with unit and property tests (canonical text round trip; keymap resolution; conflicts; reserved lists for both platforms), the document with conformance tests, the shell's key mapping and dispatch tests, the global adapter with a fake in tests, the page with light, dark and high-contrast screenshots and the `ui-review` skill. Hardware checks on macOS and Windows: a global chord while another app has the focus and while the main window is hidden.
- Follow-ups: shortcuts through the control API and MCP (`ui.shortcut.list/set`); a command palette showing chords; macOS layout mapping; Linux global shortcuts (portal `GlobalShortcuts`) when a Linux machine is available; hold-to-act bindings (push-to-talk) that need key release.

## Alternatives considered

- **Own FFI for `RegisterHotKey` and Carbon**: about 200 lines of `unsafe` in RBS for what a maintained crate from a project RBS already depends on does.
- **A CGEventTap on macOS** (sees every key, push-to-talk possible): needs the Input Monitoring permission and runs on every key press system-wide; against P13 for a feature most users never turn on.
- **Store the full keymap in the file**: simple, but new defaults would never reach existing users and every release would need a migration.
- **Physical key codes instead of characters**: would make chords layout-independent on every platform, but Slint 1.18 reports text, not key codes, so app scope cannot see them.
- **Confirm global End stream by bringing the window to the front**: would pull a hidden RBS over a game at the worst moment; a clear warning on the binding is the lesser cost.
- **Slint `MenuItem.shortcut` as the app scope engine**: native accelerators fire before text fields and exist only for menu items, so per-choice bindings (scenes) and text field undo would break.

## Review

Pending: independent AI session per ADR-0014.
