# ADR-0053: The preview picture never disappears behind popups

- Status: Proposed
- Date: 2026-11-06
- Deciders: Salih (owner)
- Builds on: ADR-0034 (macOS preview layer), ADR-0036 (Windows preview layer), ADR-0048 (selection marks on the layer), ADR-0049 (main window)

## Context

The preview and program pictures are native layers over their panels: a Core Animation layer on macOS (ADR-0034) and a DirectComposition visual on Windows (ADR-0036). They sit above everything Slint draws in the window. Slint 1.18's winit backend draws a `PopupWindow` inside its window (`winitwindowadapter.rs`: "We don't render popups as separate windows yet"); only native popup menus (`MenuBar`, `ContextMenuArea` through `muda`) are separate OS windows.

So that in-window popups, toasts and the error bar are not drawn under the picture, the app hides the whole layer while any of them is open: each popup adds itself to a counter (`Studio.overlays`) and `preview.rs` hides the layer while the counter is above zero, a toast or the error bar shows, or the deleted-source toast shows.

The owner tested the source shapes work on macOS (2026-11-06): opening the colour picker in Properties blanked the preview, although the picker did not overlap it, and the same happened earlier with other menus and dialogs. The owner's rule: the central picture must never disappear when any popup, dialog or window opens.

## Decision

1. **Rule.** While its panel is visible, the preview or program picture is never hidden by something the user opens: popups, pickers, menus, dialogs, toasts, the error bar or other windows. At most the part that something actually covers is not shown. This joins U21 ("Always reachable, never in the way") as a bullet.
2. **Covers are rectangles, not a count.** Everything the toolkit draws over the content (in-window popups, toasts, the error bar) reports its rectangle in window coordinates while it is shown, under a stable id; `Studio.overlays` is replaced. A pure function in `rbs-ui` gives the layer's visible region: the layer's rectangle minus the union of the cover rectangles, as a list of rectangles (at most 16; beyond that, the smallest pieces are dropped).
3. **A cover that does not intersect the layer changes nothing.** The layer stays as it is; no layer call is made.
4. **A cover that intersects the layer is cut out of it.** On macOS the layer gets a mask (a shape layer, even-odd fill: the layer's bounds plus the cover rectangles), which also cuts the selection marks (ADR-0048), since they are sublayers. On Windows the visible region is applied with clips on the preview's visual and the marks' visual, using one sibling visual per region rectangle with the same content; until that is checked on Windows hardware, Windows hides the layer only while a cover intersects it (rule 3 still holds there). The mask or clips change only when a cover opens, moves or closes, never per frame.
5. **Separate windows are not covers.** Native menus, the Settings window, floating panels and projectors are OS windows, which the OS draws above the layer; they do not report rectangles.
6. **Tests.**
   - The visible-region function: property tests (the result never overlaps a cover, lies inside the layer, covers every uncovered point when at most 16 rectangles suffice, is empty only when the layer is fully covered).
   - Every `PopupWindow` in the app's `.slint` files reports its rectangle: a test reads the files and fails for a popup without a matching cover id.
   - A shell test (demo build) opens each popup in turn and checks that the layer is not hidden and that its region does not intersect the popup.

## Consequences

- Opening a picker, menu or toast no longer blanks the preview; covered parts are cut out exactly on macOS and, until the Windows check, the whole picture only on Windows and only when it is really covered.
- Each new popup must report its rectangle; the `.slint` test catches one that does not.
- `rbs-gpu`'s `PreviewLayer` gains `set_visible_region(&[LayerRect])` on both platforms (UI side, not the engine's hot path: it runs only when a cover changes). `unsafe` stays in the existing layer modules with `// SAFETY:` comments.
- Linux has no preview layer yet (deferred); when it gets one, it follows the same rule.

## Alternatives considered

- **Keep hiding the whole layer, but only when a popup intersects it.** Fixes the reported case but still blanks the picture for any menu that overlaps it by a few pixels; the owner asked for never.
- **Open every popup as its own OS window.** What the OS would draw correctly, but Slint 1.18's winit backend cannot do it yet; writing our own popup windows per platform is the maintenance the owner wants to avoid. Revisit when Slint renders popups as separate windows; then rule 5 covers them and rules 2-4 apply only to toasts and the error bar.
- **Draw the picture with Slint instead of a native layer.** Rejected by the UI CPU budget (ADR-0034, ADR-0036).
- **Place popups so that they never overlap the picture.** Not always possible (a picker under a control next to the preview, small windows, 200 % text) and would make popups jump around.

## Review

Pending: independent review per ADR-0014.
