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

1. **Manual progress.** A transition can be driven by hand instead of by its duration. `canvas.transition_progress { canvas?, t }` (`control`, live, not undoable) sets the progress `t` in `0.0..=1.0` (finite; else `invalid_params`). `canvas` defaults to the studio's main canvas. Studio mode only: outside it `conflict` ("not in studio mode"). The kind and its settings are the collection's (or the next scene's override, decision 6). The duration is not used.
2. **What a manual transition is.**
   - The first `t > 0` on a canvas with no transition running starts a manual transition from the program scene to the preview scene. Nothing changes in the collection yet: the program scene stays the program until the end.
   - The binder binds the incoming compositor for the preview scene (as for a timed transition) and keeps the program scene bound as it is. The program compositor gets the outgoing layers and the incoming layer with the blend frozen at `t`:
     - Fade: opacity `t` (through a colour: the colour's opacity `min(1, 2t)` and the incoming `max(0, 2t - 1)`).
     - Swipe and Slide: the offset times `1 - t`.
     - Move: the slides at `t` and the fade-ins and fade-outs at `t`. Move uses the incoming compositor here too: the leaving and arriving layers must be drawn at `t` without a running clock.
   - The kind's easing maps `t`, as it maps time.
   - Layers are sent once per `t` change, on the control thread. No `Fade` or `Slide` runs on the node, and nothing is drawn per frame that a timed transition does not draw.
   - Sound: each strip's target is its outgoing level mixed with its incoming level at `t` (a gain lerp, ADR-0091 levels). It moves with the default mixer ramp, so a dragged T-bar sounds as smooth as a fader.
3. **Reaching the end.** `t = 1` ends it. The program change happens then, as a `canvas.transition` would: the preview keeps or swaps its scene by the app's setting (ADR-0098). The program compositor already shows the incoming picture, so the end is the two-step end of ADR-0128 decision 8, with no running phase. `transition.state` is `started` at the first `t > 0` and `ended` at `t = 1`; `duration_ms` in both is `0` and `kind` is the kind used.
4. **Going back.** `t = 0` cancels it: the outgoing layers alone, the strips back to their outgoing levels, the incoming compositor released with the next settle. Nothing changed in the collection, so nothing is published but `transition.state` `ended` with `to` equal to `from`. Between 0 and 1 the transition holds where it is for as long as the operator leaves it (as OBS).
5. **Interplay.**
   - A `canvas.transition`, `scene.switch` or preview change while a manual transition holds first cancels it (decision 4), then acts as today.
   - Leaving studio mode cancels it.
   - A timed transition running when the T-bar moves is ended at once (as ADR-0128 decision 9), then the manual one starts from what is on air.
   - Followers (ADR-0093) and recording markers (ADR-0089) act at the end, when the program changes.
   - Cut has no progress. A T-bar with Cut selected cuts when `t` reaches 1.

### A transition into a scene

6. **Override.** A scene may name the transition used when it comes on air: `Scene.transition_in: Option<SceneTransition { kind, duration_ms: Option<u32> }>`. `duration_ms` absent uses the kind's own duration from the collection. The kind's other settings (easing, colour, direction) stay the collection's: one place to tune them, as OBS.
   - Precedence for one change: a one-off `kind` or `duration_ms` (ADR-0128 decision 13), then the incoming scene's override, then the collection. A one-off `kind` with no `duration_ms` uses that kind's collection duration, not the override's.
   - A held program transitioned to itself (ADR-0063) uses its scene's override.
   - A follower canvas uses the followed scene's override.
7. **Format.** Scene document format: the next free number at merge (20 if free). The key is per scene: `transition_in = { kind, duration_ms? }`, omitted when absent. Older formats load with no overrides. An older build refuses the new format without touching the file (P9).
8. **API.**
   - `scene.set_transition {scene, transition: {kind, duration_ms?} | null}` (`control`, one undo step) sets or clears the override. `kind` takes the `collection.set_transition` names. `duration_ms` is in `0..=5000`.
   - `scene.list` and `scene.changed` carry `transition_in` on a scene row that has one.
   - Minor, the next free protocol number at merge.

### Rehearse

9. **What it shows.** In studio mode, Rehearse plays the selected transition from the program picture to the preview picture **on the Preview picture only**. Program, outputs, recordings, the mixer, markers, followers and the control API state do not change. After the duration the Preview picture shows the preview scene again. It is for the operator's eyes: no sound is rehearsed.
10. **How.**
    - Rehearse binds a rehearsal compositor for the canvas, drawn only on the UI preview display, never linked to an output.
    - It has two layers: the program compositor's canvas (a compositor output fans out, ADR-0128 Context), and the preview compositor's canvas with the blend.
    - Move rehearses as a crossfade between the two pictures. Its per-source morph needs both scenes' layers in one compositor, which the rehearsal does not bind.
    - The preview display shows the rehearsal compositor while it runs and the preview compositor after. The rehearsal compositor is removed at the next settle.
    - It needs the preview compositor, so it exists only where the scene preview exists (a studio with a UI, the main canvas). No API method: it is a UI action (`ui.transition.rehearse`), as other preview-only actions are.
11. **Interplay.** A real transition, a T-bar move or a preview change during a rehearsal ends the rehearsal at once. Rehearse is refused while a transition runs or holds on that canvas.

### UI and cost

12. **Rail and menus.** The exact layout is a UI proposal for the owner (rule 16). It must hold:
    - A T-bar on the rail: vertical between the pictures, horizontal below them. It snaps back to 0 after an end, as OBS.
    - A Rehearse command beside Transition, in the rail's overflow or its options popover.
    - "Transition into this scene…" in the Scenes panel's context menu, opening a small popover with Kind (Default, Cut, Fade, Move, Swipe, Slide) and Duration ("Default" or a value).
    - Keyboard: the T-bar is a `Slider` (arrows step it, Home and End go to 0 and 1), and End ends the transition.
13. **Cost (P13).**
    - A T-bar at rest, an override not used, and no rehearsal add no node, layer, timer or per-frame check.
    - A held T-bar keeps the incoming compositor bound (as a waiting transition does) until it ends or is cancelled.
    - A rehearsal binds one compositor for its duration plus the settle.
    - No change to `CompositorNode::process`, the shader or the mixer's per-sample path. The T-bar adds layer sends and strip updates on the control thread, at the rate the operator moves it (at most one per UI frame, coalesced).

### Phases

14. 3a: per-scene override (model, format, API, Scenes menu). 3b: T-bar (studio, API, rail). 3c: Rehearse. Each is its own PR. UI parts follow rule 16.

## Consequences

- An operator can take a transition at their own pace, hold it half-way (a split screen with Slide, a superimposition with Fade), and see a transition before it goes on air. OBS does none of these with sound following, with Swipe, Slide or Move, or before air.
- The program change of a T-bar happens at its end, not its start. Clients that watch `scene.changed` see the change when the picture is fully over. `transition.state` tells them a manual transition is running.
- The studio gains a manual mode beside the timed one. Tests by tick index, as ADR-0128 decision 18:
  - progress at 0, 0.5 and 1 for each kind;
  - cancel at 0;
  - a timed transition interrupting a held T-bar;
  - leaving studio mode while it holds;
  - strip levels at 0.5;
  - the program change and the marker at the end;
  - a rehearsal leaving every output untouched (encoded frames unchanged);
  - the override's precedence with a one-off;
  - format round-trip and refusal by an older build.
- Specs:
  - scene model (`transition_in`, format)
  - user data (format)
  - control API (`canvas.transition_progress`, `scene.set_transition`, `transition_in` on rows, `transition.state` with `duration_ms` 0)
  - recording (manual transitions, Rehearse)
  - UI design system (rail T-bar, Rehearse, the Scenes menu entry)

## Alternatives considered

- **Change the program at the T-bar's start**, as a timed transition does. Moving the T-bar back would then have to undo a program change that clients, followers and markers already saw. Rejected.
- **Drive the T-bar by moving a timed transition's start time** (`Fade.start = now - t * duration`). The animation would keep running on its own between moves. Rejected: a held T-bar must hold.
- **A per-scene override with its full settings** (easing, colour, direction per scene). Doubles every setting and their UI, while OBS users rarely tune them per scene. Rejected for kind and duration only; can be extended later as a minor change.
- **Rehearse on Program with outputs muted or frozen.** Touches every output and the encode path, and the audience could still see a frozen frame. Rejected.
- **Rehearse Move per source.** Needs the outgoing scene's layers bound into a second compositor for the preview display: more binding for a preview-only feature. Rejected for now; the crossfade shows timing and kind well enough.

## Review

Pending: independent review per ADR-0014, then the owner's approval.
