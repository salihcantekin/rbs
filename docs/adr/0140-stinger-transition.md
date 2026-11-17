# ADR-0140: Stinger transition: a media file played over the program change, cut point found from its alpha

- Status: Proposed
- Date: 2026-11-17
- Deciders: Salih (owner)
- Supersedes in part: ADR-0108 decision 6 (video with alpha may be `Rgba8` with alpha kept), ADR-0108 decision 10 (an "arm" operation and its state rows), ADR-0128 decision 11 for Stinger only (followers switch at the cut)

## Context

ADR-0128 decision 16 names phase 4: a Stinger, a video played over the scene change, with the change hidden under it. The plan there: a media source the collection holds but no scene shows; drawn as the top layer during the transition; playing once with its sound on a strip of its own; the scene change under it at a cut point the app finds from the alpha when the file is chosen ("the first frame covering 99 % of the canvas, else the middle"), which the user can correct. ADR-0108 (Accepted, built) gives the media-file source. ADR-0128 and ADR-0135 give the incoming compositor, the ready gate, timed layers, sound that follows the picture, the T-bar and Rehearse.

Facts this rests on:

- The decode port already has `PictureLayout::Rgba8` (straight-alpha sRGB). Both platform adapters already deliver video as `Rgba8` on their copy paths (AVF `mac.rs`, MF `win.rs`), but make it opaque; MF native frames are `Bgra8`; the AVF native path is NV12 only. `VideoFrame` carries no alpha mode, and the probe reports no alpha.
- A source in no scene is not in the binder's `wanted_sources` or `warm_sources`: it stays Standby and unlinked (`scene-model.md`, Graph binding).
- Under ADR-0108 decisions 5 and 10 a Standby source has no worker and no rings; a source plays on the master clock from its wake unless paused; a play record (decision 10) starts it at a given instant.
- `Fade` with a future `start` and `duration` 0 is a step evaluated by the node (`rbs-gpu` `node.rs`); the mixer's `ramp_override` starts a ramp at an instant (ADR-0128 decision 10).
- OBS asks the user for the transition point in milliseconds or frames and starts the video at once, so the change can show before the video covers it.

## Decision

1. **Model.** `TransitionKind::Stinger` and `CollectionTransition.stinger: StingerSettings { source: Option<SourceId>, cut_ms: Option<u32> }`. `source` names a media-file source (ADR-0108). It must not be placed in any scene: `item.add` and scene edits that would place it are refused with `conflict` ("This source is the stinger."), and making a placed source the stinger is refused with `conflict`. `cut_ms` is the user's cut point; `None` uses the found point (decision 3). Kind Stinger with `source: None` runs as a Cut. A scene's `transition_in` and a one-off `kind` (ADR-0135 decision 8, ADR-0128 decision 13) may name Stinger; a `duration_ms` given with Stinger is ignored (the file's play range is the duration). Removing the source clears `stinger.source` in the same undo step. The rule also holds for paste, duplicate and OBS import (ADR-0131): a placement that would put the stinger source in a scene, directly or through a nested or global scene, is refused, and an import that would do so leaves the stinger unset. A loaded document that breaks it (written by hand or by another build) loads as it is; a change then runs as a Cut and the studio reports "The stinger is placed in a scene." `cut_ms` and the found point are measured from the start of the source's play range (ADR-0108 `start_ms`) and checked against the range's length. Scene format: the next free number at merge.

2. **Alpha in the decode port.** `MediaDecoder` gains `fn has_alpha(&self) -> bool { false }`, true when the picture stream signals alpha, and an open option `keep_alpha` (default false). Placed sources open without it and play as today: an alpha file is delivered opaque where it is today. The stinger opens with `keep_alpha`: an adapter must then deliver straight-alpha `Rgba8` system pictures with the alpha kept (the existing meaning of `Rgba8`, no new frame field and no change to `rbs_core::media::VideoFrame`), or fail open with `DecodeError::AlphaUnsupported`. In this phase only the AVF adapter keeps alpha (copy path; its native path is NV12 only); the MF adapter answers `AlphaUnsupported` for files with `has_alpha`, and plays files without alpha as full-frame stingers. A native alpha surface and an alpha-mode field are follow-ups. The AVF alpha copy path is a `cpu-upload` fallback under ADR-0108 decision 4; its exit criterion, recorded in the adapter spec, is a native BGRA alpha import measured below the copy path's cost at 4K60.

3. **Cut point, found once, off the live paths.** When a file becomes the stinger, or its path, size or modification time changes, one search runs on a low-priority worker thread (never the control, render or audio thread; one at a time; cancelled on a path change, the source's removal or a collection switch). It runs only for a file with `has_alpha` and opens it with `keep_alpha`, which yields system pictures only; it decodes one picture at a time and drops it (at most one frame of `MAX_FRAME_BYTES` plus the decoder), for at most 10 s of media or the whole file, and stops at the first match. It fits each picture to the canvas as decision 4 draws it, samples alpha on a 64 × 36 grid, and the cut is the first picture where at least 99 % of the canvas samples have alpha ≥ 252 (of 255). Without alpha, or with no match, the cut is half the duration. The result is a runtime cache keyed by canonical path, size, modification time, play range and canvas size, outside the document and the undo stack; the API and UI read it as `cut_found_ms`. Until it is known, a transition uses half the probed duration.

4. **Binding and timing.** The binder's outgoing entry (ADR-0128 decision 6) gains `stinger: Option<SourceId>`. With it the binder puts that source in `running_sources` and `on_air_sources`, keeps it Active, links its picture node to the canvas's program compositor and its sound node to the mixer (heard), and unlinks both when the entry goes; `scene-model.md` Graph binding gets this text.
   - **Arm.** The media control handle (`rbs-nodes` `MediaControl`) gains `arm()`: wake paused at the start of the play range, ignoring a stored cursor or `ended` state, with `loop: false` and `restart_when_shown` as a runtime override for this run only (the document is never written). It bumps the generation. Rows for it are added to ADR-0108 decision 10's table. The handle also exposes "first picture of generation `g` ready" from the worker side (the worker sets it when that picture is in the ring), so the picture node's `process` is not changed.
   - **Gate.** A Stinger change starts like a Fade: the collection changes, `scene.changed` is published, the incoming compositor is bound, the stinger is armed, and the transition waits. It runs when both the incoming picture and the stinger's first picture are ready, or after `STINGER_WAIT` (1 s) on the master clock. So the picture may follow the event by up to 1 s.
   - **Play.** At that moment the studio calls a new handle method `play_now() -> Timestamp`, which writes the play record and returns the anchor `A` the worker will use (`record.instant + LEAD`, ADR-0108 decisions 5 and 10). Every timed layer and ramp is set from `A`, once: the outgoing layers; the incoming compositor's full-canvas layer, opaque, with a step `Fade` (from 0, start `A + cut`, duration 0); above them the stinger picture fitted to the canvas (contain, centred); the globals above everything. Until `A` the stinger layer shows nothing (its ring is empty), so the outgoing scene stays on air.
   - **End.** The earlier of `A +` the range's length and the source's end hold; then `Ending` as ADR-0128 decision 8, and the entry goes. A stinger that is not ready by `STINGER_WAIT` runs as a Cut and is counted. The duration may exceed `MAX_GLIDE_MS`. A file whose duration is unknown after the probe runs as a Cut.

5. **Failures and interplay.**
   - A decode fault while it plays: the transition jumps to its end at once (the incoming layers alone); no back-off restart (ADR-0108 decision 9) is shown on air. A fault before the cut shows the incoming scene uncovered: intended, as a Cut. While the source is in back-off after a fault, a Stinger change runs as a Cut at once and says why.
   - The source removed, or its path changed, while it plays: the transition ends before the binder retires the node, in the ordering of ADR-0128 decision 8.
   - A program change while a stinger plays jumps it (ADR-0128 decision 9); a new Stinger change bumps the source's generation, and the gate waits for that generation's first picture.
   - Play, pause and seek on the stinger source (ADR-0108 decision 10) are refused with `conflict` while it plays.

6. **Sound.** The stinger's sound plays on its own strip at the source's volume and mute, linked only while the entry exists. Scene sound switches at the cut: every scene strip whose target changes gets its incoming gain with `ramp_override = (A + cut, default ramp)`, sent once at `A`'s computation (the mixer holds the current gain until the start); the stinger strip and strips with an unchanged target are left as they are.

7. **Other canvases.** A further canvas or a follower that changes with the main canvas binds its incoming compositor at the change and joins the main canvas's gate (the stinger waits for every such canvas's picture, within `STINGER_WAIT`), then switches at the main canvas's cut with the same on-node step and no stinger picture. A follower not ready by then cuts when its picture is ready, as ADR-0128 decision 11. This replaces ADR-0128 decision 11 (switch at the end) for Stinger only. A canvas with its own stinger is not in this phase.

8. **T-bar and Rehearse.** With an effective kind of Stinger (the preview scene's override included), the T-bar is a Cut at `t = 1` and does nothing between 0 and 1, as for Cut (ADR-0135); Rehearse is disabled for Stinger in this phase.

9. **API.** `collection.set_transition` accepts `kind: "stinger"` and `stinger: {source, cut_ms?}` (`source: null` clears it; `cut_ms: null` uses the found point). `scene.list` and `scene.changed` carry `stinger` with `cut_found_ms` (absent until found) on the collection's transition. `transition.state` reports `kind: "stinger"` and `duration_ms` as the play range. Refusals: `not_found` for an unknown source; `invalid_params` for a source that is not a media file, a `cut_ms` past the probed duration (unchecked while the duration is unknown, clamped when played); `conflict` for a placed source. Minor: the next free protocol number at merge.

10. **UI.** Stinger is a sixth entry of the rail's type `Select`. Its options popover holds the file (the source's path and Browse), the cut point (`NumberField`, ms) with "Use found point", and the found point in words. The layout is a proposal for the owner (rule 16).

11. **Cost (P13).** No stinger chosen, or chosen and not playing: the source is Standby like any unplaced source (no worker, ring, link or per-frame work), and nothing of this ADR runs. A playing stinger adds its source's worker, rings, picture and sound nodes, and one layer, for its duration. No change to `CompositorNode::process`, the media picture and sound nodes' `process`, the shader or the mixer's per-sample path; the readiness signal is set by the media worker (decision 4). "Get ready in advance" (a warm decoder between transitions) is not in this phase: it needs a source state ADR-0108 does not have.

12. **Tests and measurement.** Studio tests on `TestClock` (ADR-0128 decision 18): the gate waits for both pictures, the cut on its tick, the end, a fault, a removal and a second change while it plays, and the T-bar and Rehearse rules. Port conformance for `has_alpha`, `AlphaMode` and `AlphaUnsupported`. A measurement of the CPU copy of 4K60 `Rgba8` before the AVF alpha path ships. Any change to a `process`, the import or the shader path needs measurement and P15 sign-off.

## Consequences

- A stinger never shows the change early (the gate), its cut is found from the file instead of typed, sound switches at the cut, and followers switch with it.
- The decode port, both adapters and the port spec change. Windows plays no alpha stingers in this phase (full-frame stingers without alpha work everywhere).
- The binder gains the `stinger` field on the outgoing entry; the collection gains the placement rule.
- Phase 4 is split: 4a model, format, API, port alpha and the cut search; 4b binding, playback, sound and failures; 4c UI (rule 16).
- Follow-ups: alpha on Windows; "Get ready in advance"; a stinger in a vertical canvas's own shape; Rehearse of a stinger; a track matte mode; OBS import (ADR-0131) of OBS stingers.
- Specs: `scene-model.md`, `control-api.md`, the media decode port spec, `recording.md` (Transitions), `ui-design-system.md`.

## Alternatives considered

- **Stinger as a scene item shown for its duration** (timed show, ADR-0105): drawn under the globals, a scene edit per change, and no gate.
- **Cut point typed only, as OBS:** loses the main gain; the search runs once, off the live paths.
- **Alpha as NV12 plus a separate alpha plane:** a second YUV path, which ADR-0108 decision 6 forbids.
- **The cut sent from the control loop at the cut time:** drifts by up to a service period; the on-node step is exact.
- **T-bar scrubbing the stinger:** needs frame-exact seeks on every move.

## Review

Independent review per ADR-0014.

- Round 1, independent AI session, 2026-11-17, changes requested. Concerns and how this revision answers them:
  1. Timing anchor and an undefined "first picture" signal: decision 4 wakes the stinger paused, adds the generation signal, and anchors the play record and every timed layer at `T0 + L`.
  2. Binding of an unplaced source was an unstated binder change: decision 4 adds `stinger` to the outgoing entry.
  3. "Get ready in advance" contradicted ADR-0108's states: dropped (decision 11, follow-up).
  4. The alpha decision misdescribed the port: decision 2 now names `has_alpha`, `AlphaMode`, `AlphaUnsupported`, the AVF copy path and the Windows limit, and the header records the partial supersession.
  5. Missing failure modes: decision 5.
  6. Cut on the node and followers at the cut: decisions 4, 6 and 7.
  7. Search bounds and coverage after the fit: decision 3.
  8. `cut_found_ms` in the document: now a runtime cache (decision 3).
  9. Unknown duration and the real end: decisions 4 and 9.
  10. Contract gaps: decisions 1 and 9.
  11. Tests and measurement: decision 12.
  12.-14. Nits: decisions 8 and 11, Consequences.
- Round 2, independent AI session, 2026-11-17, changes requested. Answers:
  1. The play record's instant is the handle's: decision 4 adds `play_now()` returning the anchor `A`, and times everything from `A`.
  2. The stinger was outside ADR-0108 decision 10 and the on-air set: decision 4 adds `arm()`, its table rows (header records the partial supersession), and `running_sources` / `on_air_sources`.
  3. `loop` and `restart_when_shown`: a runtime override for the stinger run only (decision 4).
  4. Alpha refusal broke placed sources: decision 2 refuses only with `keep_alpha`; placed sources play as today.
  5. Port text: `has_alpha()` on `MediaDecoder`, no `VideoFrame` change, `AlphaMode` and native alpha moved to follow-ups.
  6. Search open: only for `has_alpha` files, with `keep_alpha` (system pictures).
  7. Cache key: play range and canvas size added; cut measured from the range start (decisions 1 and 3).
  8. Placement on load, paste, duplicate, import and nesting: decision 1.
  9. Followers: gated (decision 7).
  10. P15 scope: the readiness signal is set by the worker; decisions 11 and 12 name every `process`.
  11. P4 exit criterion: decision 2.
  12. Back-off and faults before the cut: decision 5.
  13. and 14. Wording: decisions 6 and 4.
