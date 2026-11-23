# ADR-0178: Applying a source region keeps the item in place

- Status: Accepted (2026-11-23)
- Date: 2026-11-23
- Deciders: Salih (owner)
- Amends: [ADR-0103](0103-source-region.md) (how Apply places the item); aligns it with [ADR-0104](0104-display-chrome-insets.md) decision 4 (Apply does not move the box, Fit is separate)

## Context

ADR-0103 does not say what happens to an existing item when its source's region is set. The implementation placed the edited scene's item with Fill on the canvas, so a canvas-sized screen region filled the screen. For a camera in a small circle in a corner, setting a region spread the cropped part over the whole canvas (owner test on Windows, 2026-11-23). ADR-0104 decision 4 already says that applying display chrome does not move the box and that fitting is a separate, undoable step.

## Decision

1. Setting or clearing a source region (Source settings Apply and `source.set_region`) keeps the edited scene's item in place: its box keeps its centre, area, rotation and flips, and takes the visible picture's aspect (the region and the item's own crop; the whole picture when cleared) (`Layout::fitted_in_box`). Keeping the area means repeated edits neither shrink nor grow the box. It is part of the region's undo step. When the picture size is not known yet the region is stored and the box is not changed.
2. Filling or fitting the canvas is the Fit menu and `item.fit`, as for any item.
3. ADR-0103 decision 5 (Add source with a region) is unchanged: a new item is still fitted to the region's aspect when it is placed.

## Consequences

- A camera or a window placed by the user stays where it is; its picture is the region, not stretched.
- A user who wants a screen region full screen picks Fit or Fill once (one more step than before).
- Specs: `control-api.md` (`source.set_region`), `status.md`; the Source settings hint text says Apply keeps the item in place.
- Tests: `rbs-scene` `a_region_keeps_the_box_centre_and_area` (centre, area, back and forth, flips and rotation). A host-level test of `set_capture_region` (one undo step restoring region and box) is a follow-up.

## Alternatives considered

- **Keep Fill on the canvas** (the implementation before). Moves a placed camera; rejected by the owner.
- **Fill only when the item already covered the canvas.** Two rules for one button; rejected by the owner.
- **Keep the box exactly and stretch the picture.** Distorts the picture when the region's aspect differs.

## Review

Owner decision: Salih chose "Kutu yerinde kalsın" (keep the box where it is) on 2026-11-23 and asked to fix the ADR, accept it, implement it and merge ("ADR'ı düzelt, kabul et, geliştirmeyi yap ve main'e merge et").

Reviewer: independent AI session
Date: 2026-11-23
Verdict: changes requested
Concerns and resolutions (author, 2026-11-23):
1. Accepted before the review was recorded. -> Review recorded here before acceptance.
2. Clearing the region left the box at the region's aspect and stretched the whole picture. -> Decision 1: clearing takes the whole picture's aspect too.
3. Fitting inside the old box made boxes shrink with every edit. -> Decision 1: the area is kept instead.
4. A dead `mode` branch with a different crop. -> Removed; one path with the item's crop.
5. No host-level test; unknown-size behaviour unstated. -> Unknown size stated in decision 1 and the spec; the host test is a follow-up (Consequences), accepted by the owner's request to merge now.
