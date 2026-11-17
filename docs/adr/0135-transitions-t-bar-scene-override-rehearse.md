# ADR-0135: Transitions phase 3: T-bar, a transition into a scene, and Rehearse

- Status: Proposed
- Date: 2026-11-17
- Deciders: Salih (owner)
- Builds on: ADR-0128 (scene transitions; decision 15 names this phase), ADR-0061 (studio mode), ADR-0063 and ADR-0098 (held program; keep or swap the preview after a transition), ADR-0064 (Transition rail), ADR-0091 (per-scene audio levels), ADR-0093 (further canvases and followers), ADR-0111 (new values of request fields)

## Context

ADR-0128 built Cut, Fade (also through a colour), Move with fade-outs, Swipe and Slide. Its decision 15 names a third phase in one sentence: a manual T-bar (`canvas.transition_progress { t }` and a T-bar on the rail), a per-scene override, and Rehearse (the transition played on the Preview picture only). Each of the three touches a contract: the T-bar a control API method and the rail, the override the scene document format, Rehearse the studio's preview path. This ADR fixes those contracts before code (rule 2, ADR-0014).

What is there today (ADR-0128, `docs/specs/recording.md` "Transitions"):

- A program change is applied to the collection at once. The studio then runs the transition on its own clock. It waits for the incoming picture (`Waiting`), draws for its duration (`Running`), then ends in two steps (`Ending`, `settle_transitions`).
- The incoming scene is drawn whole by a compositor `(canvas, Role::Incoming)`. The program compositor gets it as one layer, and the blend is a `Fade` or a `Slide` on that layer, evaluated by the node against tick time.
- Sound follows the picture through per-strip `ramp_override` windows (start and length).
- In studio mode the preview picture is drawn by the scene-preview compositor, which exists only when the studio has a UI preview description, and only for the main canvas.

What OBS does, and does badly:

- OBS's T-bar crossfades only. Released half-way, it stays there until moved again, and the program does not change until it reaches the end.
- OBS has a per-scene transition override ("Transition Override" in the scene's menu), with a kind and a duration.
- OBS cannot rehearse a transition. The operator sees it for the first time on air.

## Decision

### T-bar

1. **Manual progress.** A transition can be driven by hand instead of by its duration.
   - `canvas.transition_progress { canvas?, t }` (`control`, live, not undoable, answers `{}`) sets the progress `t` in `0.0..=1.0` (finite; else `invalid_params`).
   - `canvas` defaults to the studio's main canvas. A follower canvas is refused with `conflict`. Studio mode only: outside it `conflict` ("not in studio mode").
   - Effect (ADR-0052): as `canvas.transition`, `edit` with the on-air check, so an agent token cannot drive a T-bar while an output runs.
   - The kind and its settings are those a `canvas.transition` would use now (the preview scene's override, decision 6, else the collection). The duration is not used.
   - Two drivers (the rail and a client) are last-write-wins. Values from the UI and the API are coalesced on the control thread: each service step applies the latest `t` per canvas.
2. **Binding (a binder contract change).**
   - The binder's per-canvas transition entry (`OutgoingScene`, ADR-0128 decision 6) gains `incoming_scene: Option<SceneId>`. `None` keeps today's meaning: the incoming compositor draws `canvas.program`. `Some(scene)` makes the incoming compositor draw that scene while the program stays as it is, and adds that scene's heard sources (`visible_sources_cued`) to the program mixer as inputs.
   - A manual transition's entry names the preview scene, with `drawn` and `heard` empty: the program scene is bound as usual.
   - When the program is held (ADR-0063), the incoming compositor draws the preview scene as it is now; no held copy exists until the end.
   - Spec: scene model, "Graph binding".
3. **What it draws.**
   - The first `t > 0`, on a canvas with no manual transition, starts one from the program scene to the preview scene. Nothing changes in the collection until the end (decision 5).
   - It waits for the incoming picture like a timed transition (ADR-0128 decision 7): the outgoing picture alone until the incoming compositor's ready counter moves or `READY_WAIT` passes, then the latest `t`. `transition.state` `started` is published at that moment.
   - The program compositor then gets the outgoing layers and the incoming layer with the blend frozen at `t`:
     - Fade: opacity `e(t)`, where `e` is the kind's easing.
     - Fade through a colour: per half, as the timed form: the colour's opacity `e(min(1, 2t))` and the incoming opacity `e(max(0, 2t - 1))`. So the T-bar at `t` shows what the timed transition shows at `t` times its duration.
     - Swipe and Slide: the offset times `1 - e(t)`.
     - Move: a crossfade through the incoming compositor, as Fade with Move's easing. A per-source morph needs both scenes' layers interpolated in one compositor; that can come later as its own change.
   - Layers are static (no `Fade` or `Slide` on the node) and sent once per applied `t`, on the control thread. Globals stay above, drawn once (ADR-0128 decision 5).
4. **Sound.**
   - Each program strip's target gain is the linear lerp of its outgoing and incoming gain at `e(t)`. A muted side counts as gain 0; ADR-0091 levels give each side's gain.
   - Sources only in the preview scene are heard through decision 2's inputs, at gain 0 at `t = 0`.
   - The strip moves with the default mixer ramp, so a dragged T-bar sounds as smooth as a fader. Master, sends and monitor are untouched.
5. **Reaching the end.**
   - `t = 1` applies the program change (the preview keeps or swaps its scene by the app's setting, ADR-0098). This change does not start a timed transition. The manual one goes straight to the two-step end of ADR-0128 decision 8 (`Ending`, then `settle_transitions`) with the incoming compositor it has.
   - After the change the entry's `incoming_scene` is `None` (the incoming scene is now the program), so the same incoming node stays bound until the settle. When ADR-0063 makes a held copy at that change, the node is rebound to it within the same update.
   - `transition.state` gets `started` once and `ended` once. Both carry `manual: true`, and `duration_ms` is the time between them.
   - Followers (ADR-0093) cut at the end: a follower's own timed transition would trail a picture that is already over.
   - Recording markers (ADR-0089) are placed at the end, when the program changes.
   - A T-bar with Cut selected cuts when `t` reaches 1.
6. **Going back, holding, observing.**
   - `t = 0` cancels it: the outgoing layers alone, the strips back to their outgoing levels, the entry dropped and the incoming compositor released at the next settle. Nothing changed in the collection, so the only event is `transition.state` `ended` with `to` equal to `from`.
   - Between 0 and 1 the transition holds where it is, as OBS. A client that disconnects leaves it where it is.
   - While it holds, `transition.state` is the latest-value state `holding` with `t` (the last applied value), published at most once per service step. A second client can show the T-bar's place.
   - A held T-bar keeps the incoming compositor (about 200 MB of canvases at 2160p, ADR-0128 decision 12) and both scenes' sources running, for as long as it holds.
7. **Interplay.**
   - A `canvas.transition`, `scene.switch` or change of the preview scene while a manual transition holds first cancels it (decision 6), then acts as today.
   - Edits to the preview scene's items while it holds update the incoming picture live, as they update the preview.
   - Leaving studio mode cancels it.
   - A timed transition running when the T-bar first moves is ended at once (ADR-0128 decision 9), then the manual one starts from what is on air.
   - **Preview equal to program** (the usual state after a take with "keep"): the T-bar runs from the held copy to the scene as edited, as `canvas.transition` does today. With no held copy (nothing edited) the two pictures are the same: it runs and changes nothing visible.

### A transition into a scene

8. **Override.** A scene may name the transition used when it comes on air: `Scene.transition_in: Option<SceneTransition { kind: Option<TransitionKind>, duration_ms: Option<u32> }>`.
   - `kind` absent uses the collection's kind. `duration_ms` absent uses the kind's own duration from the collection. Both absent is no override (stored as `None`).
   - The kind's other settings (easing, colour, direction) stay the collection's: one place to tune them, as OBS.
   - An override of Move with `duration_ms` replaces the glide's duration and every per-source override for that change, as a one-off does (control API 1.81).
   - Precedence for one change: a one-off `kind` or `duration_ms` (ADR-0128 decision 13), then the incoming scene's override, then the collection. A one-off `kind` with no `duration_ms` uses that kind's collection duration, not the override's.
   - A held program transitioned to itself (ADR-0063) uses its scene's override. A follower canvas uses the followed scene's override, except at a T-bar's end (decision 5).
9. **Format.** Scene document format: the next free number at merge (20 if free). The key is per scene: `transition_in = { kind?, duration_ms? }`, omitted when absent. Older formats load with no overrides. An older build refuses the new format without touching the file (P9).
   - OBS import (ADR-0131) maps OBS's per-scene "Transition Override" to it where the kind exists.
   - Duplicate scene (ADR-0133) copies it.
10. **API.**
    - `scene.set_transition {scene, transition: {kind?, duration_ms?} | null}` (`control`, one undo step) sets or clears the override. `kind` takes the `collection.set_transition` names. `duration_ms` is in `0..=5000`. `{}` clears, like `null`.
    - Effect: as the other `scene.*` edits, `edit`, with the on-air check when it names the program scene.
    - `scene.list` and `scene.changed` carry `transition_in` on a scene row that has one.
    - Minor: the next free protocol number at merge.

### Rehearse

11. **What it shows.** In studio mode, Rehearse plays, on the Preview picture only, the transition a `canvas.transition` would run now (the preview scene's override applies) from the program picture to the preview picture.
    - Program, outputs, recordings, the mixer, markers, followers and the control API state do not change.
    - After the duration the Preview picture shows the preview scene again.
    - It is for the operator's eyes: no sound is rehearsed.
    - Disabled for Cut and for a duration of 0.
12. **How.**
    - A studio-owned compositor (not a binder role, so no binder change), sized to the preview display's picture rather than the canvas, so it costs a fraction of a canvas: at a 640×360 preview, 3 `Rgba16Float` textures of about 1.8 MB each.
    - It has two layers: the program compositor's output and the preview compositor's output. A compositor output fans out (ADR-0128 Context). The blend runs as a timed transition on the node, with the same `Fade` and `Slide` values.
    - Both pictures already contain the global scene. In a rehearsed Swipe or Slide the global layers move with the pictures, while on air they stay still above. Stated, not hidden; drawing both scenes' layers would need the binding that decision 3 avoids.
    - Move rehearses as a crossfade, as the T-bar draws it.
    - The studio adds the compositor and relinks the UI preview display's producer to it in one apply, and links the display back to the preview compositor and removes the node when the rehearsal ends. This happens outside the binder's reconcile, like the incoming compositor's adoption.
    - It needs the preview compositor, so it exists only where the scene preview exists (a studio with a UI, the main canvas). No API method: a UI action, `ui.transition.rehearse`.
13. **Interplay.** A real transition, a T-bar move or a change of the preview scene during a rehearsal ends the rehearsal at once. Rehearse is refused while a transition runs or holds on that canvas.

### UI and cost

14. **Rail and menus.** The exact layout is a UI proposal for the owner (rule 16). It must hold:
    - A T-bar on the rail: vertical between the pictures, horizontal below them. It snaps back to 0 after an end, as OBS.
    - A Rehearse command beside Transition, in the rail's overflow or its options popover.
    - "Transition into this scene…" in the Scenes panel's context menu, opening a small popover with Kind (Default, Cut, Fade, Move, Swipe, Slide) and Duration ("Default" or a value).
    - Keyboard: the T-bar is a `Slider` (arrows step it, Home and End go to 0 and 1), and End ends the transition.
15. **Cost (P13).**
    - A T-bar at rest, an override not used, and no rehearsal add no node, layer, timer or per-frame check.
     - A rehearsal adds one preview-sized compositor for its duration.
   - A held T-bar costs what a waiting timed transition costs (decision 6), for as long as it holds.
    - No change to `CompositorNode::process`, the shader or the mixer's per-sample path. The T-bar adds layer sends and strip updates on the control thread, at most one per service step, coalesced (decision 1).

### Phases

16. 3a: per-scene override (model, format, API, Scenes menu). 3b: T-bar (studio, API, rail). 3c: Rehearse. Each is its own PR. UI parts follow rule 16.

## Consequences

- An operator can take a transition at their own pace, hold it half-way (a split screen with Slide, a superimposition with Fade), and see a transition before it goes on air. OBS does none of these with sound following, with Swipe, Slide or Move, or before air.
- The program change of a T-bar happens at its end, not its start. Clients that watch `scene.changed` see the change when the picture is fully over. `transition.state` tells them a manual transition is running.
- The studio gains a manual mode beside the timed one. Tests by tick index, as ADR-0128 decision 18:
  - the binder with `incoming_scene` set: the preview scene drawn by the incoming compositor, its incoming-only sources heard, the program unchanged;
  - the ready gate on the first `t > 0`;
  - progress at 0, 0.5 and 1 for each kind (through a colour per half);
  - the end: no second `started`, the same incoming node through the change, followers cut;
  - preview equal to program;
  - cancel at 0;
  - a timed transition interrupting a held T-bar;
  - leaving studio mode while it holds;
  - strip levels at 0.5;
  - the program change and the marker at the end;
  - a rehearsal leaving every output untouched (encoded frames unchanged);
  - the override's precedence with a one-off;
  - format round-trip and refusal by an older build.
- Specs:
  - scene model (`transition_in`, format, Graph binding: `incoming_scene`)
  - user data (format)
  - control API (`canvas.transition_progress`, `scene.set_transition`, `transition_in` on rows, `transition.state` `holding` and `manual`)
  - recording (manual transitions, Rehearse)
  - UI design system (rail T-bar, Rehearse, the Scenes menu entry)

## Alternatives considered

- **Change the program at the T-bar's start**, as a timed transition does. Moving the T-bar back would then have to undo a program change that clients, followers and markers already saw. Rejected.
- **Drive the T-bar by moving a timed transition's start time** (`Fade.start = now - t * duration`). The animation would keep running on its own between moves. Rejected: a held T-bar must hold.
- **A per-scene override with its full settings** (easing, colour, direction per scene). Doubles every setting and their UI, while OBS users rarely tune them per scene. Rejected for kind and duration only; can be extended later as a minor change.
- **Rehearse on Program with outputs muted or frozen.** Touches every output and the encode path, and the audience could still see a frozen frame. Rejected.
- **Rehearse Move per source.** Needs the outgoing scene's layers bound into a second compositor for the preview display: more binding for a preview-only feature. Rejected for now; the crossfade shows timing and kind well enough.

## Review

- Round 1 (independent AI session, 2026-11-17): changes requested; findings in the project's review notes (4 blocking, 12 non-blocking).
  - B1: the binder could not bind the incoming compositor to the preview scene. Resolved: `incoming_scene` on the entry, with heard sources (decision 2).
  - B2: manual Move was incoherent. Resolved: a crossfade (decision 3).
  - B3: the end was undefined against `note_transitions`. Resolved: no timed transition, straight to the two-step end, the same node, followers cut (decision 5).
  - B4: no ready gate. Resolved (decision 3).
  - Non-blocking, all resolved in the text:
    - preview equal to program (decision 7);
    - the sound lerp domain (decision 4);
    - `holding` with `t` and `manual: true` (decision 6);
    - the held cost, disconnects and last-write-wins (decisions 1 and 6);
    - through a colour per half (decision 3);
    - optional `kind` in the override (decision 8);
    - Rehearse binding, globals, override, Cut, size and cost (decisions 11 and 12);
    - effects and followers refused (decisions 1 and 10);
    - preview item edits (decision 7);
    - OBS import, duplicate and coalescing (decisions 1 and 9).
- Round 2: pending.
