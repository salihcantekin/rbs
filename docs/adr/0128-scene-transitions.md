# ADR-0128: Scene transitions: Cut, Fade, Move, Swipe and Slide, and later Stinger

- Status: Accepted
- Date: 2026-11-16
- Deciders: Salih (owner)
- Builds on: ADR-0013 (zero cost when unused), ADR-0023 (parameter channels; fades and slides evaluated on the node), ADR-0037 (mixer ramps), ADR-0061 (studio mode), ADR-0063 and ADR-0098 (held program, transition keeps the preview scene), ADR-0064 (Transition rail), ADR-0065 (global scenes), ADR-0074 (colour source), ADR-0089 (recording markers), ADR-0091 (per-scene audio levels), ADR-0093 (further canvases and followers), ADR-0100 (glide duration and easing), ADR-0107 (temporal sources sleep when hidden), ADR-0111 (new values of request fields), ADR-0124 (hold scenes)
- Supersedes in part: ADR-0100 decision 1, the sentence "These defaults apply to every program-scene change": the glide now applies when the selected transition is Move (decision 2). ADR-0061 decision 3, "Fade and other transitions are a later decision": this is that decision.

## Context

A program-scene change (`scene.switch`, `canvas.transition`, a follower canvas following) has one behaviour today. The program compositor gets the new scene's layer list. A source in both scenes slides to its new box and a source only in the new scene fades in, with the collection glide (ADR-0100, default 400 ms ease-in-out). A source only in the old scene leaves the layer list on the first frame; a non-temporal one stays linked as a warm source, a temporal one is unlinked (ADR-0107). The glide is evaluated when layers are sent and is never cleared by a timer. The rail calls all this "Cut", although it is not a cut while the glide is longer than 0 ms.

Operators coming from OBS expect a real cut, a crossfade (Fade), a fade through a colour, Swipe and Slide, Move (shared sources morph), and Stinger (a video with alpha that covers the change). A crossfade cannot be built from per-layer opacity: two scenes of opaque, overlapping layers at 50 % each show what is behind both (a dip), so the incoming scene must be drawn whole into a texture first.

Forces:

- The compositor node is on the render hot path (P6, P15). Its `process` evaluates `Fade` and `Slide` per layer without allocating, and it accepts any video frame on its "layers" pin. A compositor's canvas is `Rgba16Float`, linear BT.709, premultiplied, so it can be a layer of another compositor (drawn with `Transfer::Linear`), and output pins fan out.
- The binder keys compositors by `(canvas, Role)`: Program, Preview, Hold. It reconciles only links it made.
- The scene-preview compositor exists only when the studio has a UI preview description, only for the main canvas, and it is rebound to the outgoing scene when "Swap scenes after a transition" is on (ADR-0098). The program may run a held copy of its scene (ADR-0063), not the scene itself.
- The mixer's ramp (`MixerParams.ramp`) is one value for every strip, the master, sends and the monitor.
- Unused features must cost nothing (P13). The scene format and the control API are contracts (ADR-0014). Stinger needs a media-file source (ADR-0108), still Proposed.

## Decision

### Model

1. **Kinds and settings.** `rbs-scene` gains `TransitionKind` (`#[non_exhaustive]`): `Cut`, `Fade`, `Move`, `Swipe`, `Slide`. `SceneCollection` gains `transition: CollectionTransition { kind, fade: FadeSettings, swipe: SwipeSettings, slide: SwipeSettings }`. Each kind keeps its own settings, so changing kind does not lose a tuned value. Move's settings are the existing `glide` (ADR-0100), not a copy.
   - `FadeSettings { duration_ms, easing, via: Option<[u8; 3]> }`: `via` is an opaque sRGB colour to fade through; `None` is a crossfade.
   - `SwipeSettings { duration_ms, easing, direction: Left | Right | Up | Down }`.
   - Bounds: `duration_ms` in `0..=MAX_GLIDE_MS` (5000) and the `GlideEasing` enum of ADR-0100. Defaults: Fade 300 ms linear, no `via`; Swipe and Slide 500 ms ease-in-out, `Left`. A new collection's kind is Move (owner, 2026-11-16).
   - One undoable edit, `set_transition(CollectionTransition)`, refused with `InvalidTransition` when a value is out of range.
2. **What each kind does.** Duration 0 is a cut for every kind.
   - **Cut:** the new layers on the next frame. No glide and no fade-in, whatever `glide` says.
   - **Fade:** the incoming scene, drawn whole, goes from transparent to opaque over the outgoing scene. With `via`, a colour covers the outgoing scene in the first half and the incoming scene appears over the colour in the second half.
   - **Move:** today's slides and fade-ins (ADR-0100, collection glide and per-source overrides). In addition, a source only in the outgoing scene fades out over the glide duration, drawn above the incoming layers so the fade is seen.
   - **Swipe:** the incoming scene slides in from the edge opposite `direction`, over the outgoing scene, which stays still.
   - **Slide:** like Swipe, and the outgoing scene moves out by the same offset.
3. **Scene document format 19.** Root table `transition = { kind, fade = {...}, swipe = {...}, slide = {...} }`, defaults omitted when writing. Formats up to 18 load with `kind = "move"` when `glide.duration_ms > 0` and `"cut"` when it is 0, so an existing show looks the same. Older builds refuse format 19 without touching the file (P9).
4. **Not in format 19.** A per-scene override ("transition into this scene") and Stinger's settings are not reserved. Each comes with its own format number when its phase is built.

### Running a transition

5. **Where it is drawn.** The program compositor stays the only node the outputs, the preview picture, projectors and the replay buffer read. A transition changes what its layer list holds for the transition's length, then the graph returns to today's shape.
   - **Move** needs no texture: the program compositor draws the incoming scene's layers (today's slides and fade-ins) and above them the outgoing-only layers with a `Fade` to 0.
   - **Fade, Swipe and Slide** draw the outgoing scene's layers as today and, above them, one more layer: the canvas of an *incoming compositor*. That layer is drawn with `opaque: true`, so transparent areas of the incoming scene are its picture over black, exactly what the encoder shows after the transition (no jump at the end). Fade gives it a `Fade` from 0 to 1, Swipe a `Slide` from one canvas width or height off the edge, and Slide also gives each outgoing layer a `Slide` by the opposite offset. `via` adds a colour-source node (ADR-0074) for the transition's length, as a full-canvas layer between the two with its own `Fade`.
   - **The incoming compositor** is a binder compositor of a new role, `Role::Incoming`, per canvas whose program changes, in every mode including studio mode and headless control. It is bound to the run scene that will be on air: the held copy when the program will be held (ADR-0063, ADR-0098), else the scene. It is never the scene-preview compositor, so the swap setting and preview edits during the transition do not change the incoming picture.
   - **Global scenes** (ADR-0065) are left out of both the outgoing layers and the incoming compositor and drawn above both by the program compositor, unchanged, so a logo does not double, dim or move.
6. **Binding both scenes (binder contract).** The binder gains an `outgoing: BTreeMap<CanvasId, OutgoingScene>` parameter, like the hold map of `sync_held`. `OutgoingScene` holds the outgoing picture's bound layers (the snapshot the studio already takes for the glide, `Glide.from`) and its heard sources, not a `SceneId`, because a held program transitioned to itself (ADR-0063 decision 3) has the same scene id going out and coming in while only the new copy is in the run collection. For each entry it adds those sources to the program compositor's links and to the mixer's heard set, so they keep producing and sounding for the transition's length. The scene-model spec's "Graph binding" section states it.
7. **Starting: the ready gate.** The transition starts when the incoming compositor has drawn every layer whose producer is linked, or after `READY_WAIT` (250 ms) from the command at the latest. The wait covers building the incoming compositor (`prepare`, `warm`) and sources that are still opening, so the picture never flashes black. An incoming compositor is built with a ready counter, `Option<Arc<AtomicU64>>` given to `CompositorNode::new` (`None` for every other compositor, which then pays nothing). In `process` it stores `tick.index` with `Release` when every layer of the current list whose link resolves had a texture frame that tick (an empty list counts as ready); 0 means never. The incoming compositor starts with its layers passed to `new`, so any non-zero value means ready for this transition; a compositor whose list is replaced while it runs compares against the value read when the new list was sent. This is a hot-path change (P15): measured, checklist, owner's sign-off. `Studio::service` reads it; the layers of the transition are sent with `start` set to the time they are sent, so a late observation does not cut part of the fade. `program` changes in the model and `scene.changed` is published when the command is accepted; the picture follows when the transition starts.
8. **Ending in two steps.** When the elapsed time passes the duration (checked in `Studio::service`; the transition is clamped at its end value until then, so a late check costs only resources):
   1. Send the program compositor the incoming scene's own layer list. Its producers are already linked to the program compositor (decision 6), so the next frame is the same picture drawn directly.
   2. When `ParamSender::pending()` on the program compositor's layer sender reads false with no other send in between (`process` calls `update()` before it draws, so that tick already used the new list), apply the schedule swap that drops the outgoing entry from the binder, unlinks the outgoing-only temporal sources (ADR-0107), and removes the incoming compositor and any `via` node.
   Any other send of program layers between the two steps restarts the wait. `pending()` stays true while the node is idle, so step 2 also runs after `READY_WAIT` at the latest, and at once when `is_closed()`. No frame between the two steps names a node that is no longer linked. A test checks the order.
9. **One at a time.** A program change while a transition runs ends the running one at once and starts the new one from the scene that was coming in. This is a visible jump, as in OBS. Leaving studio mode, a collection switch or stopping the studio ends it at once. Undo and redo never start a transition (ADR-0061 decision 5).
10. **Sound follows the picture.** `Strip` gains `ramp_override: Option<(Timestamp, Duration)>`: start and length in engine time. For the transition the studio sends the strips of both scenes with that override set to the picture's start and duration: a source only in the outgoing scene ramps to silence, one only in the incoming scene ramps up from silence, a shared source ramps from its outgoing level to its incoming level (ADR-0091). Other strips, the master, sends and the monitor keep the short default ramp, so a fader moved during the transition reacts as usual. In `apply_params` an override builds `Ramp { from: <current gain at the take>, to: target, start, length }` instead of the default `towards`, and the rejoin path (a strip that comes back from silence) uses the override's start and length too, so incoming sound rises over the transition and not in 10 ms. A start in the past is evaluated by time. The timestamp is on the engine clock, the same as the mixer's block time. Clearing the override at the end with the same target keeps the running ramp (`towards` with an unchanged target). No per-sample work is added, but the mixer is a hot path too (P15): measured with decision 7's change, checklist, owner's sign-off. Cut keeps the default ramp.
11. **Every canvas, followers, holds, markers.** Each canvas whose program changes in the event runs the same kind and settings. Swipe and Slide offsets are in that canvas's own size, so a 9:16 canvas swipes its own width. A follower's program is set when the followed canvas's transition starts, not when the command is accepted (`with_followers` runs at the start step), and the follower runs its own transition of the same kind with its own incoming compositor and ready gate from that moment. While an output holds with a hold scene (ADR-0124), it reads its own `Role::Hold` compositor and does not show the transition; the transition still runs on the program. A recording's scene-change marker (ADR-0089) is placed at the transition's start.
12. **Cost (P13).** Outside a transition nothing of it exists: no incoming compositor, no added layer, no colour node, no strip override and no per-frame check (only incoming compositors carry the ready counter). During a transition outside and inside studio mode: one incoming compositor per changing canvas (its pipeline, and `CANVAS_RING` = 3 `Rgba16Float` canvases, about 200 MB at 2160p, kept in the pool for `POOL_IDLE_TICKS` after the end), one more full-canvas layer in the program compositor, and the outgoing scene's sources running. The measurements of decision 18 include the incoming compositor's creation time and the GPU memory peak.

### Control API (version decided at merge; 1.68 if nothing else takes it)

13. **Methods and events.** `collection.set_transition { kind?, fade?, swipe?, slide? }` (`control`, one undo step, merges the given parts). `canvas.transition` and `scene.switch` take optional `kind` and `duration_ms` for this change only (a Stream Deck button "Fade 1 s" needs no settings change); with kind Move, `duration_ms` replaces the collection glide's duration and every per-source override for that change. `scene.list` and `scene.changed` carry `transition` on the collection snapshot. A new topic `transition.state { canvas, state: "started" | "ended", kind, duration_ms, from, to, at }` (`from` and `to` scene ids, `at` engine time) is published at the start and the end, never per frame. Adding Stinger later is a new value of `kind`, minor under ADR-0111.

### UI (proposed separately, screenshots before merge)

14. **Rail.** The rail's Select lists Cut, Fade, Move, Swipe and Slide. The Duration field shows the selected kind's duration and is disabled for Cut. Easing, Direction and Through colour open from a small settings button next to the Select, with help behind (i). Outside studio mode, where there is no rail, the same choice is in the Scene menu. Tokens only, compact (ADR-0064 sizes). The exact layout is a UI proposal for the owner (rule 16).

### Later phases (each needs its own decision)

15. **Phase 3:** a manual T-bar (`canvas.transition_progress { t }` and a T-bar on the rail), a per-scene override, and Rehearse (play the transition on the Preview picture only, without going on air).
16. **Phase 4, Stinger (after ADR-0108 is accepted and built):** the planned design is a media source the collection holds but no scene shows, linked as the top layer during the transition, playing once with its sound on a strip of its own, with the scene change under it at a cut point that the app finds from the alpha when the file is chosen (the first frame covering 99 % of the canvas, else the middle) and the user can correct. An optional "Get ready in advance" setting keeps its first frames decoded while it is selected. It gets its own ADR, format number and protocol version.

### Phases 1 and 2

17. Phase 1: model and format 19, Cut, Fade (with `via`), the binder's outgoing scene, the incoming compositor, ready gate, two-step end, sound following the picture, control API, the rail's Select and Duration. Phase 2: Move fade-outs, Swipe, Slide, the settings popup.

### Tests and measurement

18. Model: format 18 to 19 mapping (glide 0 to Cut, else Move), round-trip, bounds, undo; an older build refuses format 19 without changing the file. Binder: with an `outgoing` entry, the outgoing visible and temporal sources are linked to the program compositor and heard; without it, today's result. Studio (with `TestClock`, by tick index): a Fade's added layer and its removal; the layer list is sent before the schedule swap at the end; after the end the program compositor's layers equal a plain switch; the ready gate starts on readiness and on `READY_WAIT`; a held program transitioned to itself fades from the old copy to the new one; a second change mid-transition; leaving studio mode mid-transition; studio mode with Keep and with Swap while the program is held; headless studio mode; an extra canvas in studio mode; Swipe offsets on a 9:16 canvas; globals drawn once above during Swipe; a follower switches at the start; the marker is at the start. Mixer: strip gains at start, middle and end; a fader change during a transition uses the default ramp. GPU: a crossfade of two opaque scenes at t = 0.5 is the mean of the two within 1/255; a partly transparent incoming scene: the last blended frame equals the first plain frame after the encoder's composite over black. Control API: the schema classifier marks the change minor. Hot path: before/after on the Mac M4 Pro and the Windows RTX 4090, 1080p60 and 2160p60, render tick p50 and p99 idle and during a Fade inside and outside studio mode, the incoming compositor's creation time and the GPU memory peak, with the hot-path checklist and the owner's sign-off before phase 1 merges.

## Consequences

- RBS gets the transitions OBS users expect, built from parts that already exist (a binder compositor, `Fade`, `Slide`, mixer ramps). `CompositorNode::process` gains only the optional ready counter of incoming compositors, and phases 1 and 2 add no shader.
- What OBS does not do: the transition waits for the incoming picture (no black flash); sound always follows the picture while faders keep reacting; a 9:16 canvas transitions in its own shape; global overlays stay steady through every transition; one-off `kind` and `duration_ms` from the API; nothing runs between transitions. Phases 3 and 4 add Rehearse and an automatic stinger cut point.
- The model's program changes when the command is accepted; the picture can trail it by up to `READY_WAIT` plus the duration. Clients that need the picture use `transition.state`.
- During a transition two scenes' sources run and an extra compositor exists, so GPU memory and device use peak for its length (decision 12).
- A second change mid-transition jumps.
- The rail's "Cut" becomes a real cut. Existing collections load as Move (or Cut when their glide was 0 ms) and new ones start as Move, so no show looks different.
- Specs change with it: scene model (transition, format 19, Graph binding: the outgoing scene and `Role::Incoming`), user data (format 19), control API (decision 13), gpu-compositor (a canvas as a layer, the ready counter; transitions leave the out-of-scope list), audio graph (strip `ramp_override`), recording (program changes trail by the transition; marker time), UI design system (rail).

## Alternatives considered

- **Per-layer opacity for Fade, no texture.** Overlapping opaque layers dip (Context). Rejected.
- **The scene-preview compositor as the incoming one in studio mode.** It is missing in headless control and on further canvases, it is rebound by the swap setting, and it draws the scene rather than the held copy that goes on air. Rejected; the dedicated `Role::Incoming` costs one compositor for the transition's length.
- **A dedicated transition node with two inputs and its own shader, always in the graph.** Costs a pass on every frame when idle (P13), adds a hot-path node and moves every output's link. Rejected for phases 1 and 2; a shader node may come for wipes with a mask image (luma wipe), as its own ADR.
- **Render both scenes to textures and blend in a third compositor.** One more full pass than drawing the outgoing layers directly. Rejected.
- **Raise the global mixer ramp for the transition.** Every fader, mute and master change would then ramp for up to 5 s. Rejected for the per-strip override.
- **Keep "Cut" as today's morph and add Fade beside it.** The name would keep lying. Rejected; Move is the honest name and existing shows keep their look.
- **Stinger now, or its settings reserved in format 19.** ADR-0108 is Proposed and forbids a media node that later grows sound; a key that format 19 refuses reserves nothing. Waits for it.

## Review

Independent review per ADR-0014.

- Reviewer: independent AI session (fresh context; given only this ADR, the principles, the related ADRs, the specs and the code)
- Date: 2026-11-16
- Verdict: changes requested, 13 findings (5 blocking). All are resolved in this revision:
  1. Blocking: the scene-preview compositor cannot be the incoming one (missing headless and on further canvases, rebound by Swap, draws the scene, not the held copy). Resolved: dedicated `Role::Incoming` bound to the run scene that goes on air (decision 5); cost restated (decision 12); alternative recorded.
  2. Blocking: the end swap was not frame-exact (swap before layers would flash the old scene). Resolved: two-step end, layers first, swap after the acknowledgement (decision 8), with an order test.
  3. Blocking: transparent areas of the incoming scene jumped at the end. Resolved: the incoming canvas layer is `opaque: true` (decision 5); GPU test with a partly transparent scene (decision 18).
  4. Blocking: the ready gate had no signal. Resolved: a per-node atomic ready counter, marked hot path (decision 7); `start` at send time.
  5. Blocking: the binder change is a contract change. Resolved: `outgoing` parameter specified (decision 6) and the Graph binding spec listed.
  6. The mixer ramp is global. Resolved: per-strip `ramp_override` (decision 10).
  7. Global scenes drawn twice. Resolved: drawn once above (decision 5).
  8. `via` colour form and its producer node. Resolved: opaque RGB, a colour-source node for the transition's length (decisions 1, 5, 12).
  9. Cost understated. Resolved: decision 12 and measurements in decision 18.
  10. Move's leaving layers hidden under the incoming ones. Resolved: drawn above (decisions 2 and 5).
  11. Contract details. Resolved: no reserved `transition_in`, no Stinger in format 19 (decision 4); `transition.state` carries `from`, `to`, `at`; `duration_ms` with Move defined; Stinger later is minor under ADR-0111 (decision 13).
  12. Interactions. Resolved: followers switch at the start, hold scenes keep their own compositor, marker at the start (decision 11); ADR-0105 removed from the dependencies; mid-transition jump in Consequences.
  13. Missing tests. Resolved: decision 18.
- Round 2, 2026-11-16, same reviewer on the revision: approve with changes, 5 findings, resolved here:
  1. Blocking (text): the layer channel has no version acknowledgement. Resolved: step 2 waits for `ParamSender::pending()` to read false, with the restart, idle and closed cases stated (decision 8).
  2. The ready counter was under-specified. Resolved: type, ordering, meaning of "every layer", and only incoming compositors carry it (decision 7).
  3. `ramp_override` under-specified, and the mixer is a hot path. Resolved: `apply_params` and the rejoin path, clock, clearing, P15 (decision 10).
  4. Blocking: a held program transitioned to itself has one scene id going out and in. Resolved: `outgoing` carries the bound layers and heard sources, not a `SceneId` (decision 6), with a test.
  5. Followers: resolved, each runs its own transition from the leader's start (decision 11).

Owner acceptance, 2026-11-16: Salih approved the ADR after both review rounds.

Superseded in part by [ADR-0153](0153-a-transition-of-its-own-outside-studio-mode.md) decision 1, 2026-11-18: decision 14 (one kind for every program change) no longer holds; outside studio mode a program change uses a kind of its own, Move by default.
