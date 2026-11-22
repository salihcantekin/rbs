# ADR-0172: The Panels menu toggles, and a closed panel reopens where it was

- Status: Accepted
- Date: 2026-11-22
- Deciders: Salih (owner)
- Spec: [ui-design-system](../specs/ui-design-system.md) (Pictures leaf paragraph, menu table row `ui.panel.select`, window frame)
- Supersedes in part: [ADR-0073](0073-pictures-area-and-studio-mode-switch.md) decision 4 (panel placement on open), for a panel closed earlier in the same session
- Builds on: [ADR-0049](0049-main-window-areas-panel-states-and-sizes.md) (areas, edge groups, anchors), [ADR-0050](0050-command-registry-menus-and-toolbar.md) (command registry), [ADR-0017](0017-flexible-panels-and-workspaces.md)

## Context

The owner tested on the Mac (2026-11-22) and found two problems with the Panels menu:

1. Choosing a panel that is open does nothing visible. The macOS menu shows open panels checked and flips the mark on a click, but `ui.panel.select` always runs `ui.panel.open`, which only shows an open panel. The only way to close a panel is its own "..." menu or close button.
2. A panel closed and opened again does not come back where it was. `Workspace::open` follows ADR-0073 decision 4: a closed panel goes into the first stack of its default home area (Scenes and Sources left, Properties right, Mixer bottom), else the centre's first stack. A Mixer the user had moved under Properties comes back at the bottom; Sources, which sat under Scenes, comes back as a tab of Scenes.

## Decision

1. **The Panels menu toggles.** Each item of `ui.panel.select` is checked while its panel is open, in any window or auto-hidden. Choosing a checked item closes the panel (`ui.panel.close {id}`, the same command and guarded-close toast as the panel's own Close); choosing an unchecked item opens it (`ui.panel.open {id}`), as before. An open panel that is hidden (an inactive tab, a collapsed stack, a hidden area, an auto-hidden group) counts as open, so its item closes it: the check mark always says what a click does. Showing an open panel stays with `ui.panel.open` (command palette, shortcuts, control API), which is unchanged. Preview and Program of the Pictures leaf are items too: like their own Close, choosing a checked one closes `rbs.pictures` (both pictures, ADR-0073 decision 2), a guarded close with the undo toast while live (ADR-0054). The registry tooltip becomes "Open a panel, or close it if it is open".
2. **A closed panel reopens where it was.** When a panel is closed (from any route: menu, close button, `ui.panel.close`, closing a floating window), the workspace remembers its spot: the tree it was in (the centre, an area, an edge group or a floating window with its placement), the other panels of its stack and its tab index, and its neighbour: the node next to the part of the tree that holds its stack, at the lowest split that has one (the one before it preferred), kept as that node's panels, with the side of it the panel was on and the share of the two the panel's part had. Opening it again tries, in order:
   1. into the stack (or edge group) of one of its former stack mates that is still open, at its old tab index;
   2. else a new stack on the same side of the neighbour node, found again as the node with exactly those panels, with its old share (kept between 5 % and 95 %); if no node has exactly those panels, beside the stack of one of them; only when the tree depth limit allows;
   3. else its old area (first stack, the area made visible, created when empty), its old edge group (created when missing, within the group limits) or a new floating window with the old placement;
   4. else ADR-0073 decision 4, unchanged (a panel that was in the centre with no mate or neighbour left goes to its default home).
   The Pictures leaf and Preview and Program of any canvas keep their own rules (ADR-0073 decision 2, ADR-0093 decision 7) and are not remembered.
3. **In memory only.** The spots belong to the open workspace and live while the app runs: they are not written to the workspace file (format 3 stays, no contract change), and are gone after a restart, a workspace switch or Reset workspace; then decision 4 of ADR-0073 applies. They do not count as layout: two workspaces that differ only in their spots compare equal (history, modified checks). At most 64 spots are kept, oldest forgotten first. Undo of a close restores the layout from the history as today.
4. **Zero cost.** A spot is recorded only on close and read only on open; nothing runs per frame or per redraw.
5. **Tests.** Unit tests in `rbs-ui`: a tab reopens in its stack at its index; a lone stack reopens beside its neighbour with its share (Edit's Sources: the layout equals Edit again); a panel moved to another area reopens there and, with its neighbour also closed, still in that area; a stack beside a nested split reopens beside the whole split with its share; two panels of a closed floating window come back in one window; undo and redo of a close and of a reopen through the layout session; a floating panel reopens in a floating window with its placement; an auto-hidden panel reopens in its edge group at its index; the file is unchanged and a workspace read from the file uses the default home.

## Consequences

- The Panels menu matches what macOS users expect from a checked Window menu item, and closing a panel no longer needs its "..." menu.
- Closing and reopening a panel is a round trip within a session, which also makes the menu toggle safe to use.
- After a restart a closed panel still opens in its default home. Keeping spots across restarts needs a workspace file change (a new optional table and a format bump, ADR-0017), left for a later ADR if wanted.
- A user who used the Panels menu to bring a hidden tab forward now closes it instead; the command palette and shortcuts still show it.
- One click on Preview or Program in the Panels menu now removes the Pictures area, as their own Close does; while live it shows the undo toast.
- Each layout history step shares the spots with the workspace (copy on write), so the history does not copy them per step.
- The menu toggle itself (`rbs-app` `panels_menu.rs`) has no unit test: it is two lines over the tested commands and is checked by hand on the Mac.

## Alternatives considered

- **Checked item shows a hidden panel first, closes it on a second click.** The check mark would no longer say what a click does, and the macOS menu flips its mark on every click anyway.
- **Save spots in the workspace file now.** Survives restarts, but changes the file format (public contract) and old builds would refuse the file; not needed to fix the reported problem.
- **Never remove a closed panel from the tree, only hide it.** Keeps the place perfectly, but every layout operation, drop guide, stack index and the control API's `ui.layout.get` would have to skip hidden entries; a much larger change.

## Review

Independent review per ADR-0014, 2026-11-22, fresh AI session: approve with changes, 10 findings.

1. A neighbour that is a nested split was found again as one of its stacks: fixed, the neighbour is now the node (decision 2), with a test.
2. Preview and Program items close the Pictures leaf: stated in decision 1 and Consequences.
3. Missing tests: added undo and redo, a closed floating window with two panels, a nested neighbour (decision 5). Depth-limit and full edge group fallbacks fall through to the next rule and stay untested; the menu toggle is checked by hand.
4. `ClosedSpots` equality is always true: kept, with a comment on why a spots-only change is dropped on purpose.
5. Spots copied per history step: now shared copy on write (`Arc`).
6. A failed insert next to one mate stopped the search: now tries the next mate.
7. A panel back in its edge group went to the end: now at its old index.
8. The picture check repeats `open()`'s list: kept; the two differ (`open()` checks only pictures without a canvas parameter), and both are the two built-in picture ids of ADR-0073, not a new list of types.
9. Spec sync: the ADR now says "the Panels menu"; the spec's Workspace file section says spots are not saved; ADR-0073 gets its "superseded in part" line when this ADR is accepted.
10. The macOS check mark after a refused close: the menu's check marks are bound to the panel list that is read from the workspace on every sync after a command, refused or not, so a refused close shows the mark as it was; to be confirmed on the Mac.

Salih (owner) approved on 2026-11-22 ("Onayla, Mac'te dene, merge et"); merge after a hand check of the Panels menu on the Mac.
