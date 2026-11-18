# ADR-0150: Scrolling is a layer motion evaluated by the compositor node, for every source

- Status: Proposed
- Date: 2026-11-18
- Deciders: Salih (owner)
- Answers: [ADR-0129](0129-text-source.md) decision 9 (scrolling decided separately); clarifies [ADR-0117](0117-still-sources-as-images.md) decision 2's "ticker" (decision 9 below)
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0023](0023-node-parameter-channels.md), [ADR-0053](0053-item-style-and-video-filter-chains.md), [ADR-0058](0058-crop-editing-of-sources.md), [ADR-0100](0100-configurable-glide-duration-and-easing.md), [ADR-0128](0128-scene-transitions.md), [ADR-0143](0143-performance-rules-for-every-change.md) (R5, render on change)

## Context

A ticker, a news crawl, end credits and a subtitle line that runs across the screen all need a picture that moves through a window. RBS has a text source (ADR-0129) but nothing moves it, so the owner asked on 2026-11-18 whether a text source can scroll. OBS does this with a Scroll filter on any source.

Forces:

- **ADR-0129 decision 9** fixed the requirements: f64 phase on the CPU, only the wrapped offset sent on (no f32 drift on 24/7 streams, no jump when the speed changes), nothing drawn for an off-air item, the picture not static while it moves (ADR-0117), and a line wider than one 4096 px box. It named two designs (a per-layer UV offset with repeat in the compositor, an animated video filter) and asked for measurements of both.
- **The compositor node already animates layers** from targets sent once: `NodeLayer` carries a `Fade` and a `Slide` with a start time, a duration and an `Easing`, evaluated at each tick's time (ADR-0023 decision 7, `gpu-compositor.md` "Compositor node" step 4). Parameter channels are typed per parameter and keep only the latest value (ADR-0023 decisions 1 and 2), so anything the node must not miss belongs in the list.
- **Several compositors draw one scene.** A Fade, Swipe or Slide transition draws the incoming scene in its own compositor (ADR-0128 decision 5); studio mode draws the preview in another; a global scene can be on two canvases; a node is rebuilt when a canvas changes size. A motion kept as state inside one compositor would restart or disagree between them.
- **Render on change (R-03, ADR-0143 R5).** An unchanged tick costs one compare per layer and no GPU work. A moving layer must make its canvas draw every tick, and only while it moves.
- **P6, P13, P15.** Nothing allocates in `Node::process`; a layer that does not scroll must not pay more than the `Option` test fade and slide already cost; the compositor is a hot path, so the change needs measurements and the owner's sign-off.
- **Text size.** The text contract caps a box side at `MAX_SIDE = 4096` and its area at 8 388 608 pixels (`text-port.md`); ADR-0129 decision 5 reserves textures the size of the box.
- **Owner rules.** Feature parameters are saved settings reachable through the control API (Stream Deck parity). A visual change is approved with dark and light screenshots before merge. Beyond-OBS behaviour is welcome (vision, rule 17).

## Decision

1. **Scroll is part of an item's layout.** `Layout` gains `scroll: Option<Scroll>` (absent: no scroll; `Layout` stays `Copy`, `const fn new` sets `None`), so a per-canvas layout override (U15) can scroll differently or not at all. It applies to source items of every type (text, image, colour, browser, media, capture). An item that places a scene is refused (`ScrollOnScene`, like `CropOnScene` and `StyleOnScene`). `Scroll` holds:
   - `direction`: `Left`, `Right`, `Up` or `Down`.
   - `speed`: canvas pixels per second, `1.0..=4000.0`.
   - `gap`: empty canvas pixels between two copies of the picture, `0.0..=16384.0`.
   - `repeat`: `Loop` (copies follow each other forever) or `Once` (the picture crosses the window once).
   - `start`: `Inside` (the picture starts where it is placed today) or `Outside` (it enters from the edge the motion comes from).
   - `fade`: soft edges in canvas pixels at both ends of the window, `0.0..=2048.0`; the node clamps it to half the window each tick, so no canvas-dependent check is needed at validation.
   - `ramp_ms`: how long a pause, resume or speed change eases, `0..=5000`, default 300, with `Easing::EaseInOut` (ADR-0100).
   The scene format version goes up by one at merge; a document without the field reads as no scroll. `scene-model.md` gets the field and the refusal.

2. **The item box is the window; everything is in canvas pixels.** Across the direction the picture fills the box with its aspect ratio kept (a horizontal scroll scales the picture to the box height); along the direction the box shows a window onto the picture and its copies. The crop (ADR-0058) cuts the picture first. Rotation and style (ADR-0053) apply to the window, so a rounded ticker keeps its corners while the text moves. The node converts canvas pixels to texture coordinates each tick from the texture size it already reads for the crop, so a picture that changes size (a text edit, a browser resize) keeps the scroll's canvas position: the offset is the position modulo the new period (picture length on the canvas + `gap`), with no jump in speed.

3. **The phase is closed form from an anchor on the master clock.** The host keeps, per scrolling item and canvas, a `ScrollRun`: `anchor` (master-clock `Timestamp`, integer nanoseconds), `position` at the anchor (f64 canvas pixels), and an optional speed change (`from`, `to`, `start`, `duration`). It is sent in `NodeLayer::scroll: Option<LayerScroll>` with each layer list, like a `Slide`. The node evaluates, in f64, `position(now) = position0 + distance(anchor, now)`, where the distance integrates the eased speed in closed form (for `EaseInOut`, `∫₀ᵘ (3x² − 2x³) dx = u³ − u⁴/2`, scaled by duration and speed difference; constant speed after it). It wraps the result to one period in f64 and hands only the fraction to the GPU as f32. There is no state in the node, so every compositor that draws the item (program, incoming transition, preview, a second canvas, a rebuilt node) shows the same offset at the same tick, and nothing accumulates over a long run. `now` before the anchor (a clock that steps back) saturates to the anchor.

4. **Pause, resume, speed and restart are list changes.** The host turns each into a new `ScrollRun` (re-anchored at the control-side master-clock time of the request, the old position evaluated with the same closed form, a speed change from the current to the new speed over `ramp_ms`; pause is a change to speed 0, restart a new anchor at `start`) and sends the next layer list as for any other edit. No separate command message exists, so no command can be skipped by the latest-value channel, and an unrelated edit re-sends the current run state rather than the saved speed.

5. **When a scroll starts.** A run is anchored when the item becomes shown on a canvas from not shown: at the time of the program switch for the program canvas (the switch time, so an incoming transition compositor and the program compositor agree), at the time it appears for the preview canvas, and when its `scroll` is first set or edited. A Take in studio mode starts the program's run anew at the switch (the preview's position is not carried over), as a lower third would. While the item stays shown, edits elsewhere and node rebuilds do not restart it. A hidden item or an item on a scene not shown is not listed and costs nothing. "Shown" means visible in a scene a canvas shows; opacity does not count, so an item faded to 0, which flattening leaves out of the list, keeps its run and continues where the closed form puts it when it fades back in.

6. **`Once` ends on the control side.** From the run the host knows when a `Once` picture has fully left the window; at that time (checked on the control thread's existing 1 Hz host tick and on each list send, not per frame) it marks the run finished, sends a list in which the layer draws nothing (the node also draws nothing for a position past the end, so the end is exact even before that list arrives), and publishes `item.scroll_finished`. A finished run stays finished until a restart, a scroll edit or the item being shown again.

7. **Four wrap pipelines and a scroll buffer, used only by scrolling layers.** The compositor has four pipelines today (plain, styled, yuv, yuv_styled). It gains a `wrap` variant of each, and a `scrolls` storage buffer beside `styles` and `yuvs` (one `ScrollGpu` record per scrolling layer: offset fraction, period and window in texture units, direction, gap, `Once` flag, fade), written only for layers with `scroll`. The wrap fragment stage maps the window position plus the offset into one period, draws transparent inside the gap and (for `Once`) outside the single copy, keeps every sample half a texel inside the crop of the plane being sampled (luma and, for 4:2:0, the half-size chroma plane each with its own texel size) so a seam never blends with the clamped edge, and multiplies the edge fade. A layer without `scroll` keeps today's pipelines, shaders and layer record. The sampler stays `ClampToEdge`.

8. **Render on change knows scrolling.** The draw key of a scrolling layer includes its wrapped offset, so a moving layer draws every tick and a paused or finished one (offset unchanged) lets the canvas be skipped again. Layers without `scroll` cost the same per-tick `Option` test as `fade` and `slide`; nothing else.

9. **Stillness and separate files.** Scroll is a layout property, not part of the source: `rbs_sources::is_still` does not change, and a separate-file (ISO) track of a scrolling text source records the unscrolled picture, as it records it without crop or style today. ADR-0117 decision 2's "a text source with a … ticker never [is still]" means a text whose own picture moves; a scrolling item does not make its source move. The canvas is not static while the item moves (decision 8), which is what ADR-0129 decision 9's "not still" protects.

10. **Long text.** The text source gains `single_line` (`text-port.md`, `scene-sources.md`): one line, no wrapping, the box width following the text like **Fit box to text**, within today's `MAX_SIDE = 4096` and area cap, so `reserve` and the box-sized textures of ADR-0129 decision 5 are unchanged (a width change is the existing one-allocation-per-size-change path). Text that does not fit is cut and reports `RenderNotes::cut_to_fit`, shown in Properties. Phase 2 raises the limit by folding a long line into rows of one texture (a 20 000 x 60 line becomes rows of 4096 x 60 stacked into one 4096 x 300 texture); the text contract then gets a new cap, and the wrap shader unfolds rows from a row count in `ScrollGpu`, sampling half a texel inside each row so bilinear filtering never mixes rows. Phase 2 has its own PR and measurement.

11. **Control API.** `item.set_scroll {item, scroll | null, canvas?}` sets or clears the saved scroll (one undo step, `source.changed`), with the `canvas` rule of the other item edits (`"main"` default, `"vertical"`). Live calls, not saved and not undoable (like ADR-0105's show): `item.scroll_pause`, `item.scroll_resume`, `item.scroll_restart`, `item.scroll_speed {speed}`, each `{item, canvas?}`; a live speed lasts until the item is hidden, its scroll is edited or the app quits. Event `item.scroll_finished {item, canvas}`. Errors use the existing kinds: `not_found` (unknown item), `invalid_params` (out-of-range value, a scene item, unknown keys, a live call on an item without scroll), `conflict` (the canvas rules). The minor API version goes up at merge; `control-api.md` gets the rows.

12. **UI.** Properties gets a **Scroll** section on the item (closed by default, `FeatureSection`) with Direction, Speed (`NumberField`, live), Gap, Repeat, Start, Edge fade and a Pause/Resume button; the Text tab gets a **One line** toggle. Glossary term: "Scroll". The layout and dark and light screenshots are proposed to the owner before the UI PR merges (ADR-0115).

13. **Measured before it merges (P15).** The compositor PR shows, with `cargo bench -p rbs-nodes --bench render_on_change` and `benches/compositor.rs` extended: a 1080p tick with no scrolling layer against `main` (equal within noise), with one scrolling text layer and with eight (plain and 4:2:0); the CPU cost of evaluating 64 runs; a paused scroll skipped by render on change. Tests with `TestClock`: the offset after 86 400 s and after 30 days equals the exact value; a speed change and a pause are continuous in position and speed; two compositors given one run show the same offset; a picture size change keeps the canvas position. Short runs on the Mac and SC-PC. It fills the `hot-path-review` checklist and needs Salih's sign-off.

## Consequences

- Any source scrolls, not only text: a logo strip, a browser page, an image of credits. OBS needs a filter per source; here it is a layout field with one shared implementation and no extra pass or texture.
- Speed changes and pauses ease instead of jumping; a ticker runs for weeks without drift; every picture of the same item agrees, through transitions and studio mode; a `Once` scroll reports when it ends.
- A moving ticker makes its canvas draw, convert and read back every tick while it moves, the expected cost of a moving picture. Pausing it brings render on change back.
- The compositor gains four pipelines, a scroll buffer and a closed-form evaluation per scrolling layer; the host keeps one `ScrollRun` per scrolling shown item.
- Specs in the same PRs: `gpu-compositor.md`, `scene-model.md`, `scene-sources.md`, `text-port.md`, `control-api.md`, `docs/ui/glossary.md`.
- Tests (decision 13), plus golden images of the wrap pipelines (offset, gap, seam, `Once`, fade, styled window, 4:2:0), scene round-trip and refusal, API conformance.
- Phases: (a) layout field, run evaluation, wrap pipelines, render on change, specs, measurements; (b) control API; (c) text `single_line`; (d) UI with screenshots; (e) folded long text.

## Alternatives considered

- **Animated video filter (`org.rbs.filter.scroll`).** Works without touching the compositor, but adds one full-size pass and one texture per scrolling source per tick, its output is the source's size so the item box cannot be a window onto a wider picture, and every item of a shared source moves alike. ADR-0129 decision 9 asked for measurements of both designs; the owner chose the compositor design on 2026-11-18 without the comparison (the filter costs at least the wrap pipeline plus that pass), and this ADR records that waiver.
- **Phase kept as state in the compositor node** (the first draft). Integrating `speed x dt` per tick is simple, but each compositor (transition, preview, second canvas, rebuilt node) restarts or disagrees, and a sum over millions of ticks drifts from the exact value.
- **A separate control message for pause and speed.** The latest-value parameter channel can skip it, and it races with layer lists.
- **Repeat sampler (`AddressMode::Repeat`).** Needs a second sampler and bind group, cannot draw a gap or `Once`, and repeats the crop's neighbours, not the crop.
- **Re-render a moving text window on the CPU.** Uploads a new picture every tick, costing CPU and upload bandwidth that the GPU offset avoids.
- **Scroll in the source's settings.** Every item of the source would move alike and per-canvas overrides would be impossible.

## Review

Independent review per ADR-0014.

Reviewer: independent AI session (round 1)
Date: 2026-11-18
Verdict: changes requested
Concerns and resolutions:
1. A `ScrollControl` message on a latest-value channel can be lost and races with lists. -> Decision 4: run state is part of the list; no command message.
2. Phase inside one compositor: transitions, studio mode, global scenes and node rebuilds restart or disagree. -> Decision 3: closed form from a host anchor on the master clock; decision 5: when a run starts.
3. Units and picture size changes; window size not known on the control side. -> Decision 2: canvas pixels, node converts from the texture each tick, position kept modulo the new period.
4. Four pipelines exist (plain, styled, yuv, yuv_styled); where scroll parameters go; chroma seam. -> Decision 7: four wrap variants, `scrolls` buffer, per-plane half-texel rule.
5. `single_line` above 4096 breaks the text contract. -> Decision 10: phase 1 stays within `MAX_SIDE` and the area cap; phase 2 changes the contract explicitly.
6. Departures from ADR-0129 d9 and ADR-0117 d2 unstated. -> Alternatives: owner waiver recorded; decision 9: scroll is layout, ISO records the unscrolled picture.
7. `Once` end and its event have no path back. -> Decision 6: the host computes the end and publishes the event.
8. Key not unique for nested scenes. -> No key needed: the run travels with the layer (decision 3).
9. API names and error kinds do not match the spec; saved or live unclear. -> Decision 11: `item.set_scroll`, existing error kinds, live calls not saved.
10. Scene validation and fade bound. -> Decision 1: `ScrollOnScene`, fade clamped in the node.
11. Zero-cost wording. -> Decision 8 states the `Option` test.
12. Precision test and clock edge cases. -> Decisions 3 and 13: exact closed form, saturating clock.
13. Easing, opacity, folded-row seams undefined. -> Decision 1 (`EaseInOut`), decision 10 (row seams); an item faded to 0 is left out of the list but keeps its host run (decision 5), so it does not restart.
