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
   - `speed`: layout units (decision 2) per second, `1.0..=4000.0`.
   - `gap`: empty layout units between two copies of the picture, `0.0..=16384.0`.
   - `repeat`: `Loop` (copies follow each other forever) or `Once` (the picture crosses the window once).
   - `start`: `Inside` (the picture starts where it is placed today) or `Outside` (it enters from the edge the motion comes from).
   - `fade`: soft edges in layout units at both ends of the window, `0.0..=2048.0`; the node clamps it to half the window each tick, so no canvas-dependent check is needed at validation.
   - `ramp_ms`: how long a pause, resume or speed change eases, `0..=5000`, default 300, with `Easing::EaseInOut` (ADR-0100).
   For ADR-0133, `scroll` is an item attribute group of its own, `scroll` (UI name "Scroll"; default none; source items only), so Reset source clears it and Paste attributes copies it; `item.fit`, `item.align` and `keep_scale` crops ignore it. The scene format version goes up by one at merge; a document without the field reads as no scroll. `scene-model.md` gets the field and the refusal.

2. **The item box is the window; lengths are layout units.** Across the direction the picture fills the box with its aspect ratio kept (a horizontal scroll scales the picture to the box height); along the direction the box shows a window onto the picture and its copies. The crop (ADR-0058) cuts the picture first. Rotation and style (ADR-0053) apply to the window, so a rounded ticker keeps its corners while the text moves. `speed`, `gap` and `fade` are in the units of the item's box size in its layout (ADR-0110 layout space), and are scaled with the box wherever the box is scaled (another canvas size, a nested scene's box), as `FlatLayer` already scales the box. `direction` is in the box's own frame: a rotated box scrolls along its rotated axis, and a flipped box mirrors the picture and its motion together. The node converts these units to texture coordinates each tick from the texture size it already reads for the crop. When the picture changes size (a text edit, a browser resize) the run keeps its position along the direction and wraps it into the new period (picture length + `gap`): speed is continuous, but what is under the window changes as the picture did. A test checks exactly that.

3. **The phase is closed form from an anchor.** The host keeps a `ScrollRun` per run key (decision 5): `anchor` (engine `Timestamp`, integer nanoseconds), `position` at the anchor (f64 layout units), and an optional speed change (`from`, `to`, `start`, `duration`). It reaches the node in `NodeLayer::scroll: Option<LayerScroll>` with each layer list, like a `Slide`. The node evaluates, in f64, `position(now) = position0 + distance(anchor, now)`, where the distance integrates the eased speed in closed form (for `EaseInOut`, `u²(3 − 2u)`, `∫₀ᵘ = u³ − u⁴/2`, scaled by duration and speed difference; constant speed after it). For `now` before the anchor it extrapolates at the `from` speed, so a re-anchor at the control side's `clock.now()` stays continuous in position and speed (when the run before was still easing, the difference is the lead time times the change of speed, below a pixel at the control lead times RBS has); a fresh start (no run before it) holds at its start position until the anchor. The result is wrapped to one period in f64, and only the fraction reaches the GPU as f32. The node keeps no scroll state, so every compositor that draws the run shows the same offset at the same tick, and nothing accumulates over a long run.

4. **Pause, resume, speed and restart are list changes.** The host turns each into a new `ScrollRun` (re-anchored at the control-side engine time of the request, the old position evaluated with the same closed form, a change from the current to the new speed over `ramp_ms`; pause is a change to speed 0, restart a new anchor at `start`) and sends the next layer list as for any other edit. No separate command message exists, so the latest-value channel cannot skip one, and an unrelated edit re-sends the current run rather than the saved speed. **Where it is carried:** `FlatLayer` and `BoundLayer` (`rbs-scene`) gain `scroll: Option<Scroll>` (from the layout, scaled as in decision 2) and its run key; `scene-model.md`'s "Graph binding" section gets the `BoundLayer` field. `rbs_sources::layers::node_layers` and `glide_layers` take `runs: &ScrollRuns` and fill `NodeLayer::scroll` (`gpu-compositor.md` gets that field). A pause or speed change re-sends the lists from the current plans without a new sync.

5. **Run keys, and when a run starts.** The runs live in `rbs-record`'s `Studio`, which builds and sends every list (program, preview, transitions, hold scenes of ADR-0124, replay) and owns the engine clock; `rbs-app` forwards live calls to it and receives its finished events. A run key is `(canvas, source id, scaled scroll settings, cross-axis box size)`: the settings and box size as `FlatLayer` carries them after scaling (decision 2), compared by `to_bits` after normalising `-0.0` to `0.0` (values are finite by validation), so a run's position and period have one scale. Every compositor drawing that key on that canvas uses one run: program, preview in studio mode, an incoming transition compositor, the outgoing snapshot, a hold scene's compositor (`Role::Hold`) and the replay compositor. So a Take or a scene switch does not restart a ticker whose source is on both scenes with the same scroll (beyond OBS: a ticker placed in every scene runs on through scene changes, also while its box glides, as long as the box keeps its size across the direction; a glide to another cross-axis size starts a new run), and two items of one source with the same scroll and the same cross-axis size on one canvas show the same position. The host learns the keys from `rbs_scene::scroll_keys_cued(canvas, scene, cued)`, beside `visible_sources_cued`, which also returns items at opacity 0. A run is created when `Studio` builds the first list on that canvas that contains its key (program, preview or an incoming compositor for a T-bar or timed transition), anchored at the control-side engine time then; it ends when no list on that canvas contains the key, and a key seen again starts a new run at `start`. "Contains" counts visible items of shown scenes and items a cue draws (ADR-0105), and also an item faded to opacity 0, which flattening leaves out of the list: the host keeps its run so it does not restart when it fades back in. A studio restart that builds a new engine with a new clock (a canvas format change) hands the runs from the old `Studio` to the new one and rebases every run the way `text_elapsed.moved(old_now, new_now)` rebases text timers: every anchor and ramp start moves by `new_now - old_now`, and a `Once` wake is armed again on the new clock, with a test. A layer whose key has no run when `sync*` is called draws without scroll (debug assertion, tested). A hidden item, or a scene no canvas shows, costs nothing.

6. **`Once` ends exactly, and is reported when known.** The node draws nothing for a position past the single copy, so the end on screen is exact. When the host knows the picture's length (`picture_size` has been seen), it computes the end time from the run and sets a one-shot wake for it (as ADR-0105 decision 4 does for cues; no polling); at the wake it marks the run finished and publishes `item.scroll_finished`. When the size is not known yet (a media source before its first picture), it computes the end when the size arrives; the event may then come late, never early. A size change re-computes the end. A finished run stays finished until a restart, a scroll edit or a new run (decision 5).

7. **Wrap pipelines are built only when something scrolls.** The compositor has four pipelines today (plain, styled, yuv, yuv_styled), and each `Compositor::new` builds its own layer bind group layout and shader module. This decision moves the layer layout to `GpuContext`, beside `sampled_layout` and `yuv_layout` (built once; `Compositor::new` uses it), and adds a **wrap set** kept in a `OnceLock` on `GpuContext`: its own shader module, a `wrap` variant of each of the four pipelines with a device-wide pipeline layout (groups 0 and 1 as the compositor's, group 2 for scroll data). The set is built with `get_or_init` on the control thread before `Studio` sends the first list with a scrolling layer to any compositor of that device; `process` only calls `get()`. The control side then builds, for each compositor that will draw a scrolling layer, a `ScrollBinding` (an `Arc` holding a storage buffer of `MAX_SCROLL_LAYERS = 64` `ScrollGpu` records, 256-byte aligned, and its group 2 bind group) and sends it through a second latest-value parameter channel of the node before that list; the value is idempotent, so a skipped update is harmless. Each wrap draw selects its record with a dynamic offset on group 2 (no allocation), so `LayerGpu` stays unchanged. `ScrollGpu` holds the offset fraction, period and window in texture units, direction, gap, `Once` flag and fade; the node writes the records of its scrolling layers before its own submit, which queue order makes safe. A list with more than 64 scrolling layers draws the extra ones without scroll and counts them (`RenderStats::unscrolled_layers`). A layer with scroll whose compositor has no wrap set or no `ScrollBinding` yet (a race, a `sibling()` context, a lost device) is drawn without scroll and counted; the node never fails for it. The `GpuContext` that holds the set is the shared `Arc` kept across `Studio::start_with_tree`, so the set survives a studio restart. A device on which nothing ever scrolled has no wrap set and no binding (P13), and building a compositor costs what it costs today. The wrap fragment stage maps the window position plus the offset into one period, draws transparent inside the gap and (for `Once`) outside the single copy, keeps every sample half a texel inside the crop of the plane being sampled (luma and, for 4:2:0, the half-size chroma plane each with its own texel size) so a seam never blends with the clamped edge, and multiplies the edge fade. A layer without `scroll` keeps today's pipelines, shaders and layer record. The sampler stays `ClampToEdge`.

8. **Render on change knows scrolling.** The node's existing `moving` flag, evaluated at the last draw's tick time as it is for fades and slides (`drawn_at <= end`), is set while a listed run's speed at that time is above 0, `drawn_at` is before a fresh run's anchor and its target speed is above 0, `drawn_at` is not past its ramp's end, or `drawn_at` is not past the time its `Once` copy leaves the window (from the texture size), so one draw always follows the end of a pause ramp or a `Once` exit, so the canvas draws every tick while something moves and is skipped again when every run is paused or finished. The draw key is not widened. Layers without `scroll` cost the same per-tick `Option` test as `fade` and `slide`, and nothing else.

9. **Stillness and separate files.** Scroll is a layout property, not part of the source: `rbs_sources::is_still` does not change, and a separate-file (ISO) track of a scrolling text source records the unscrolled picture, as it records it without crop or style today. ADR-0117 decision 2's "a text source with a … ticker never [is still]" means a text whose own picture moves; a scrolling item does not make its source move. The canvas is not static while the item moves (decision 8), which is what ADR-0129 decision 9's "not still" protects.

10. **Long text.** The text source gains `single_line` (`text-port.md`, `scene-sources.md`): one line, no wrapping, the box width following the text like **Fit box to text**, within today's `MAX_SIDE = 4096` and area cap, so `reserve` and the box-sized textures of ADR-0129 decision 5 are unchanged (a width change is the existing one-allocation-per-size-change path). Text that does not fit is cut and reports `RenderNotes::cut_to_fit`, shown in Properties. Phase 2 raises the limit by folding a long line into rows of one texture (a 20 000 x 60 line becomes rows of 4096 x 60 stacked into one 4096 x 300 texture); the text contract then gets a new cap, and the wrap shader unfolds rows from a row count in `ScrollGpu`, sampling half a texel inside each row so bilinear filtering never mixes rows. Phase 2 has its own PR and measurement.

11. **Control API.** `item.set_scroll {item, scroll | null, canvas?}` sets or clears the saved scroll (one undo step, `source.changed`), with the `canvas` rule of the other item edits (`"main"` default, `"vertical"`). Live calls, not saved and not undoable (like ADR-0105's show): `item.scroll_pause`, `item.scroll_resume`, `item.scroll_restart`, `item.scroll_speed {speed}`, each `{item, canvas?}`. A live call acts on the item's run on that canvas, so it moves every item that shares the run (decision 5), including the program's copy when the call names an item seen in the studio-mode preview; `control-api.md` and the Properties help say so. A live speed lasts as long as the run, until a scroll edit, a restart or the run's end. Event `item.scroll_finished {canvas, items}` is published once per run and lists every item that used it. Errors use the existing kinds: `not_found` (unknown item), `invalid_params` (out-of-range value, a scene item, unknown keys, a live call on an item without scroll), `conflict` (the canvas rules, or an item with scroll that has no run because no canvas shows it). The minor API version goes up at merge; `control-api.md` gets the rows.

12. **UI.** Properties gets a **Scroll** section on the item (closed by default, `FeatureSection`) with Direction, Speed (`NumberField`, live), Gap, Repeat, Start, Edge fade and a Pause/Resume button; the Text tab gets a **One line** toggle. Glossary term: "Scroll". The layout and dark and light screenshots are proposed to the owner before the UI PR merges (ADR-0115).

13. **Measured before it merges (P15).** The compositor PR shows, with `cargo bench -p rbs-nodes --bench render_on_change` and `benches/compositor.rs` extended: a 1080p tick with no scrolling layer against `main` (equal within noise), with one scrolling text layer and with eight (plain and 4:2:0); the CPU cost of evaluating 64 runs; a paused scroll skipped by render on change. Tests with `TestClock`: the offset after 86 400 s and after 30 days equals the exact value; a speed change and a pause are continuous in position and speed; two compositors given one run show the same offset; a re-anchor at a control time later than the tick does not jump; a picture size change keeps the position along the direction; a studio restart keeps runs; `Compositor::new` build time is unchanged on a device where nothing scrolls; the wrap set's build time on the control thread; a speed change during a ramp with a tick before the anchor. Short runs on the Mac and SC-PC. It fills the `hot-path-review` checklist and needs Salih's sign-off.

## Consequences

- Any source scrolls, not only text: a logo strip, a browser page, an image of credits. OBS needs a filter per source; here it is a layout field with one shared implementation and no extra pass or texture.
- Speed changes and pauses ease instead of jumping; a ticker runs for weeks without drift; every picture of the same item agrees, through transitions and studio mode; a `Once` scroll reports when it ends.
- A moving ticker makes its canvas draw, convert and read back every tick while it moves, the expected cost of a moving picture. Pausing it brings render on change back.
- The compositor gains four lazily built pipelines, a scroll buffer and a closed-form evaluation per scrolling layer; the host keeps one `ScrollRun` per run key; the binder gets the runs as an input.
- Specs in the same PRs: `gpu-compositor.md`, `scene-model.md` (field, refusal, attribute group, Graph binding), `scene-sources.md`, `text-port.md`, `control-api.md`, `docs/ui/glossary.md`.
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

Reviewer: independent AI session (round 2)
Date: 2026-11-18
Verdict: changes requested
Concerns and resolutions:
1. One run per item and canvas cannot hold program and preview. -> Decision 5: run key `(canvas, source, scroll)`, shared by program, preview, incoming and outgoing compositors.
2. Binder contract missing. -> Decision 4: `scroll` on `FlatLayer`/`BoundLayer`, `runs` argument to `sync*`.
3. Studio restart breaks anchors. -> Decision 5: rebase like `text_elapsed.moved`, tested.
4. Holding before the anchor jumps. -> Decision 3: extrapolate at the `from` speed; hold only for a fresh start.
5. Scene switches restart tickers; T-bar switch time undefined. -> Decision 5: runs keyed by source and settings carry across switches; a run starts at the first list that contains it.
6. "Canvas pixels" ambiguous; rotation and flips. -> Decision 2: layout units scaled with the box; direction in the box's frame.
7. Extra pipelines cost when nothing scrolls. -> Decision 7: built once per device, lazily, outside `Node::process`; build time measured.
8. `Once` end not computable; 1 Hz tick not always running. -> Decision 6: one-shot wake when the size is known, late but never early otherwise; exact on screen from the node.
9. Widened draw key. -> Decision 8: the `moving` flag, key unchanged.
10. Cued items. -> Decision 5 counts them.
11. ADR-0133 group. -> Decision 1: group `scroll`; fit, align and crop ignore it.
12. Size-change wording. -> Decision 2 reworded, test of the expected change.
13. Scroll buffer cap. -> Decision 7: `capacity` records.

Reviewer: independent AI session (round 3)
Date: 2026-11-18
Verdict: changes requested
Concerns and resolutions:
1. Shared wrap pipelines vs per-compositor bind groups; buffer size; missing set. -> Decision 7: device-wide layouts in a `OnceLock`, group 2 per compositor, `MAX_SCROLL_LAYERS`, draw without scroll when missing, set kept across restarts.
2. Live calls on shared runs. -> Decision 11: they act on the run and move every sharer (preview to program stated), lifetime tied to the run, `conflict` without a run, one event per run listing items.
3. Key and units for scaled instances. -> Decision 5: scaled settings plus cross-axis size, normalised `to_bits`.
4. Where keys come from; missing run in the binder. -> Decision 5: `scroll_keys_cued`; draw without scroll, debug assertion.
5. Extrapolation wording. -> Decision 3: continuous in position and speed; test added.
6. `moving` time rule. -> Decision 8: evaluated at `drawn_at`, one draw after each end.
7. Restart rebase details. -> Decisions 5 and 7.
8. Control-thread build stall. -> Decisions 7 and 13 measure it.

Reviewer: independent AI session (round 4)
Date: 2026-11-18
Verdict: changes requested
Concerns and resolutions:
1. `rbs-scene` cannot name `NodeLayer`; lists are built in `rbs_sources::layers`. -> Decision 4: `BoundLayer` carries scroll and key, `node_layers`/`glide_layers` take the runs; pauses re-send lists.
2. No non-real-time hook when a list arrives. -> Decision 7: control-built `ScrollBinding` through a second idempotent channel; drawn without scroll until it arrives.
3. Fresh-start hold stops render on change. -> Decision 8: `moving` while before a fresh anchor.
4. Run owner unclear; missing compositors. -> Decision 5: runs in `rbs-record` `Studio`, hold and replay compositors listed, runs handed over on restart.
5. Record lookup. -> Decision 7: dynamic offset on group 2.
6. Layer layout move and shader module. -> Decision 7 states it; `Compositor::new` build time measured.
7. Glide claim. -> Decision 5 reworded.
