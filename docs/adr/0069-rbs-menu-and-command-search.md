# ADR-0069: The RBS menu: an app menu with command search, and the menu bar only on macOS

- Status: Proposed
- Date: 2026-11-07
- Deciders: Salih (owner)
- Amends: ADR-0050 decision 4 (menus) and its rejected alternative "An app menu button instead of a menu bar"
- Builds on: ADR-0049 (frame and areas), ADR-0054 (preview stays visible behind popups), ADR-0060 (shortcuts), ADR-0013 (zero cost when unused)

## Context

ADR-0050 put every command in a menu bar: the macOS system menu bar, a native Win32 menu bar row on Windows, and a row drawn by Slint on Linux. It rejected round 2's option G (an app menu button) because commands are one level deeper and macOS users expect the system menu bar.

On 2026-11-07 the owner looked at option G's app menu again (the mockup in the round 2 proposal, Alternative G "Control room") and asked for it, as close to the mockup as possible: an RBS button that opens a flyout with a search field, the menus as a column of categories, the chosen menu's items in titled sections with icons, shortcuts, checkmarks and submenus, the output profiles as chips and Settings. He approved the plan before it was built ("değişiklikleri yapabilirsin onaylıyorum", 2026-11-07), including that the Windows and Linux menu bar row gives way to it. Two things in ADR-0050 stand in the way:

- Decision 4 says every window declares the menu bar and the main window draws it on Windows and Linux.
- The palette (Ctrl/Cmd+K, ui-design-system spec) had no home yet.

On Windows the native menu bar follows the OS theme, not ours (ADR-0050's own consequence), and on Linux the drawn row has low contrast; the RBS menu is drawn with our tokens on every platform.

## Decision

1. **The RBS menu.** The frame's toolbar row starts with the RBS button (`menu` icon and "RBS"), which is part of the frame, not a toolbar item: it cannot be removed, so every command stays reachable whatever the workspace's toolbar holds. It opens the RBS menu, a popup drawn with design tokens:
   - a search field ("Type a command, setting or panel");
   - a column of categories, one per menu of `MENU_LAYOUT`, in menu bar order (empty menus left out);
   - the chosen menu's items, with the section headings `MENU_LAYOUT` gives (decision 3), each item with its registry icon (or its checkmark while checked), title, bound shortcut or a picker's current choice, and `›` on pickers, whose choices open in the same column with a Back row;
   - the output profiles as chips (`profile.select`'s choices) and Settings (`ui.settings.open`).
   Picking an item runs its registry id as the menu bar does, with the same confirmations. A disabled item shows why under the list while it is the current row (ADR-0050 decision 4's reason, which native menus could not show).
2. **Command search is the RBS menu's search field.** A new registry entry `ui.app_menu.open` ("Search commands", Help menu, icon `search`, default shortcut Ctrl/Cmd+K, client-scoped control API method of the same name) opens the menu with the field focused. A query matches every command whose menu and title hold all its words, each picker choice as "Picker: Choice" and each Settings section as "Settings: Section", in menu order, at most 40.
3. **Section headings in `MENU_LAYOUT`.** A `#Heading` entry starts a titled section. The RBS menu shows the title; the menu bar draws it as a separator (none at a menu's start). Separators and headings that would lead, end or double are dropped.
4. **The menu bar only on macOS.** Main and floating windows keep declaring the generated menu bar, shown only where the OS has a system menu bar (macOS, `Shell.native-menu-bar`). On Windows and Linux there is no menu bar row; the RBS menu takes its place. This replaces ADR-0050 decision 4's "On Windows and Linux only the main window draws it".
5. **Zero cost when unused.** The menu's rows are built from the registry when it opens or its view changes (category, picker, query), never while it is closed; no state is kept besides the last category.
6. **Preview.** The RBS menu is an in-window popup, which the preview layer already treats as a cover (ADR-0054).

## Consequences

- One row less on Windows and Linux, and one look on every platform. Windows users lose Alt-key access to a native menu bar; Ctrl+K and the button (Tab-reachable, Return, Space or Down opens it) replace it.
- Commands are one click deeper than in a menu bar on Windows and Linux, as ADR-0050 warned; the search field and shortcuts offset it, and core flows never depended on menus (U1).
- macOS keeps the system menu bar and gains the RBS menu as well.
- The control API gains the client-scoped `ui.app_menu.open` (in `UI_METHODS`).
- Tests: rows, sections, pickers, search and profile chips are unit-tested from a fake state (`rbs-app` `app_menu.rs`); the registry tests check headings and icons.

## Alternatives considered

- **Keep the menu bar on Windows and Linux and add the RBS menu beside it**: two ways to the same commands in one window, and the row the owner wanted gone stays.
- **A separate command palette dialog**: a second list of the same commands; the mockup puts search at the top of the menu.
- **Draw the RBS menu as a native menu**: native menus cannot hold a search field, sections with headings or chips.

## Review

Independent review per ADR-0014: pending.
