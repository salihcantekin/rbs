# ADR-0110: Canvas layout space

- Status: Proposed
- Date: 2026-11-13
- Deciders: Salih

## Context

A canvas size change used to rewrite every item box with `rescale_layout`. The scale was the smaller of the two side ratios, and a box near an edge kept its margin to that edge. That fits a layout into a smaller or differently shaped canvas, and it is reversible when the aspect ratio does not change. When the aspect changes, both directions take the shrinking side, so the boxes compound. A layout designed at 1920×1080, shown on 1080×1920, then shown on 1920×1080 again, came back at about a third of its size. The previous boxes were not kept, and the undo stack was cleared, so the design could not be restored.

Item boxes are already the document. A second copy of each box, or a stored multiplier, does not fix this: the multiplier is determined by the two sizes, and the old edge rule was not a bijection, so a later edit could not be mapped back onto every position on the new canvas.

## Decision

1. Each canvas has a layout space, `layout_size`, separate from the picture size `size`. Stored item boxes (the default layout and per-canvas overrides) are in that space. A new canvas sets `layout_size` equal to `size`. `set_canvas_format` changes `size` and the frame rate only. It must not change `layout_size` and must not rewrite item boxes.
2. The picture is the stored box mapped by fitting the whole layout-space rectangle, centred, into the current canvas with one scale, the smaller side ratio. Position, size and style lengths take that scale. Rotation, crop and opacity do not. When the two sizes are equal the stored box is used unchanged. Sides of the layout space follow the same bounds as the canvas size.
3. The map is invertible. An edit made in current canvas pixels (a move, a crop that keeps scale, a fit, a style, a new item) is stored as the inverse, so the canvas draws that edit where it was made. Switching back to `layout_size` shows the stored boxes, including those edits, with no further shrink.
4. Flattening, auto layout and the studio host apply this map. The control API and the picture panel speak current canvas pixels. The scene file stores the layout space (`layout_width` and `layout_height`, omitted when equal to the canvas size, format 15). A file from before format 15 has no layout space; it is the canvas size.
5. A held program copy keeps the stored boxes. It is not rewritten on a canvas size change; flattening places it like the scene. This replaces the rescale of that copy in ADR-0063 decision 1.

## Consequences

- Returning to the size a layout was designed at shows that layout again. A shared default box is no longer rewritten when one canvas changes size, so another canvas that uses it keeps drawing the same pixels.
- The whole layout scales as one picture. A source in a corner of the design stays in that corner of the fitted picture, not in the corner of a canvas whose aspect differs. The bars outside the fitted picture are empty.
- A collection already saved with the old compounded boxes cannot be repaired: those pixels are the only copy, and its layout space equals its canvas size.
- Scene format 15. Specs: scene model, user data, the Video section of the UI design system, and the control API's item box.

## Alternatives considered

- Keep rewriting boxes, but store the last scale and divide by it on the way back. The old map is not invertible, and the same scale can come from different canvas pairs, so the edge a box was anchored to cannot be recovered.
- One multiplier and no layout size. The multiplier is already a function of the two sizes. Without the size the boxes were written in, a later canvas has nothing to map from.
- A layout size on every item. Canvases share a default box. One size on the canvas is enough, and an edit is written back into that space.

## Review

Independent review per ADR-0014. Not yet reviewed.
