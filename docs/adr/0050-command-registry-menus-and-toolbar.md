# ADR-0050: One command registry for menus, context menus, the toolbar and the palette

- Status: Proposed
- Date: 2026-11-06
- Deciders: Salih (owner)
- Builds on: ADR-0008 (UI as an API client, U20), ADR-0017 and ADR-0049 (workspaces, main window frame), ADR-0022 (Slint)

## Context

The owner chose a main window with a menu bar that holds every command and a customisable toolbar (design proposal round 2, option E; ADR-0049), so that new features never crowd the top of the window. Today each UI command is wired by hand where it appears: the app builds its menus from `PopupWindow`s, the command palette and shortcut map are planned separately, and U20 requires every UI command to be a control API command so that Stream Deck, MIDI and hotkeys can run it.

Slint 1.18.1 provides `MenuBar`, `Menu`, `MenuItem` (title, icon, shortcut, checkable, enabled) and `ContextMenuArea`. On macOS and Windows the menu bar is native (through `muda`); on macOS the system menu bar shows the menus of the focused window only (seen in the dock spike, 2026-11-06). On Linux Slint draws the menu bar itself without keyboard or accessibility support (ADR-0049, decision 8).

## Decision

1. **One registry.** `rbs-ui` holds a command registry. Each entry has:
   - an id (grammar below);
   - a title and a tooltip (Fluent messages), an icon from the icon set, and an optional default shortcut;
   - its menu and group (File, Edit, View, Scene, Source, Output, Tools, Window, Help);
   - a kind: `action` (runs one control API command), `toggle` (a pair of control API commands for on and off, plus the state that says which applies, for example `stream.toggle` = `stream.start` / `stream.stop`) or `picker` (opens a list of choices, each running an action with a parameter: `ui.workspace.select` runs `ui.workspace.switch(name)`, `transition.pick` picks a transition);
   - how to read whether it is enabled (with the reason when it is not) and, for toggles, whether it is on.
   The menu bar, context menus, toolbar, command palette and shortcut map are all built from the registry. A command added once appears in all of them; a toolbar button is never added for a feature by default.
2. **Ids.** A command id has the panel id grammar (ADR-0017): `<namespace>.<name>` in lowercase ASCII letters, digits, `.`, `-` and `_`, at most 128 bytes, with optional parameters `?key=value&...`, keys unique and stored sorted. Built-in ids are control API method names or, for toggles and pickers, a name in the same namespace ending in `.toggle` or naming the choice (`.select`, `.pick`). An id's parameters are the command's parameters: `ui.area.toggle?area=left` runs `ui.area.toggle {area: "left"}`, values converted by the types the entry declares. Values are UTF-8, percent-encoded outside the id characters (`ui.workspace.switch?name=Live%20show`); the 128-byte limit counts the encoded form. Client-scoped UI commands use `ui.*` (control API spec). Plugin commands use `<plugin-id>.<name>` and are declared in the plugin manifest once the plugin SDK has commands; until then only built-in commands exist. Ids are a public contract (they are stored in workspace files and used by control surfaces): renaming one keeps the old id as an alias for at least one major version. Control surfaces run any registry id, `.toggle` and picker ids included, with the client-scoped `ui.command.run {id}` and read the registry with `ui.command.list` (control API spec); `ui.command.run` acts as the menu item would but without the local confirmation, because the caller is the one asking (as `stream.stop` does today), and never with more rights than the caller: the routed request carries the caller's scopes and whether it is remote, the methods the entry resolves to are sent as requests of the caller's session, never the UI client's, and are checked as such (`scope_denied`); `ui.command.list` marks entries the caller cannot run.
3. **Toggles keep their safety rules.** The off half of a toggle is the same command with the same rules as elsewhere: End stream and Stop recording ask for confirmation while live (design system spec, "Actions that cannot be undone"), and the toolbar draws the stream and record toggles with the live controls' button styles (the fixed live colour while running, U12).
4. **Menus.** The menu bar is Slint's `MenuBar`; context menus are Slint's `ContextMenuArea`. Every RBS window that can take focus (main and floating windows; projectors take none, U19) declares the same menu bar, so the macOS system menu bar never goes empty. On Windows and Linux only the main window draws it. Menus show shortcuts, checkmarks for toggles and disabled items with their reason as a tooltip. The app's hand-made command menus are replaced by these.
5. **Toolbar.** A workspace may store a toolbar (`toolbar` in format 2, ADR-0049): `style` (`icons` or `icons-text`), `visible`, and `items`, a list of command ids, separators `"|"` and at most one flexible space `"~"`; at most 64 items; a command id appears at most once; leading, trailing and repeated separators are removed on load. Items that do not fit go into a » overflow menu at the end (U11). Unknown ids (a missing plugin) are kept and drawn as a disabled placeholder whose tooltip names what is missing (Fluent message `toolbar-unknown-command`). A Customize toolbar dialog (View menu, or the toolbar's context menu) adds commands from a searchable list, removes and reorders them by pointer or keyboard, adds separators, picks the style and resets to the workspace's default.
6. **Core flows do not depend on the toolbar (U1).** U1 forbids starting a core task from a menu bar menu, so every core flow can be started from a panel or a frame element that cannot be removed, never from the toolbar or a menu alone (transition from the Transition panel or studio rail, Go live and Record from Live controls). Each built-in workspace declares the U1 flows it serves (Edit and Live all of them; Edit therefore has Live controls), and a test checks every declared flow with the toolbar hidden, within the Core flows limits (opening an edge tab counts as one interaction). Removing a toolbar item for a core flow shows a one-line note in the Customize dialog saying where the command still is.

## Consequences

- The command registry becomes the single list that the U20 test checks: every registry entry maps to control API commands (methods listed by `api.describe` or `ui.*` methods), every registry id runs through `ui.command.run`, and every control API command a user can run from the UI has an entry.
- Menus come from Slint, which removes our hand-made menu code; on Windows the native menu bar follows the OS theme rather than ours. If that looks out of place next to a custom theme, a shell setting can draw the menu bar with Slint and our tokens instead (no model change).
- The control API gains `ui.command.list`, `ui.command.run`, `ui.toolbar.get/set/reset` and `ui.area.toggle(area)`, all client-scoped. `stream.toggle` and `record.toggle` are registry ids, not engine methods: they resolve to `stream.start`/`stream.stop` and `record.start`/`record.stop`, and surfaces reach them through `ui.command.run`.
- The plugin SDK will need a command declaration (id, title, icon, kind) before plugins can add menu items or toolbar buttons; plugins can never add a toolbar button by default (U14 spirit: plugins cannot place UI in the frame on their own).

## Alternatives considered

- **Wire each command where it appears** (today's approach): every new surface (palette, toolbar, menus, Stream Deck) repeats the list and drifts.
- **An app menu button instead of a menu bar** (round 2, option G): one row less on Windows and Linux, but commands are one level deeper and macOS users expect the system menu bar.
- **A fixed toolbar**: simpler, but it is exactly what grows crowded as features arrive, which the owner wants to avoid.

## Review

- 2026-11-06, independent AI session (second round of the ADR-0049 review, which covered this ADR): changes requested on decision 6 (U1 forbids menu-only starts; Edit lacked Live controls) and on decisions 1-2 (toggle ids unreachable for surfaces, parameter mapping, picker items, `ui.toolbar.*` undefined). Resolved: decision 6 rewritten, `picker` kind, id parameter mapping, `ui.command.run` / `ui.command.list`, `ui.toolbar.*` listed in the control API spec.
- 2026-11-06, third independent AI session (verification): `ui.command.run` must not widen a token's rights (now checked per resolved method, `scope_denied`), parameter values need an encoding (percent-encoded UTF-8), the U1 workspace test must count edge tabs. Resolved in decisions 2 and 6 and the specs.
- 2026-11-06, fourth independent AI session (final check): the scope check needed a mechanism (resolved: resolved methods run as the caller's session, decision 2) and the studio toggle needed real methods and a canvas (resolved in ADR-0049 and the spec).
