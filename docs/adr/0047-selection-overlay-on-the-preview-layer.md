# ADR-0047: Draw the preview's selection outline and handles as native layers of the preview

- Status: Proposed
- Date: 2026-11-04
- Deciders: Salih (owner)
- Builds on: ADR-0034 (preview as a native layer, macOS), ADR-0036 (Windows preview layer), ADR-0035 (meters as native layers), ADR-0013 (zero cost when unused), ADR-0015 (UI design rules)

## Context

UI principles require that "sources are positioned, resized, rotated and cropped directly on the preview with handles, snapping guides and keyboard nudging", with a target of under 15 s to move, resize and crop a source. The desktop app has numeric fields for a source's box (PR #126) but nothing on the preview.

The preview is not drawn by the toolkit. On macOS it is a Core Animation layer above the toolkit's layer (ADR-0034); on Windows a DirectComposition visual in a topmost composition target of the window (ADR-0036). Anything Slint draws over the preview area is hidden under the picture; that is why in-window popups hide the layer (ADR-0034 decision 4). A selection outline and handles must show over the picture while it plays, so they cannot be drawn by Slint and cannot hide the layer.

Pointer input is not affected: neither layer takes hit testing, so presses over the picture reach the window and the toolkit's `TouchArea` as before.

Scene edits during a drag reach the running studio through `Studio::update` (ADR-0041), which syncs the binder and calls `Engine::apply` on every call; a drag issues tens of edits per second.

## Decision

1. **The overlay belongs to the preview layer.** `rbs_gpu::share::PreviewLayer` gains `set_marks(&[Mark])`: filled, axis-aligned rectangles with an sRGB colour (straight alpha), in the same window coordinates and units as `set_rect` (macOS: the view's points; Windows: physical pixels of the client area). An empty slice removes them. Marks are drawn above the picture and are not clipped by the layer (they may extend past the picture into the letterbox bars); the shell clips them to the panel's area, since past it they would cover other panels. They are hidden and shown with the layer (`set_hidden`), and removed by `detach` (which clears them, so a later `attach` starts without marks). They sit in the preview layer, not in a separate UI layer as ADR-0035's meters do, because on Windows they must share the preview's DirectComposition device and target (a window has one topmost target), and because hiding, attaching and detaching must stay in step with the picture.
2. **macOS.** A container sublayer of the preview layer holds up to `MAX_MARKS` (32) solid-colour sublayers, created on first use and then reused; `set_marks` changes frames, colours and `hidden` in one transaction without implicit animations and makes no Rust heap allocation. Edges snap to the device pixel grid (`contentsScale`). A rectangle that is not finite or has no area is skipped (Core Animation throws on non-finite frames).
3. **Windows.** The target's root becomes a container visual with two children: the existing picture visual (unchanged, with its fit transform) and an overlay visual with an identity transform. Each mark is a child visual of the overlay whose content is one shared 1 x 1 `IDCompositionSurface` per colour (premultiplied BGRA, filled once with `BeginDraw` and a D3D11 clear), scaled to the mark's rectangle by its transform. Visuals and surfaces are created on first use, at most `MAX_MARKS` visuals, and reused; one `Commit` per `set_marks`. A visual that is not used has its content removed (`SetContent(None)`), never a zero scale, which DirectComposition refuses as a non-invertible transform; a used mark is at least 1 px wide and high.
4. **What is drawn is UI logic, outside the GPU crate.** `rbs_ui::transform` (toolkit-neutral, pure) maps between the window and the canvas, finds the handle or source under the pointer, computes moves and resizes (corners keep the aspect ratio unless Shift; edges change one side; snapping to the canvas's edges and centre unless Ctrl/Cmd; never smaller than 8 canvas pixels or inside out) and lists the marks: a 1 px outline and eight 8 px handles in `color.accent` with a `surface.raised` inside. The shell converts to the layer's units and colours from the theme's tokens.
5. **Edits during a drag.** The shell shows the new box in the marks at once, and keeps at most one `SetSourceBox` in flight; while it is unanswered, only the newest box per source waits. Each request carries a sequence number and the studio host always answers it with `BoxPlaced { seq, ok }` (also when the box was unchanged or refused), so an unrelated update is never taken as the answer and a refused or lost edit cannot leave the drag stuck; the shell also forgets what is in flight when the studio starts, stops or fails, and when there is no studio to send to. The studio thus sees at most one edit per round trip however fast the pointer moves. Each edit is a normal `Studio::update` (binder sync and `Engine::apply`); its cost on the control thread is measured on the reference Mac in the PR. If it is too high, skipping `Engine::apply` for edits that change only the compositor's layer list is the next step (it needs a way to tell that the bound graph did not change, which `GraphDescription` does not offer today). Scene editing stays off the hot paths (`docs/architecture/hot-paths.md`).
6. **Zero cost when unused (P13).** No sublayers or visuals exist until the first selection is shown; an unselected preview has no marks and does no overlay work. Nothing runs per frame: marks change only on selection, pointer and layout changes.
7. **Fallback.** Where no layer shows the picture (Linux, remote desktop on Windows, software adapters: the toolkit shows it, or nothing is shown yet), the toolkit draws the same marks itself, so editing on the picture panel works there too.

## Consequences

- The preview layer API grows by one method on both platforms; the Windows layer's visual tree changes (a root container), so the Windows preview needs a hardware check again.
- Handles and outline show over the live picture without redrawing the toolkit; a drag costs one compositor transaction per pointer move plus coalesced scene edits.
- Two drawing paths for the marks (layer and toolkit fallback), fed by the same `rbs_ui::transform::marks`.

## Alternatives considered

- **Draw the marks in Slint and hide the layer while editing.** The picture would vanish whenever a source is selected or dragged; the user positions a camera against what is on screen, so this defeats the purpose.
- **Draw the marks into the preview frames on the GPU (in the preview node).** Touches a hot path (P15), ties UI state to the engine's frame rate and adds a frame of latency; the outline would also be recorded if a preview ever fed an output.
- **A separate UI-side native overlay (as the meters, ADR-0035).** Fine on macOS, but on Windows a window has a single topmost composition target, owned by the preview layer; a second device and target cannot stack above it.
- **Layer below a transparent toolkit window with a hole (ADR-0034 alternative).** Correct stacking for every overlay, but the whole window becomes non-opaque; ADR-0034 keeps it as a fallback only.

## Review

- 2026-11-04, independent AI session (`architecture-review` skill, given only this ADR, the specs and the code): changes requested, eight findings, all resolved in the same PR:
  1. A box could stay in flight forever when the studio stopped or refused it: requests are numbered and always answered (`BoxPlaced`), and the shell resets on start, stop, failure and when there is no studio (decision 5).
  2. Any update was taken as the answer: answers are matched by sequence number.
  3. One waiting slot was shared by all sources: the newest box waits per source.
  4. Windows: a zero scale for unused visuals is not invertible: unused visuals lose their content instead, used ones are at least 1 px (decision 3).
  5. macOS: snapping relative to the picture's rectangle was off the device grid: edges snap in window coordinates (decision 2).
  6. `detach` left marks behind for the next `attach`: it clears them (decision 1).
  7. The fallback wording did not say editing stays available where no layer is used: reworded (decision 7).
  8. A cancelled pointer (lost capture) left a drag running: cancel ends the drag.
