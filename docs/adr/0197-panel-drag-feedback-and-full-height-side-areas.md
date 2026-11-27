# ADR-0197: Panel drag feedback and full-height side areas

- Status: Proposed
- Date: 2026-11-27
- Deciders: Salih (owner)
- Builds on: [ADR-0049](0049-main-window-areas-panel-states-and-sizes.md) (areas, dragging), [ADR-0163](0163-one-command-path-for-the-window-and-the-api.md)
- Amends: ADR-0049 decision 1, only where it says the bottom area always spans the whole width: it now spans the whole width unless a side area is set to full height. Everything else in ADR-0049 stands.

## Context

1. Dragging a tab changes nothing on screen until the pointer reaches a drop target: the tab stays as it is, nothing follows the pointer, and the user cannot tell whether a drag started (owner, 2026-11-27). The drop guide is also drawn only over the stack under the pointer.
2. The bottom area always spans the whole window width (ADR-0049 decision 1), so the left and right areas end above it and can never use the full window height (owner, 2026-11-27).

## Decision

1. **Drag feedback (UI only, no layout change).** While a panel is dragged (the source `DragArea`'s `dragging`, kept in `Shell.panel-dragging`):
   - the source tab stays behind as an empty slot (sunken, outlined, quiet title), so the user sees where it came from;
   - every stack that takes the panel (`StackData.takes-drop`: not the Pictures leaf, ADR-0073) is outlined in the accent colour from the first moment, and the stack the drop would fill keeps the existing shaded guide;
   - a card with the panel's icon and title follows the pointer (raised card look, accent outline, fades in in `motion.fast`);
   - Escape cancels: `Shell.drag-cancelled` stops the drop areas from accepting, the guide goes away and nothing is docked.
   No new token: the colours, `radius.md`, `size.focus-ring`, `size.border` and `motion.fast` already exist. The card is drawn inside the window's dock area, so it follows the pointer only while that is over the dock area (Slint 1.18's winit backend has no native drag, so a drag cannot leave the window).
2. **Full-height side areas.** `Area` gains `full_height` (left and right only; saved only when true, so older files load unchanged and an older build refuses a file that has it, as `deny_unknown_fields` does today). A full-height side area runs from the top to the bottom of the window content. The bottom area then spans only the width between the full-height areas (the whole width when none is). Everything else stays: sizes, limits, dividers (the area edge divider is as tall as the area), collapse, and the bottom area's own size. A full-height flag on an area without panels or on a hidden area does nothing. The default is off, so no existing layout changes.
3. **How the user gets it.**
   - **Drag:** dropping a panel near the left or right window edge beside the bottom area (a drop target `DropPlace::FullHeightEdge(edge)`). Its guide is the strip along the whole window height; the ordinary edge drop above the bottom area now shows a guide that ends at the bottom area, so the two targets look different. A side area that already runs the full height takes an ordinary edge drop. Emptying a side area clears its flag moves it to that side area and turns `full_height` on. Near the same edge above the bottom area the drop keeps its current meaning (`WindowEdge`). `drop_target` stays a pure function in `rbs-ui`; `WindowFlat` gains `bottom: Option<Rect>` to tell the two apart.
   - **Panel menu:** `ui.panel.full_height {id}` (registry entry, checkable, in the panel context menu, not the menu bar, `NOT_IN_MENU`) toggles `full_height` of the side area that holds the panel; disabled with a reason for a panel in the bottom area, the centre or a floating window, and while the layout is locked. Control API: protocol 1.114, `ui.panel.full_height {id}` flips the area's full-height flag and answers `{}`; a caller that needs a known result reads `checked` of the entry in `ui.command.list` first (an optional `on` can be added in a later minor). It is refused with `conflict` while the layout is locked, as `ui.panel.move`, `float` and `close` are.
   - Layout commands: `LayoutCommand::MoveToFullEdge`, `SetAreaFullHeight`; both are one layout history step and blocked by the lock when they come from the pointer.
4. **Plugins.** Panels, including plugin panels, move like any other panel; the layout model knows no panel type.

## Consequences

- The workspace file format gains one optional key per side area. No migration; a spec line in the workspace format and `control-api.md` (1.114) are added in the same change.
- Pure `main_rects` gains the full-height rule; tests: layout beside the bottom area for one and both sides, nothing changes without a bottom area, the drop target beside the bottom area, round trip and old files.
- The panel drag keeps its single DropArea per dock area, so no new per-frame work outside a drag; nothing is drawn when no drag is running (zero cost when unused).
- Hot paths: none touched (layout and UI only).

## Alternatives considered

- **A native OS drag image (`DragArea.drag-image`).** The in-window drag overlay would need a pre-rendered bitmap of the tab; Slint cannot snapshot an element, and the winit backend does not start native drags in 1.18. The card in the dock area gives the same cue with theme tokens.
- **Always full-height side areas (bottom between them).** Changes every existing workspace without being asked; a per-area switch keeps today's layout as the default.
- **A fourth "corner" area.** More model, more drop targets and a larger file format change for the same effect.
- **Dim the dragged tab with opacity.** The token set has no opacity token, and a new token group needs its own review; an empty slot uses existing colour tokens.

## Review

Pending: independent review per ADR-0014 (a new workspace file key, a drop target and a control API method). The ADR stays Proposed until it is recorded and the owner approves.
