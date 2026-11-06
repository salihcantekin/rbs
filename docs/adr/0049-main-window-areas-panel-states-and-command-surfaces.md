# ADR-0049: Main window areas, panel states and command surfaces (workspace format 2)

- Status: Proposed
- Date: 2026-11-06
- Deciders: Salih (owner)
- Builds on: ADR-0017 (flexible panels and workspaces), ADR-0015 (UI design rules), ADR-0022 (Slint), ADR-0013 (zero cost when unused), ADR-0008 (UI as an API client)

## Context

The owner reviewed main window alternatives (design proposal round 1 and round 2, 2026-11-06) and chose "Workbench with area toggles": a menu bar that holds every command, a customisable toolbar under it, docked panels that can auto-hide to a window edge or collapse to their header, three toolbar buttons that show or hide the left, right and bottom areas, and divider sizes that can be dragged and are kept. The transition controls in studio mode become a narrow rail that can be widened or collapsed to a floating control of zero width.

The owner also asked for these reasons:

- Screen space: a panel the user wants to keep must be shrinkable to almost nothing without closing it (Visual Studio's auto-hide, collapse and expand).
- A calm top area: new features must not make the top bar crowded; toolbars must be customisable (Visual Studio) or commands moved to menus.
- Sizes that matter, such as the transition column, must be adjustable and saved, never fixed widths that shrink the preview.
- Maintenance: use what the framework provides wherever it can; every piece we write ourselves is a long-term cost.

What the workspace model (ADR-0017, format 1) has today: each window is a split tree of tab stacks with ratios, plus floating windows and projectors, a lock flag and layout undo. It has no notion of areas, no hidden or collapsed state, no sizes in pixels and no toolbar.

What Slint 1.18.1 (the version we use) provides, checked in its source and with the spike `crates/rbs-app/examples/dock_spike.rs`:

- `MenuBar`, `Menu`, `MenuItem` (shortcuts, checkable items, icons), `MenuSeparator`: native menu bar on macOS and Windows through `muda`, drawn by Slint elsewhere. On macOS (M4 Pro, 2026-11-06) the system menu bar shows the window's menus only while that window has focus: with a second window focused, it showed no View or Window menu.
- `ContextMenuArea`: context menus on any element.
- `DragArea` / `DropArea` with a `data-transfer` payload and the pointer position in the drop target: works inside one window. The winit backend does not implement native drags (`WindowAdapter::start_drag` returns `false`), so a drag cannot reach another window or leave the window.
- A pressed `TouchArea` keeps receiving pointer moves and the release outside the window (seen on X11; macOS and Windows to be confirmed on hardware), which is enough to tear a panel off and to move it to another RBS window by hit-testing screen positions ourselves.
- `ToolTip`, `PopupWindow`, `Dialog`, several `Window`s, `always-on-top`, `WindowMoveArea`.
- No splitter, no docking system. No docking library exists for Slint; the Rust docking crates found belong to other toolkits.

## Decision

1. **Main window structure.** The main window is a frame around a centre and three areas:
   - Frame (top to bottom): menu bar (in the system menu bar on macOS), toolbar, the window content, status bar. The live indicator stays in the frame as required by U4 and U21: at the right of the menu bar on Windows and Linux, at the right of the toolbar on macOS.
   - Content: `left`, `right` and `bottom` areas around a `centre`. Each area and the centre holds a split tree of tab stacks as in format 1. An area has a size in logical pixels (its width, or its height for `bottom`), so it keeps its size when the window is resized and the centre takes the rest. The bottom area spans the width under the left area, the centre and the right area.
   - Area toggles: View menu items, shortcuts and three toolbar buttons show or hide an area. A hidden area keeps its contents and size; showing it restores both. "Hide all side panels" hides all three.
   - Floating windows and projectors are unchanged (format 1).
2. **Panel states.** Every tab stack in an area or the centre is in one of three states:
   - `docked` (default): as today.
   - `collapsed`: only the stack's title row is shown (the `size.control.lg` height plus border); the other stacks of its split take the space. Expanding gives it back the share it had. A collapsed panel may declare a compact view that is still drawn in the title row (the mixer shows one small meter and a mute button per source); panels without one show only their titles.
   - `auto-hide`: the stack leaves the split tree and becomes tabs on a window edge (the edge of its area, or the nearest edge for the centre). Activating an edge tab (click, Enter, hover after `motion.hover-open`, default 400 ms) slides the panel out over the content at its remembered size; it slides back when focus or the pointer leaves it, on Escape, or when another edge tab opens. Pinning it docks it back where it was: next to the same anchor panel (the panel it shared a stack or split with), else at the outer side of its area.
   - **Maximize** (double-click a tab, or Shift+Esc) shows one panel over the whole content until it is restored. It is a transient view state and is not saved.
   - State changes are layout commands (ADR-0017): undoable in the layout history, refused by pointer when the workspace is locked, and never touching sources or outputs. Guarded panels (`rbs.program`, `rbs.live-controls`, `rbs.mixer`) may be collapsed or auto-hidden while live without the undo toast, because they stay one interaction away; closing them still shows it.
3. **Sizes and limits.** Each panel declares `min`, `default` and `max` sizes along both axes in logical pixels (`max` may be a share of the window). Every divider, between stacks and at area edges, can be dragged within those limits, moves by keyboard as today (`step.nudge`), and returns to the default on double-click. View › Reset panel sizes resets every size in the workspace. Sizes are saved with the workspace.
   - The transition panel in studio mode (`rbs.transition`, docked between preview and program) declares `min` 0, `default` 56, `max` 200. Below 40 px it is drawn as a floating control over the bottom edge of the two views instead of a column; above 120 px it shows names and the duration field. The rail and the floating control are two renderings of the same panel, chosen by its size.
4. **Commands, menus and the toolbar.**
   - `rbs-ui` holds one command registry: id (the control API command name, ADR-0008, U20), title, icon, default shortcut, the menu it belongs to, and how to read its enabled and checked state. The menu bar, context menus, toolbar, command palette and shortcut map are built from it, so a command added once appears everywhere and a feature never needs a new button in everyone's toolbar.
   - The menu bar is Slint's `MenuBar`. Context menus are Slint's `ContextMenuArea`. Every RBS window that can take focus (main and floating windows; projectors take none, U19) declares the same menu bar from the registry, because on macOS the system menu bar shows the focused window's menus; on Windows and Linux only the main window draws it. The app's hand-made `PopupWindow` menus are replaced by them where they show commands.
   - The toolbar is a list of command ids, separators (`"|"`) and one flexible space (`"~"`), stored in the workspace, so each workspace can have its own toolbar. Items that do not fit go into an overflow menu at the end; nothing is cut off (U11). A Customize toolbar dialog adds, removes and reorders items, chooses "icons" or "icons and text", and resets to the workspace's default. Unknown ids (an uninstalled plugin) are kept and shown as a placeholder, as for panels.
5. **Dragging.**
   - Moving a tab within one window uses Slint's `DragArea` and `DropArea`. Drop targets (the five-way guide on a stack and the four window-edge targets) and the area a drop will fill are computed by a pure function in `rbs-ui` from the layout and the pointer position, and drawn by the shell.
   - Tearing a panel off and moving it to another RBS window use pointer capture on a `TouchArea`: the shell converts the pointer to screen coordinates, `rbs-ui` hit-tests the known window rectangles, and the result is the same layout command (float, dock into window N at target T). If Slint gains native or cross-window drags later, the shell switches to them without changing `rbs-ui`.
   - Floating windows snap to screen edges and to each other within 12 px; holding Alt (Option) turns snapping off for that drag.
6. **Workspace format 2.** These additions make a new format version. RBS reads format 1 and migrates it on load: the whole format 1 main window tree becomes the `centre`, the three areas start empty and visible, no stack is collapsed or auto-hidden, and the toolbar is the default one. Built-in workspaces are redefined in format 2. A format 2 file is refused by older builds (ADR-0017 rule: never rewritten). The exact keys are in the [design system spec](../specs/ui-design-system.md#workspace-file).
7. **Zero cost (P13, U5, U21).** Collapsed panels draw only their title row (and a declared compact view at its own rate); auto-hidden panels and panels in hidden areas do no rendering and no polling until they are shown, and plugin panels among them go to Standby. A slid-out auto-hide panel counts as visible while it is out.

## Consequences

- The workspace model in `rbs-ui` grows: areas with pixel sizes, stack states, edge tab groups with anchors, size limits per panel, toolbar items, and the drop-target function. All of it is plain Rust with unit and property tests (round trip, migration from format 1, command results always valid). The JSON Schema promised by ADR-0017 is written with format 2.
- The Slint shell gets simpler in one place (menus come from Slint) and larger in another (areas, edge tabs, flyouts, collapsed title rows, drop guide). The drawing stays thin; decisions are made in `rbs-ui`.
- The control API's `ui.*` commands gain `ui.area.show/hide`, `ui.panel.collapse/expand`, `ui.panel.auto_hide/pin`, `ui.panel.maximize/restore`, `ui.toolbar.*` and `ui.layout.reset_sizes`, addressed by panel id (see the control API spec's open question on stable addressing).
- macOS: the menu bar moves to the system menu bar, giving back its height. Windows: the native menu bar follows the OS theme, not ours; if that looks out of place next to a custom theme, Slint can draw it with our tokens instead (a shell setting, no model change).
- Tear-off across windows relies on pointer events outside the window while a button is held; this must be confirmed on macOS and Windows hardware before the floating part ships. If it fails on a platform, tearing off falls back to the panel menu's Float command there.
- Usability tests (U1, U21) gain a task: hide the mixer to an edge and bring it back.

## Alternatives considered

- **Keep format 1 and express areas as conventions in the free split tree** (outermost children touching an edge). No migration, but area toggles, pixel sizes and "dock back where it was" become guesses about tree shapes, and any drag can silently break an area.
- **Round 2 option F (fixed areas only, no free docking).** Simplest to learn, but drops U21's free docking that power users need.
- **Round 2 option G (app menu button and quick-access bar instead of a menu bar).** One row less on Windows, but commands are one level deeper and macOS users expect the system menu bar.
- **Write our own native drag and drop per platform now** (NSDraggingSession, OLE DoDragDrop) for cross-window tab drags. More platform code to maintain for a gesture pointer capture already covers; reconsider if Slint does not add it and capture proves unreliable.
- **A third-party docking toolkit** (Qt Advanced Docking System, Makepad's dock): would mean a second UI toolkit next to Slint; rejected with ADR-0022.

## Review

Pending: independent review per ADR-0014.
