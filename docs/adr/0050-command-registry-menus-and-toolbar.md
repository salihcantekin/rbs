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
   - a kind: `action` (runs one control API command) or `toggle` (a pair of control API commands for on and off, plus the state that says which applies, for example `stream.toggle` = `stream.start` / `stream.stop`);
   - how to read whether it is enabled (with the reason when it is not) and, for toggles, whether it is on.
   The menu bar, context menus, toolbar, command palette and shortcut map are all built from the registry. A command added once appears in all of them; a toolbar button is never added for a feature by default.
2. **Ids.** A command id has the panel id grammar (ADR-0017): `<namespace>.<name>` in lowercase ASCII letters, digits, `.`, `-` and `_`, at most 128 bytes, with optional parameters `?key=value&...`, keys unique and stored sorted. Built-in ids are control API method names or, for toggles, a name in the same namespace ending in `.toggle`. Client-scoped UI commands use `ui.*` (control API spec). Plugin commands use `<plugin-id>.<name>` and are declared in the plugin manifest once the plugin SDK has commands; until then only built-in commands exist. Ids are a public contract (they are stored in workspace files and used by control surfaces): renaming one keeps the old id as an alias for at least one major version.
3. **Toggles keep their safety rules.** The off half of a toggle is the same command with the same rules as elsewhere: End stream and Stop recording ask for confirmation while live (design system spec, "Actions that cannot be undone"), and the toolbar draws the stream and record toggles with the live controls' button styles (the fixed live colour while running, U12).
4. **Menus.** The menu bar is Slint's `MenuBar`; context menus are Slint's `ContextMenuArea`. Every RBS window that can take focus (main and floating windows; projectors take none, U19) declares the same menu bar, so the macOS system menu bar never goes empty. On Windows and Linux only the main window draws it. Menus show shortcuts, checkmarks for toggles and disabled items with their reason as a tooltip. The app's hand-made command menus are replaced by these.
5. **Toolbar.** A workspace may store a toolbar (`toolbar` in format 2, ADR-0049): `style` (`icons` or `icons-text`), `visible`, and `items`, a list of command ids, separators `"|"` and at most one flexible space `"~"`; at most 64 items; a command id appears at most once; leading, trailing and repeated separators are removed on load. Items that do not fit go into a » overflow menu at the end (U11). Unknown ids (a missing plugin) are kept and drawn as a disabled placeholder whose tooltip names what is missing (Fluent message `toolbar-unknown-command`). A Customize toolbar dialog (View menu, or the toolbar's context menu) adds commands from a searchable list, removes and reorders them by pointer or keyboard, adds separators, picks the style and resets to the workspace's default.
6. **Core flows do not depend on the toolbar (U1).** Every core flow of U1 can be started from a panel or the menu bar without the toolbar (transition from the Transition panel, workspace from View › Workspace, Go live and Record from Live controls). Removing a toolbar item for a core flow shows a one-line note in the Customize dialog saying where the command still is.

## Consequences

- The command registry becomes the single list that the U20 test checks: every registry entry maps to control API commands, and every control API command a user can run from the UI has an entry.
- Menus come from Slint, which removes our hand-made menu code; on Windows the native menu bar follows the OS theme rather than ours. If that looks out of place next to a custom theme, a shell setting can draw the menu bar with Slint and our tokens instead (no model change).
- The control API gains the toggle commands' pairs where missing (`ui.area.toggle(area)` is client-scoped; `stream.toggle` and `record.toggle` resolve to existing start and stop commands in the UI, they are not new API methods).
- The plugin SDK will need a command declaration (id, title, icon, kind) before plugins can add menu items or toolbar buttons; plugins can never add a toolbar button by default (U14 spirit: plugins cannot place UI in the frame on their own).

## Alternatives considered

- **Wire each command where it appears** (today's approach): every new surface (palette, toolbar, menus, Stream Deck) repeats the list and drifts.
- **An app menu button instead of a menu bar** (round 2, option G): one row less on Windows and Linux, but commands are one level deeper and macOS users expect the system menu bar.
- **A fixed toolbar**: simpler, but it is exactly what grows crowded as features arrive, which the owner wants to avoid.

## Review

Pending: independent review per ADR-0014.
