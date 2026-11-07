# ADR-0064: The Transition rail sits with the pictures

- Status: Accepted
- Date: 2026-11-07
- Deciders: Salih (owner)
- Builds on: ADR-0061 (studio mode), ADR-0049 (docking, areas), ADR-0050 (command registry), ADR-0054 (the picture never disappears)
- Supersedes in part: ADR-0049 decision 3 (the studio split of `studio = true` with `rbs.transition` as a fixed-size child, and `ui.layout.studio_split`); the UI design system spec (Studio mode, Workspace file format 2) changes with it

## Context

The owner tested studio mode on the Mac (2026-11-07). Transition was an ordinary panel in a tab next to Live controls, far from the two pictures. He asked that the Transition control appear only in studio mode, only in the centre with Preview and Program, between the two pictures by default, and below them (centred) if the user prefers; no other place.

The spec planned a studio split (`studio = true`, three children: Preview, `rbs.transition` with a fixed `size`, Program), with the rail also shown outside studio mode as a transition picker, and generic fixed-size split children. None of it is built. This ADR replaces that plan with a smaller one that matches the owner's request.

## Decision

1. **The studio split.** A horizontal split in the main window's centre may carry `studio = "between"` or `studio = "below"` in the workspace file. It has exactly two children, each a stack with one tab that is not collapsed: Preview (`rbs.preview`, or `rbs.preview?canvas=...`) and then the Program of the same canvas. At most one exists. A file with any other shape is refused (validation error).
2. **The rail is not a panel.** The Transition control in studio mode is drawn by the window in a rectangle that the layout reserves. It is not a tab stack and cannot be dragged, floated, collapsed or docked elsewhere.
   - `between` reserves a 156 px column (a 144 px rail and a divider gap on each side) between the two pictures.
   - `below` reserves a 56 px row under both pictures (a 50 px rail under a divider gap), with the controls centred.
   - Outside studio mode the split shows Preview alone. Program counts as hidden (U5), and there is no rail. The picture goes to Preview as it does whenever Program is not shown (ADR-0061).
3. **Moving the rail.** The registry command `ui.layout.studio_rail {at}` (`at` is `between` or `below`; the View menu and the rail's own "Rail position: Middle | Below" choice run it) moves the rail. It is one layout history step, allowed on a locked workspace like other commands, and refused with `conflict` when the workspace has no studio split. It is a `ui.*` command routed to the client (control API, ADR-0050).
4. **Dissolving.** Preview and Program of the studio split cannot be collapsed or auto-hidden: those commands are refused with the reason, and their menu items are not offered. Moving or closing either of them, or dropping a tab into their stacks, leaves an ordinary split, and the `studio` flag is dropped in the same step; outside studio mode Program then shows again, as it does in any workspace without a studio split. Resetting the workspace brings the studio split back. Outside studio mode Program is open but hidden (U5): the Panels menu lists it as open, and closing it from there dissolves the split. In the `between` position the rail takes the place of the divider, so the pictures' ratio cannot be dragged there; in the `below` position the divider between the pictures works as usual. When the split is too narrow for the rail's column and two 48 px pictures, the rail is drawn below them whatever its saved place.
5. **Built-in workspaces.** Edit and Live put a studio split (`between`) in the centre. `rbs.transition` leaves the built-in layouts and the Panels menu. A workspace file that still has it loads and shows it as before, so no saved layout is lost.
6. **Zero cost.** The rail is laid out only in studio mode and draws no picture. Outside studio mode the split costs the same as a single stack.

## Consequences

- Studio mode shows Preview, Transition and Program side by side in Edit and Live with no setup. Outside studio mode Edit looks as it does today.
- Workspace format 2 gains the `studio` key on a split. The spec's `studio = true` and fixed-size children are dropped; they never shipped.
- A saved copy of Edit or Live from before this change keeps its own layout (no studio split, and `rbs.transition` as a panel) until the user resets the workspace.
- The studio split follows studio mode of the canvas being edited. A split of `rbs.preview?canvas=...` for another canvas follows the same flag until per-canvas studio mode reaches the UI.
- A custom workspace without a studio split has no rail. Transition then runs from the toolbar, the Scene menu and Ctrl/Cmd+Enter.
- Tests: layout tests for both rail positions, the hidden Program outside studio mode, stable stack indices, dissolving, and the file shape; a view test that Edit in studio mode reserves the rail.

## Alternatives considered

- **The spec's three-child studio split with a fixed-size Transition stack.** It needs fixed-size children in every split, and a Transition tab that can still be dragged out, which the owner does not want.
- **A floating rail drawn over the pictures.** It would cover part of the picture (ADR-0054) and move about with the window.
- **Keep Transition as a normal panel.** That is what was tested, and it is far from the pictures it acts on.

## Review

Independent review per ADR-0014, 2026-11-07, by a fresh AI session given only this ADR, the specs, the code and the screenshots. Verdict: accept with changes, nothing blocking. Settled:

1. The spec still counted the rail as a fixed-size child of minimum 0, and a narrow split could squeeze the pictures to nothing. The rail now goes below the pictures when the column does not fit (decision 4, tested), and the spec says so.
2. and 3. The hidden Program's place in the Panels menu, and Program showing again when the split dissolves outside studio mode, are now stated in decision 4.
4. and 5. Saved copies of Edit and Live from before this change, and the studio split following the edited canvas's studio mode, are now stated in Consequences.
6. Screenshots are dark theme only; light and high-contrast screenshots and the keyboard walkthrough are left to the owner's Mac test.

Nits taken: the `between` position has no divider (decision 4); ADR-0049's review section points here. Left as they are: the rail's "Cut" label is the only transition until fades (ADR-0061); `Studio.state == 1` is the studio's running state as elsewhere in the views.
