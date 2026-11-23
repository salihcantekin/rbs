# ADR-0177: A scrolling fill moves its whole shape

- Status: Proposed
- Date: 2026-11-23
- Deciders: Salih (owner)
- Supersedes: part of [ADR-0150](0150-scrolling-layers-in-the-compositor.md) decision 2, for fills only

## Context

Library shapes (circle, square, rectangle, rounded rectangle, ellipse, triangle, arrow) and plain frames are colour sources with a 2 x 2 picture: a fill that the item's box stretches. (A Colour source added from the source list has a canvas-sized picture and is not a fill.) The shape and its border are the item's style (ADR-0053), drawn over the box.

ADR-0150 decision 2 scrolls a picture through its box with two rules made for tickers: across the direction the picture keeps its aspect ratio, and the style stays on the window, so a rounded ticker keeps its corners while its text moves. For a fill, both rules go wrong. The owner saw this on 2026-11-23:

- The shape never moves. A plain colour moves under a fixed shape mask, so `Loop` with no gap looks still and `Once` wipes the colour out of the mask once. Loop and Once look as if they were swapped.
- A 2 x 2 picture is square. Its scrolled length is the box's height, so a shape that is not square (an ellipse, an arrow, or any shape resized on one edge) scrolls only a part of its box, with gaps between copies.

The owner chose that a shape scrolls as a whole (2026-11-23, in the thread).

## Decision

1. **A fill.** A picture of at most `FILL_TEXELS = 2` texels on each side is a fill: it has no size or aspect ratio of its own. Reset transform uses the same threshold (`rbs-studio-host` `crop.rs`, `FILL_ONLY`) on the shown picture; scrolling tests the whole texture, so a cropped fill stays a fill. It looks only at the picture's size, not at a source type (rule 19, P3), so a plugin's fill behaves the same.
2. **Its length is the window.** `rbs_gpu::scroll::picture_length` gives a fill the window's length (the box it is stretched to) and any other picture `along × cross / across` as before. The node's `LayerScroll::frame` and the studio's `Once` end (`service_scroll`) both call it, so the reported end matches the drawing.
3. **Its style moves with it.** The 32-byte `ScrollGpu` record uses its spare slot: `b.w = 1` for a fill. The styled wrap shaders then evaluate the shape and border at the picture position (`styled_at`), with the pixel footprint still taken from the box position, so edges stay sharp at the seam. Coverage (gap, outside a `Once` copy, edge fade) multiplies shape and border together, so a gap shows no border. Other pictures keep their style on the window, as ADR-0150 decided.
4. **Nothing else changes.** No settings, API, scene format or UI change. `Once` still leaves the box empty when its picture has left, and a paused run still draws where it stopped.

## Consequences

- A Library shape with Scroll moves as a whole: `Loop` wraps it round, and `Once` takes it out of (or, with `Outside`, through) its box. A shape resized on one edge scrolls its whole box.
- A tiny real image (2 x 2 or smaller) also scrolls as a fill, which is what such an image shows anyway. A Colour source at its default canvas size is not a fill and keeps ADR-0150's behaviour.
- Each pixel takes the shape of the copy its centre falls in, so with `gap` 0 a straight edge or border that meets the seam mid-pixel can lose up to half a pixel of coverage there (a faint line on a framed or square shape). Evaluating both neighbouring copies near the seam would remove it at the cost of a second shape evaluation; left as a follow-up if it shows.
- Hot path: one comparison per scrolling layer in `frame` and two `select`s in the styled wrap shaders. Plain and styled layers that do not scroll draw with the same shaders as before (`styled` now calls `styled_at` with the box position, the same arithmetic). Numbers are in the commit.
- Specs: `gpu-compositor.md`, "Scrolling layers".
- Tests: `rbs-gpu` unit test `a_fill_is_as_long_as_the_window_and_its_style_moves`; GPU tests `a_fill_scrolls_its_whole_shape_and_wraps` and `a_once_fill_takes_its_border_along_and_leaves_the_rest_empty`; node tests `a_scroll_draws_its_picture_and_gap` and `a_finished_once_scroll_draws_nothing_and_stops_drawing` (now fills). The studio's `Once` end calls the same `picture_length`, so it is covered by the unit test rather than a studio test.

## Alternatives considered

- **A capability or flag on the colour source type.** Clearer intent, but it needs a new field through the scene, binder and layer list, and a plugin would have to set it. The picture size already says what is needed. Rejected.
- **No Scroll section for shapes.** Simple, but the owner wants shapes to scroll. Rejected.
- **Keep the stretched aspect for every picture** (the scrolled length from the item's own scale). It would also change a stretched image, and a single-line text box that is narrower than its text would squeeze the text. Rejected; only fills change.

## Review

- 2026-11-23, independent AI session (fresh context; ADR, ADR-0150, spec and diff): **approve with changes**. Addressed: the Colour source is not a fill at its default size (Context, Consequences, spec, index); the seam coverage limit is written down; the fill threshold's relation to Reset transform is stated. It confirmed WGSL uniformity (derivatives before any branch, the only branch on a uniform), no border in gaps, unchanged paths for non-fills and non-scrolling styled layers, and no other old-formula length in the workspace. Open: a short SC-PC or Mac bench (P15) and the owner's approval.
