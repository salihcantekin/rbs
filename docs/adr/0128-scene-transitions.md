# ADR-0128: Scene transitions: Cut, Fade, Move, Swipe, Slide and Stinger

- Status: Proposed
- Date: 2026-11-16
- Deciders: Salih (owner)
- Builds on: ADR-0013 (zero cost when unused), ADR-0023 (parameter channels; fades and slides evaluated on the node), ADR-0061 (studio mode), ADR-0063 and ADR-0098 (held program, transition keeps the preview scene), ADR-0064 (Transition rail), ADR-0074 (colour source), ADR-0091 (per-scene audio levels), ADR-0093 (further canvases and followers), ADR-0100 (glide duration and easing), ADR-0107 (temporal sources sleep when hidden), ADR-0108 (media-file source, Proposed)
- Supersedes in part: ADR-0100 decision 1, the sentence "These defaults apply to every program-scene change": the glide now applies when the selected transition is Move (decision 2 below). ADR-0061 decision 3, "Fade and other transitions are a later decision": this is that decision.

## Context

A program-scene change (`scene.switch`, `canvas.transition`, a timed-show cue, a follower canvas following) has one behaviour today. The program compositor gets the new scene's layers in one schedule swap. A source in both scenes slides to its new box and a source only in the new scene fades in, with the collection glide (ADR-0100, default 400 ms ease-in-out). A source only in the old scene disappears on the first frame, because the binder unlinks it in the same swap. The rail calls this "Cut", although it is not a cut while the glide is longer than 0 ms.

Operators coming from OBS expect a real cut, a crossfade (Fade), a fade through a colour, Swipe and Slide (the incoming scene pushes in), Move (shared sources morph), and Stinger (a video with alpha that covers the change). A crossfade cannot be built from per-layer opacity: two scenes of opaque, overlapping layers at 50 % each show what is behind both (a dip), so the incoming scene must be drawn whole into a texture first.

Forces:

- The compositor node is on the render hot path (P6, P15). Its `process` already evaluates `Fade` and `Slide` per layer, allocation-free, and it accepts any video frame on its "layers" pin, including another compositor's canvas.
- In studio mode a second compositor already draws the preview scene, the scene that goes on air next (ADR-0061). Outside studio mode nothing draws the next scene before the switch.
- Unused features must cost nothing (P13): no texture, node, thread or per-frame check for a transition that is not running.
- Studio mode, the held program (ADR-0063/0098), timed-show cues (ADR-0105), hold scenes (ADR-0124), projectors (ADR-0125) and follower canvases (ADR-0093) all read or change the program and must keep working.
- The scene format and the control API are contracts (ADR-0014). Stinger needs a media-file source (ADR-0108), which is still Proposed.

## Decision

### Model

1. **Kinds.** `rbs-scene` gains `TransitionKind` (`#[non_exhaustive]`): `Cut`, `Fade`, `Move`, `Swipe`, `Slide`, `Stinger`. `SceneCollection` gains `transition: CollectionTransition { kind, fade: FadeSettings, swipe: SwipeSettings, slide: SwipeSettings, stinger: Option<StingerSettings> }`. Each kind keeps its own settings, so switching kind does not lose a tuned value. Move's settings are the existing `glide` (ADR-0100), not a copy.
   - `FadeSettings { duration_ms, easing, via: Option<[u8; 4]> }`: `via` is a colour to fade through (premultiplied sRGB RGBA, like item borders); `None` is a crossfade.
   - `SwipeSettings { duration_ms, easing, direction: Left | Right | Up | Down }`.
   - `StingerSettings { source: SourceId, cut_ms: Option<u32>, under: Cut | Fade }` (decision 12).
   - Bounds: `duration_ms` in `0..=MAX_GLIDE_MS` (5000), the `GlideEasing` enum of ADR-0100. Defaults: Fade 300 ms linear, no `via`; Swipe and Slide 500 ms ease-in-out, `Left`.
   - One undoable edit, `set_transition(CollectionTransition)`, refused with `InvalidTransition` when out of range or when the stinger source is not a media source.
2. **What each kind does.** Duration 0 is a cut for every kind.
   - **Cut:** the new layers on the next frame. No glide, no fade-in, whatever `glide` says.
   - **Fade:** the incoming scene, drawn whole, goes from transparent to opaque over the outgoing scene. With `via`, a colour layer covers the outgoing scene during the first half and the incoming scene appears over the colour during the second half.
   - **Move:** today's behaviour (ADR-0100, slides and fade-ins with the collection glide and the per-source overrides), and a source only in the outgoing scene now fades out over the glide duration instead of vanishing.
   - **Swipe:** the incoming scene slides in from the edge opposite `direction`, over the outgoing scene, which stays still.
   - **Slide:** like Swipe, and the outgoing scene moves out by the same offset at the same time.
   - **Stinger:** decision 12.
3. **Scene document format 19.** Root table `transition = { kind, fade = {...}, swipe = {...}, slide = {...}, stinger = {...} }`, with defaults omitted when writing. Formats up to 18 load with `kind = "move"` when `glide.duration_ms > 0` and `"cut"` when it is 0, so an existing show looks the same after the upgrade. Older builds refuse format 19 without touching the file (P9). What kind a new collection starts with is the owner's choice (open question 1).
4. **Per scene: later.** An override "transition into this scene" (`Scene.transition_in: Option<TransitionKind>`) is phase 3. It is in this ADR so the format reserves the key now; format 19 refuses it until phase 3 ships.

### Running a transition

5. **Where it is drawn.** The program compositor stays the only node the outputs, the preview picture, projectors, hold scenes and the replay buffer read. A transition changes only what that compositor's layer list holds, for the transition's length, and then the graph returns to today's shape. `CompositorNode::process` is not changed: it already accepts a canvas frame as a layer and already evaluates `Fade` and `Slide`.
   - **Move** needs no texture: the program compositor draws the outgoing scene's leaving layers (with a `Fade` to 0) under the incoming scene's layers (with today's slides and fade-ins).
   - **Fade, Swipe, Slide and Stinger with `under = Fade`** draw the outgoing scene's layers as today, and on top one more layer: the canvas of an *incoming compositor*, a second compositor node drawing the incoming scene at the canvas size. Fade gives that layer a `Fade` from 0 to 1; Swipe gives it a `Slide` from one canvas width or height off the edge; Slide also gives each outgoing layer a `Slide` by the opposite offset. `via` adds a full-canvas colour layer between them with its own `Fade`.
   - In studio mode the incoming compositor is the preview compositor that already draws the preview scene (ADR-0061), so no node is added. Outside studio mode the studio adds one for the transition and removes it in the swap that ends it, the way the scene preview is added and removed today.
6. **Both scenes run for the transition's length.** The binder takes the outgoing scene as a second program scene while a transition runs, so the sources only in it stay linked and keep producing until the end. When the transition ends, one schedule swap gives the program compositor the incoming scene's own layers (exactly what the incoming compositor drew, so the last blended frame and the first plain one are the same picture), unlinks the outgoing-only sources (they go to Standby or sleep as ADR-0107 says) and removes an added incoming compositor.
7. **It starts when the picture is ready.** A transition starts on the first tick at which the incoming compositor has drawn every layer whose producer is linked, or after `READY_WAIT` (250 ms) at the latest, so a camera that is still opening never causes a flash of black. In studio mode the preview scene is already running, so it starts on the next tick. `program` changes in the model, and `scene.changed` is published, when the command is accepted; the picture follows when the transition starts. Time comes from the engine clock (tests use `TestClock`).
8. **One at a time.** A program change while a transition runs ends the running one at once (its incoming scene becomes the outgoing scene of the new one); the new one then starts as usual. Leaving studio mode, a collection switch or stopping the studio ends it at once too. Undo and redo never start a transition (ADR-0061 decision 5).
9. **Sound follows the picture.** For the transition's length the mixer holds the strips of both scenes: a source only in the outgoing scene ramps to silence, one only in the incoming scene ramps up from silence, and a shared source ramps from its outgoing level to its incoming level (ADR-0091), all over the transition's duration with the mixer's existing ramps (the ramp time is a parameter on the strips message, no new per-sample work). Cut keeps today's short default ramp.
10. **Every canvas.** Each canvas whose program changes in the event runs the same kind with the same settings. Swipe and Slide offsets are in that canvas's own size, so a 9:16 canvas swipes its own width. A follower changes when the followed canvas's transition starts (ADR-0093).
11. **Zero cost.** Outside a transition there is no incoming compositor outside studio mode, no added layer, no colour layer, no extra strip and no per-frame check: the studio keeps `Option<RunningTransition>` on the control thread and the end is a scheduled step in `Studio::service`, as the glide's end is today. During a transition the cost is one more compositor pass at canvas size (outside studio mode) and one more full-canvas layer (both modes).

### Stinger (phase 4, after ADR-0108)

12. **Stinger.** The stinger is a media source (ADR-0108) that the collection holds but no scene shows. While Stinger is not the selected kind it is Installed and costs nothing. When the transition starts, the media node is linked as the top layer of the program compositor, plays once from 0 (`loop = false`, `restart_when_shown = true`), and its sound goes to the mixer as its own strip. At `cut_ms` the layers under it change: with `under = Cut` in one swap, with `under = Fade` as a 200 ms Fade. The transition ends when the media ends. If `cut_ms` is `None`, the app finds the cut point once when the file is chosen: the first frame where the picture covers at least 99 % of the canvas (from the alpha), else the middle of the file; it stores the result in `cut_ms` and shows it, so the user can correct it.
    - Optional, off by default and explained in the UI with its cost: "Get ready in advance", which keeps the stinger in Standby with its first frames decoded while studio mode is on and Stinger is selected, so the first frame is on time. Without it the transition waits for the first frame under decision 7.

### Control API (version decided at merge; 1.68 if nothing else takes it)

13. **Methods.** `collection.set_transition { kind?, fade?, swipe?, slide?, stinger? }` (`control`, one undo step, merges the given parts). `canvas.transition` and `scene.switch` take optional `kind` and `duration_ms` for this transition only (a Stream Deck button "Fade 1 s" needs no settings change). `scene.list` and `scene.changed` carry `transition` on the collection snapshot. A new topic `transition.state { canvas, state: "started" | "ended", kind, duration_ms }` is published at the start and the end, never per frame. Older servers ignore the optional fields; clients check the protocol version first (minor rule).

### UI (proposed separately, screenshots before merge)

14. **Rail.** The rail's existing Select lists Cut, Fade, Move, Swipe, Slide (Stinger once phase 4 ships). The Duration field shows the selected kind's duration and is disabled for Cut. Kind settings that are not duration (Easing, Direction, Through colour, the stinger file and cut point) open from a small settings button next to the Select, with help behind (i). Tokens only, compact (ADR-0064 sizes). Outside studio mode, where there is no rail, the same choice is in the Scene menu. The exact layout is a UI proposal for the owner (rule 16), not part of this ADR.

### Phases

15. Phase 1: model and format 19, Cut, Fade (with `via`), sound follows picture, ready gate, control API, rail Select and Duration. Phase 2: Move fade-outs, Swipe, Slide, settings popup. Phase 3: manual T-bar (`canvas.transition_progress { t }` and a T-bar on the rail, its own proposal), the per-scene override, and Rehearse (play the transition on the Preview picture only, without going on air). Phase 4: Stinger, after ADR-0108 is accepted and built.

### Tests and measurement

16. Model: format 18 → 19 mapping (glide 0 → Cut, else Move), round-trip, bounds, undo. Studio (with `TestClock`, by tick index): a Fade's added layer and its removal at the end; the program compositor's layer list after the end equals a plain switch; both scenes' sources linked during and only the incoming after; the ready gate starts on readiness and on `READY_WAIT`; a second change mid-transition; leaving studio mode mid-transition; Swipe offsets on a 9:16 canvas; audio strip gains at start, middle and end. GPU: a crossfade of two opaque scenes at t = 0.5 equals the mean of the two pictures within 1/255 (no dip). Hot path: before/after on Mac M4 Pro and the Windows RTX 4090 at 1080p60 and 2160p60 of the render tick (p50, p99) idle, during a Fade outside and inside studio mode, plus GPU memory during a Fade, with the hot-path checklist and the owner's sign-off before phase 1 merges.

## Consequences

- RBS gets the transitions OBS users expect, built from parts that already exist (a second compositor, `Fade`, `Slide`, mixer ramps), with no change to `CompositorNode::process` and no new shader for phases 1 and 2.
- What OBS does not do: the transition waits for the incoming picture (no black flash); sound always follows the picture; a 9:16 canvas transitions in its own shape; the stinger's cut point is found for you; a transition can be rehearsed on Preview (phase 3); one-off `kind`/`duration_ms` from the API; nothing runs for transitions between them.
- The program in the model changes when the command is accepted, while the picture can trail it by up to `READY_WAIT` plus the duration. Clients that need the picture use `transition.state`.
- A Fade outside studio mode costs one extra compositor and the incoming scene's sources start before the change, as they do today. Two scenes' sources run during a transition, so device and GPU use peaks for its length.
- The rail's "Cut" becomes a real cut. Existing collections load as Move (or Cut when their glide was 0 ms), so they look the same.
- Specs change with it: scene model (transition, format 19), user data (format 19), control API (decision 13), gpu-compositor (a canvas as a layer, stated; transitions leave the out-of-scope list), recording (program changes trail by the transition), UI design system (rail), media-graph (none).

## Alternatives considered

- **Per-layer opacity for Fade, no texture.** Overlapping opaque layers dip (Context). Rejected.
- **A dedicated transition node with two inputs and its own shader, always in the graph.** Costs a pass on every frame when idle (P13), adds a hot-path node and moves every output's link. Rejected for phases 1 and 2; a shader node may come for wipes with a mask image (luma wipe), as its own ADR.
- **Render both scenes to textures and blend in a third compositor.** One more full pass than drawing the outgoing scene's layers directly into the program compositor. Rejected.
- **Keep "Cut" as today's morph and add Fade beside it.** The name would keep lying. Rejected; Move is the honest name and existing shows keep their look.
- **Stinger now as a silent overlay.** ADR-0108 forbids a media node that later grows sound. Waits for it.

## Open questions for the owner

1. Which kind a new collection starts with: Move 400 ms (today's look) or Fade 300 ms (what OBS users expect).

## Review

Independent review per ADR-0014. Pending.
