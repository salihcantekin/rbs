# ADR-0083: The Mixer orientation is chosen by the user, not only by the panel shape

- Status: Accepted
- Date: 2026-11-09
- Deciders: Salih (owner)
- Refines: [ADR-0078](0078-saved-section-state-and-shared-section-components.md) decision 5 (the Mixer picks its layout from the panel shape)
- Builds on: [ADR-0017](0017-flexible-panels-and-workspaces.md), [ADR-0050](0050-command-registry-menus-and-toolbar.md), [ADR-0080](0080-hide-sources-in-the-mixer.md)
- Spec: [ui-design-system](../specs/ui-design-system.md#workspace-file) (workspace file, Mixer)

## Context

The Mixer draws its sources in one of two arrangements: **rows** (sources stacked top to bottom, horizontal meters) or **columns** (sources side by side, vertical meters and faders). Today the arrangement follows the panel shape (ADR-0078 decision 5): a wide, low panel draws columns, a narrow or tall one draws rows. The owner (2026-11-09) wants to decide this himself with an option in the Mixer's right-click menu, because the panel shape is a side effect of the layout (a panel docked at the bottom is wide and low even when the user wants rows), and a size-driven switch can flip while the user resizes the window.

Forces: the choice is view state, not scene or engine state (P8); the Mixer already has a right-click menu (Hide All, Hide Muted, Unhide All, ADR-0080); the registry (ADR-0050) is the one list of commands; no hard-coded visual values (rule 15).

## Decision

1. **Three modes.** The Mixer has an *orientation* setting: `auto` (the current behaviour: a wide, low panel draws columns, a narrow or tall one draws rows; the implementing PR writes the exact rule, as token-derived sizes, into `ui-design-system.md` so `auto` is testable), `rows` and `columns`. The default is `auto`, so nothing changes for an existing user.
2. **Where the user picks it.** The Mixer's right-click menu gets a "Layout" submenu with the three modes as a radio group (check mark on the current one), placed above the hide commands. The panel's own menu repeats it (U3). Both are one registry picker command, `ui.mixer.orientation` (kind `picker`, ADR-0050), so it also appears in the command palette and can get a shortcut for free.
3. **Saved per workspace.** The workspace file gains one optional table `panel_options`, keyed by panel id, each value a table of simple keys: `[panel_options."rbs.mixer"] orientation = "columns"`. Absent means `auto`. Like `closed_sections` (ADR-0078) it keeps the format number (3) and is written only when a panel has a non-default option; a build that predates the key refuses the file, as ADR-0078 accepts for its key. `panel_options` is an open map that the workspace parser handles specially (the parser is strict about unknown top-level keys, ADR-0078): unknown panel ids and unknown keys inside a known panel are kept on save and ignored on use, with a round-trip test. An invalid value (`orientation = "diagonal"`) means `auto` and a warning. The Mixer is a single panel instance (`rbs.mixer`); Reset workspace keeps `panel_options`, like `closed_sections`. Applying another workspace applies its options, so a Live workspace can use columns while an Edit workspace uses rows.
4. **Not layout, not engine.** Changing the orientation is not a step in the layout undo history, is allowed in a locked workspace (like collapsing a section), and touches no source, output or the engine. The control API exposes it as the client-scoped command `ui.mixer.set_orientation {orientation}` (`auto`, `rows`, `columns`; anything else is `invalid_params`) and a getter, routed to one UI client like other `ui.*` commands (ADR-0017); the engine never sees it. A generic per-panel option method is not added until a second option needs it.
5. **A fixed mode never reflows by size.** In `rows` the Mixer scrolls vertically when the sources do not fit, in `columns` it scrolls horizontally; the group headers, hidden section and master strip (ADR-0081) keep their places relative to the arrangement, as ADR-0078 and ADR-0080 describe for each arrangement. A column has a minimum width token so strips stay readable; below it the panel scrolls instead of squeezing.
6. **Zero cost.** Switching re-lays out the strips once and re-places the meter layers (ADR-0035); there is no per-frame check of the mode. The meter feed is the same in both arrangements.
7. **Visual change.** Per rule 16 the owner approves a proposal first, then screenshots in dark, light and high contrast for `rows`, `columns` and `auto` at a wide and a narrow panel. All sizes, gaps and the minimum column width are tokens (rule 15); a missing token is added in the same change.

## Consequences

- The user decides, the layout is predictable, and a docked bottom Mixer can show rows.
- `ui-design-system.md` (Mixer, workspace file) and the workspace JSON Schema change in the same PR as the code; the glossary gains "Rows" and "Columns" as Mixer layouts.
- A fixed `columns` layout with many sources scrolls horizontally; `auto` stays for those who prefer no scrolling.
- `panel_options` is the place for later per-panel view settings, so the next one does not need a new top-level key.

## Alternatives considered

- **Keep size-driven only, tune the thresholds.** Still surprising; no user control.
- **Two separate panels (Mixer rows, Mixer columns).** Duplicates the panel, its sections and its state; users would keep two open.
- **Store the choice in the scene collection.** It is view state and differs by monitor and workspace; the collection would change when only the view changed.
- **A top-level `mixer_orientation` key.** Simple now, but every later per-panel option would add another key.

## Review

- 2026-11-09, independent AI session (round 1, reviewing only the ADRs, specs and code): changes requested. Concerns covered contract details, claims about existing code, hot-path and zero-cost rules, and number allocation across these ADRs; each was resolved in the text above (the numbers of protocol minors and scene formats are now assigned at merge time, not claimed by ADRs).
- 2026-11-12: Owner (Salih) accepts this ADR. The feature is in the tree: three modes (`auto`, rows, columns), workspace `panel_options`, the Layout menu and `ui.mixer.set_orientation`. Status: Accepted.
