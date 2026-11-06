# ADR-0049: Main window areas, panel states and sizes (workspace format 2)

- Status: Proposed
- Date: 2026-11-06
- Deciders: Salih (owner)
- Amends: ADR-0015 (rule U5, collapsed panels), ADR-0017 (workspace format)
- Builds on: ADR-0022 (Slint), ADR-0013 (zero cost when unused), ADR-0008 (UI as an API client)
- Related: ADR-0050 (command registry, menus and toolbar)

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
- A pressed `TouchArea` keeps receiving pointer moves and the release outside the window on X11, which is enough to tear a panel off and to move it to another RBS window by hit-testing screen positions ourselves. On macOS (M4 Pro, 2026-11-06, hand test of the spike) drags inside the window dropped on the right stack and target, and while the grip was held the pointer kept being reported outside the window on three sides (down to -121 px left, -72 px above, 75 px below), with the release caught outside. Windows is unchecked.
- On Linux, Slint draws the menu bar itself (`MenuBarImpl`); its items have no keyboard focus, key handling or accessible role (only the opened popup has). On macOS and Windows the menu bar is native and gets the OS's keyboard and screen reader support.
- `ToolTip`, `PopupWindow`, `Dialog`, several `Window`s, `always-on-top`, `WindowMoveArea`.
- No splitter, no docking system. No docking library exists for Slint; the Rust docking crates found belong to other toolkits.

## Decision

1. **Main window structure.** The main window is a frame around a centre and three areas:
   - Frame (top to bottom): menu bar row, toolbar row, the content, status bar. On macOS the menus are in the system menu bar and the menu bar row does not exist.
   - **Live indicator** (U4, U21): always at the right end of the first frame row of the window, which can never be hidden: the menu bar row on Windows and Linux, the toolbar row on macOS. Hiding the toolbar (View › Toolbar) hides its items but on macOS keeps that row as a slim strip with the indicator, so the indicator is in the same place in every workspace whatever the toolbar setting.
   - Content: `left`, `right` and `bottom` areas around a `centre`. Each area and the centre holds a split tree of tab stacks as in format 1. An area has a size in logical pixels (its width, or its height for `bottom`), so it keeps its size when the window is resized and the centre takes the rest. The bottom area spans the width under the left area, the centre and the right area. An area that is visible but empty takes no space.
   - Area toggles: View menu items and three toolbar buttons show or hide an area; no shortcut is bound by default (U21: Ctrl+Alt combinations clash with AltGr). A hidden area keeps its contents and size; showing it restores both. "Hide all side panels" hides all three. Edge tabs of auto-hidden panels stay visible when their area is hidden.
   - Floating windows and projectors are unchanged (format 1). In a floating window stacks can collapse but not auto-hide (it has no areas); the panel menu does not offer auto-hide there.
   - With every area hidden and an empty centre, the centre shows an empty state with "Show areas" and "Reset layout" (U10).
2. **Panel states.** Every tab stack in an area or the centre is in one of three states. Commands name a panel (`ui.panel.collapse(id)`); they act on the stack that holds it.
   - `docked` (default): as today.
   - `collapsed`: only the stack's title row is shown: `size.control.lg` plus border high in a vertical split, a vertical title strip as wide in a horizontal split. The other stacks of its split take the space. The collapsed child keeps its ratio, which is used again on expand. If every stack of a split is collapsed, the split shrinks to its title rows; if every stack of an area is collapsed, the area shrinks to its title rows and keeps its `size` as the size to restore. The centre's tree keeps at least one stack that is not collapsed.
   - **Compact view (amends U5).** A panel may declare a compact view drawn in its title row while collapsed, with its own declared update rate; it follows the same visibility rules as a full panel (no rendering while its window, area or flyout is hidden). `rbs.mixer` declares one: one small meter and a mute button per audio source, at 30 Hz while audio is active. U5's "collapsed panels do no rendering and no polling" becomes "collapsed panels render only their declared compact view". Panels without one show only their titles and do no rendering or polling.
   - `auto-hide`: the stack leaves the split tree and becomes tabs on a window edge (the edge of its area, or the nearest edge for the centre). Hovering an edge tab for `motion.hover-open` (400 ms) slides the panel out without taking focus; clicking it, or Enter or Space on a focused edge tab, slides it out and moves focus into it. It stays out while the pointer is on the tab or the panel, with a 300 ms grace period after the pointer leaves both (WCAG 1.4.13), and slides back on Escape, when focus leaves it, or when another edge tab opens; focus then returns to the edge tab. Edge tabs have the role tab and report expanded or collapsed. Pinning the panel docks it back next to its anchor panel (the panel it shared a stack or split with when it was auto-hidden) if that panel is still docked, else at the outer side of its area.
   - **Maximize** (double-click a tab, or Shift+Esc) shows one panel over the whole content until restored. It is a transient view state: not saved and not a step in the layout history (neither is sliding an auto-hide panel out or back). Panels covered by a maximized panel count as hidden for U5.
   - Collapse, auto-hide, pin and area show or hide are layout commands (ADR-0017): undoable in the layout history and never touching sources or outputs. When the workspace is locked, auto-hide and pin are refused from the pointer like other rearranging commands; collapse, expand, area show or hide, maximize and sliding a panel out stay allowed, because they hide or show without rearranging. Guarded panels (`rbs.program`, `rbs.live-controls`, `rbs.mixer`) may be collapsed, auto-hidden, covered by maximize or hidden with their area while live without the undo toast, because one interaction brings them back; closing them still shows it.
3. **Sizes and limits.** Each panel declares `min`, `default` and `max` sizes along both axes in logical pixels (`max` may be a share of the window). Every divider, between stacks and at area edges, can be dragged within those limits, moves by keyboard as today (`step.nudge`), and returns to the default on double-click. View › Reset panel sizes resets every size in the workspace. Sizes are saved with the workspace.
   - **When limits conflict** (small window, 200 % text), space is resolved in this order: the centre's minimum is kept first; then the visible areas shrink in proportion down to their minimums; then the bottom area collapses its stacks; then the left and right areas are drawn as their edge tabs (not saved: they come back when there is room). Nothing is cut off (U11). A property test checks this over window sizes from 1280 x 720 upwards and text scales up to 200 %.
   - The transition panel in studio mode (`rbs.transition`, docked between preview and program) declares `min` 0, `default` 56, `max` 200. Below 40 px it is drawn as a floating control over the bottom edge of the two views instead of a column; above 120 px it shows names and the duration field. The rail and the floating control are two renderings of the same panel, chosen by its size.
4. **Commands, menus and the toolbar** are decided in ADR-0050. This ADR only reserves the frame rows and the toolbar's place in the workspace file.
5. **Dragging.**
   - Moving a tab within one window uses Slint's `DragArea` and `DropArea`. Drop targets (the five-way guide on a stack and the four window-edge targets) and the area a drop will fill are computed by a pure function in `rbs-ui` from the layout and the pointer position, and drawn by the shell.
   - **Confirmed on macOS and X11, provisional on Windows until checked on hardware:** tearing a panel off and moving it to another RBS window use pointer capture on a `TouchArea`: the shell converts the pointer to screen coordinates, `rbs-ui` hit-tests the known window rectangles, and the result is the same layout command (float, dock into window N at target T). Where this does not work, the panel menu's Float and Dock commands are the shipped way, and the drag only moves tabs inside a window. If Slint gains native or cross-window drags later, the shell switches to them without changing `rbs-ui`.
   - Floating windows snap to screen edges and to each other within 12 px; holding Alt (Option) turns snapping off for that drag.
6. **Workspace format 2.** These additions make a new format version, fully specified with its validation rules in the [design system spec](../specs/ui-design-system.md#workspace-file). RBS reads format 1 and migrates it in memory on load: the whole format 1 main window tree becomes the `centre`, the three areas start empty and visible, nothing is collapsed or auto-hidden, and the toolbar is the default one. The file is rewritten in format 2 only when the workspace is next saved; from then on older builds refuse it (ADR-0017: never rewritten), a one-way upgrade the release notes must state. A user's format 1 override of a built-in workspace migrates to "everything in the centre", losing the new default look; the app says so once and offers Restore default.
7. **Zero cost (P13, U5, U21).** Collapsed panels draw only their title row and a declared compact view; auto-hidden panels, panels in hidden areas and panels covered by a maximized panel do no rendering and no polling until they are shown, and plugin panels among them go to Standby. A slid-out auto-hide panel counts as visible while it is out. A U21 test counts redraws and polls of collapsed panels with and without a compact view.
8. **Linux menu keyboard access.** Until Slint's drawn menu bar has keyboard and accessibility support, the keyboard route to commands on Linux is the command palette (Ctrl+K) and F10, which opens the same menus as a popup (Slint's popup menus have keyboard handling); this is checked with a screen reader before Linux ships (Linux work is parked, no hardware).

## Consequences

- The workspace model in `rbs-ui` grows: areas with pixel sizes, stack states, edge tab groups with anchors, size limits per panel with the conflict order, and the drop-target function. All of it is plain Rust with unit and property tests (round trip, migration from format 1, command results always valid). The JSON Schema promised by ADR-0017 is written with format 2.
- The Slint shell grows: areas, edge tabs, flyouts, collapsed title rows and the drop guide. The drawing stays thin; decisions are made in `rbs-ui`.
- U5 is amended (decision 2) and U21 gains a bullet; the glossary gains area, edge tab, collapse, auto-hide, pin and maximize.
- The control API's `ui.*` commands gain `ui.area.show/hide/toggle`, `ui.panel.collapse/expand`, `ui.panel.auto_hide/pin`, `ui.panel.maximize/restore` and `ui.layout.reset_sizes`, addressed by panel id (see the control API spec's open question on stable addressing).
- Tear-off across windows relies on pointer events outside the window while a button is held; it works on macOS and X11 and is provisional on Windows (decision 5) until checked there.
- Usability tests (U1, U21) gain a task: hide the mixer to an edge and bring it back.

## Alternatives considered

- **Keep format 1 and express areas as conventions in the free split tree** (outermost children touching an edge). No migration, but area toggles, pixel sizes and "dock back where it was" become guesses about tree shapes, and any drag can silently break an area.
- **Round 2 option F (fixed areas only, no free docking).** Simplest to learn, but drops U21's free docking that power users need.
- **Round 2 option G (app menu button and quick-access bar instead of a menu bar).** One row less on Windows, but commands are one level deeper and macOS users expect the system menu bar.
- **Write our own native drag and drop per platform now** (NSDraggingSession, OLE DoDragDrop) for cross-window tab drags. More platform code to maintain for a gesture pointer capture already covers; reconsider if Slint does not add it and capture proves unreliable.
- **A third-party docking toolkit** (Qt Advanced Docking System, Makepad's dock): would mean a second UI toolkit next to Slint; rejected with ADR-0022.

## Review

- 2026-11-06, independent AI session (given the proposal, specs and code only): changes requested, 14 findings. Resolutions:
  1. Compact view broke U5: this ADR now amends ADR-0015's U5 explicitly (decision 2) with a redraw-count test (decision 7).
  2. Default Ctrl/Cmd+Alt area shortcuts clashed with U21: none bound by default.
  3. macOS live indicator could vanish with the toolbar: it stays in the first frame row, which cannot be hidden (decision 1).
  4. Slint's drawn Linux menu bar has no keyboard or accessibility support: recorded in the context; decision 8 gives the Linux keyboard route.
  5. ADR and spec disagreed on locked workspaces: decision 2 now matches the spec.
  6. Format 2 gaps: types, ranges, clamping, `active`, `anchor`, collapse in horizontal splits, ratios kept, empty areas, edge tabs of hidden areas, fully collapsed areas, panel versus stack addressing: all specified (decisions 1-2, spec). Toolbar ids moved to ADR-0050.
  7. Conflicting size limits: resolution order and a property test (decision 3).
  8. Built-in workspaces drifted from U21 and the example: aligned in the spec; Live keeps Live controls.
  9. Auto-hide accessibility: hover does not take focus, grace period, roles, toast rule stated (decision 2).
  10. Edge cases (floating windows, maximize, history, empty state, placeholder strings): decided (decisions 1-2, spec).
  11. U1 flows starting from removable toolbar items: handled in ADR-0050.
  12. Glossary: terms added.
  13. Split: command registry, menus and toolbar moved to ADR-0050; cross-window tear-off marked provisional (decision 5).
  14. Migration: one-way upgrade and the built-in override loss stated (decision 6).
