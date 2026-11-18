# ADR-0150: Scrolling is a layer motion evaluated by the compositor node, for every source

- Status: Proposed
- Date: 2026-11-18
- Deciders: Salih (owner)
- Answers: [ADR-0129](0129-text-source.md) decision 9 (scrolling decided separately)
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0023](0023-node-parameter-channels.md), [ADR-0053](0053-item-style-and-video-filter-chains.md), [ADR-0100](0100-configurable-glide-duration-and-easing.md), [ADR-0117](0117-still-sources-as-images.md), [ADR-0143](0143-performance-rules-for-every-change.md) (R5, render on change)

## Context

A ticker, a news crawl, end credits and a subtitle line that runs across the screen all need a picture that moves through a window. RBS has a text source (ADR-0129) but nothing moves it, so the owner asked on 2026-11-18 whether a text source can scroll. OBS does this with a Scroll filter on any source.

Forces:

- **ADR-0129 decision 9** fixed the requirements: keep the phase in f64 on the CPU, integrate speed x dt and send only the wrapped offset (no f32 drift on 24/7 streams, no jump when the speed changes), draw nothing for an off-air item, make the picture not static while it moves, and handle a line wider than one texture. It named two designs: a per-layer UV offset with repeat in the compositor, and an animated video filter.
- **The compositor node already animates layers.** `NodeLayer` carries a `Fade` and a `Slide` that the node evaluates at each tick's time from targets sent once (ADR-0023 decision 7, `gpu-compositor.md` "Compositor node" step 4). A scroll is the same kind of thing: a motion of what a layer shows, known in advance.
- **Render on change (R-03, ADR-0143 R5).** An unchanged tick costs one compare per layer and no GPU work. A moving layer must make its canvas draw every tick, and only while it moves.
- **P6, P13, P15.** Nothing allocates in `Node::process`; a layer that does not scroll must not pay for scrolling (no extra branch per pixel, no per-tick check of a flag that is off); the compositor is a hot path, so the change needs measurements and the owner's sign-off.
- **Texture size.** A one-line ticker can be far wider than one texture. `max_texture_dimension_2d` is 8192 on the desktop adapters RBS targets (the `gpu.max_texture_dimension` capability reports it), less on some software adapters.
- **Owner rules.** Feature parameters are saved settings and reachable through the control API (Stream Deck parity). A visual change is approved with dark and light screenshots before merge. Beyond-OBS behaviour is welcome (vision, rule 17).

## Decision

1. **Scroll is part of an item's layout.** `Layout` gains `scroll: Option<Scroll>` (absent: no scroll), so a per-canvas layout override (U15) can scroll differently or not at all. It applies to source items of every type: text, image, colour, browser, media, capture. `Scroll` holds:
   - `direction`: `Left`, `Right`, `Up` or `Down`.
   - `speed`: canvas pixels per second, `1.0..=4000.0`.
   - `gap`: empty canvas pixels between two copies of the picture, `0.0..=16384.0`.
   - `repeat`: `Loop` (copies follow each other forever) or `Once` (the picture crosses the window once, then the item draws nothing until restarted).
   - `start`: `Inside` (the picture starts where it is placed today) or `Outside` (it enters from the edge the motion comes from).
   - `fade`: soft edges in canvas pixels at both ends of the window along the direction, `0.0..=` half the window.
   - `ramp_ms`: how long a pause or a speed change eases in, `0..=5000`, default 300.
   The scene format version goes up by one at merge; a document without the field reads as no scroll.

2. **The item box is the window.** Across the direction the picture fills the box with its aspect ratio kept (a horizontal scroll scales the picture to the box height); along the direction the box shows a window onto the picture and its copies. The crop (ADR-0058) still cuts the picture first. Rotation and style (ADR-0053) apply to the window, so a rounded ticker keeps its corners while the text moves.

3. **The node owns the phase.** `NodeLayer` gains `scroll: Option<LayerScroll>`: the settings above, the window size in picture pixels and a `key: u64` (scene item and canvas) that names the motion across list changes. The compositor node keeps one state per scrolling layer in a buffer reserved in `prepare` (no allocation): phase in f64 picture pixels, current speed, target speed, the tick time of the last step. Each tick it advances `phase += speed * dt` with `dt` from the tick's `Timestamp` (integer nanoseconds), eases the speed towards its target over `ramp_ms`, and wraps the phase to `[0, picture + gap)` in f64. Only the wrapped offset, as a fraction of one period, reaches the GPU as f32. A new layer list keeps the state of every key it still lists (a speed edit ramps, it never jumps); a new key starts at `start`. Looking up keys is a linear search over the previous states, bounded by the node's capacity.

4. **Control without a new list.** Pause, resume, a speed change and restart go through the same parameter channel as layer lists, as one small `ScrollControl { key, action }` message (ADR-0023); they do not rebuild the list or the node. Pause eases the speed to zero over `ramp_ms` and keeps the phase.

5. **One extra pipeline, used only by scrolling layers.** The compositor gains a `wrap` variant of its plain and styled pipelines (as `yuv_styled` exists beside `styled`). Its fragment stage maps the window position plus the offset into one period, draws transparent inside the gap and in `Once` mode outside the single copy, samples within half a texel of the picture's crop so a seam never blends with the clamped edge, and multiplies the edge fade. A layer without `scroll` keeps today's pipelines, shaders and record size: non-scrolling scenes draw exactly as before. The sampler is unchanged (`ClampToEdge`).

6. **Render on change knows scrolling.** A layer whose speed is not zero (moving or easing) changes the draw key every tick, so its canvas draws every tick. A paused or finished scroll changes nothing, and a canvas whose only motion is paused is skipped again (zero GPU work). No per-tick flag is tested for layers without `scroll`: the node checks the scroll states it holds, and holds none when no layer scrolls.

7. **Off air costs nothing.** Only listed layers of a drawn canvas scroll. An item that is hidden, on a scene not shown on any canvas, or `Once` and finished, has no state and no draw. When the item is listed again it starts at `start` (a ticker that comes back on air starts fresh, as a lower third would). Studio mode's preview canvas scrolls its own copy with its own phase.

8. **Long pictures.** A picture up to `gpu.max_texture_dimension` scrolls as one texture. The text source gains a `single_line` setting (`text-port.md`, `scene-sources.md`): one line, no wrapping, the picture as wide as the text. Phase 1 caps it at `gpu.max_texture_dimension` and reports `RenderNotes::cut_to_fit` (shown in Properties) beyond it. Phase 2 folds a longer line into rows of one texture (a 20 000 x 60 picture becomes 8192 x 180) and the `wrap` shader unfolds it from a row count in the layer record; it is part of this decision but ships in its own PR with its own measurement.

9. **Control API.** `item.set_layout` carries `scroll` like the other layout fields. New calls: `item.scroll_pause`, `item.scroll_resume`, `item.scroll_restart`, `item.scroll_speed { speed }` (each with `canvas`, optional). Event `item.scroll_finished` when a `Once` scroll ends (so a macro or Stream Deck can hide the item or switch scenes). Errors are typed (`NotScrolling`, `OutOfRange`); the minor API version goes up at merge.

10. **UI.** Properties gets a **Scroll** section on the item (closed by default, `FeatureSection`), with Direction, Speed (`NumberField`, live), Gap, Repeat, Start, Edge fade and a Pause/Resume button; the Text tab gets a **One line** toggle. Glossary term: "Scroll". The layout and screenshots in dark and light are proposed to the owner before the UI PR merges (ADR-0115).

11. **Measured before it merges (P15).** The compositor PR shows, with `cargo bench -p rbs-nodes --bench render_on_change` and `benches/compositor.rs` extended: a 1080p tick with no scrolling layer against `main` (must be equal within noise), with one scrolling text layer, and with eight; the CPU cost of stepping 64 scroll states; a 24-hour simulated run with `TestClock` showing the offset after 86 400 s equals the closed-form value to 1e-6 picture pixels; a paused scroll skipped by render on change. Short runs on the Mac and SC-PC. It fills the `hot-path-review` checklist and needs Salih's sign-off.

## Consequences

- Any source scrolls, not only text: a logo strip, a browser page, an image of credits. OBS needs a filter per source; here it is a layout field with one shared implementation and no extra pass or texture.
- Speed changes and pauses ease instead of jumping, and a ticker runs for weeks without drift. A `Once` scroll reports when it ends.
- A moving ticker makes its canvas draw, convert and read back every tick while it moves. That is the expected cost of a moving picture; pausing it brings render on change back.
- The compositor gains two pipelines and the layer record a variant; the node gains a bounded state buffer. Spec updates in the same PRs: `gpu-compositor.md` (layer, node, wrap pipeline, render-on-change key), `scene-model.md` (`scroll`), `scene-sources.md` and `text-port.md` (`single_line`, `cut_to_fit`), `control-api.md` (calls, event, errors), `docs/ui/glossary.md` ("Scroll").
- Tests: unit tests of the phase step (wrap, ramp, `Once`, key carry-over, speed change continuity) with `TestClock`; golden images of the wrap pipeline (offset, gap, seam, fade, styled window); render-on-change tests (moving draws, paused skips); scene round-trip; API conformance.
- Phases: (a) layout field, node state, wrap pipeline, render-on-change, specs, measurements; (b) control API; (c) text `single_line`; (d) UI with screenshots; (e) folded long pictures.

## Alternatives considered

- **Animated video filter (`org.rbs.filter.scroll`).** Works without touching the compositor, but adds one full-size pass and one texture per scrolling source per tick, its output is the source's size so the item box cannot be a window onto a wider picture, and a filter on a shared source moves every item of it the same way. The owner chose the compositor design on 2026-11-18. The filter would cost at least what the wrap pipeline costs plus that pass, so it was not built for comparison.
- **Repeat sampler (`AddressMode::Repeat`).** Needs a second sampler and bind group, cannot draw a gap or `Once`, and repeats the crop's neighbours, not the crop.
- **Phase on the control side, one layer list per tick.** Sends a list every frame (ADR-0023 says targets with timing are sent once) and ties smoothness to the control thread's timing.
- **Re-render a moving text window on the CPU.** Uploads a new picture every tick; costs CPU and upload bandwidth that the GPU offset avoids.
- **Scroll in the source's settings.** Every item of the source would move alike and the window would be the source's size; per-canvas overrides would be impossible.

## Review

Independent review per ADR-0014. Record: reviewer, date, verdict, main concerns and how each was resolved.
