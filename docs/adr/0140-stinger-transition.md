# ADR-0140: Stinger transition: a media file played over the program change, cut point found from its alpha

- Status: Proposed
- Date: 2026-11-17
- Deciders: Salih (owner)

## Context

ADR-0128 decision 16 names phase 4: a Stinger, a video played over the scene change, with the change hidden under it. The planned design there is: a media source the collection holds but no scene shows; drawn as the top layer during the transition; playing once with its sound on a strip of its own; the scene change under it at a cut point that the app finds from the alpha when the file is chosen, and that the user can correct. ADR-0108 (Accepted, built) gives the media-file source. Phases 1 to 3 (ADR-0128, ADR-0135) give the transition machinery: an incoming compositor, a ready gate, timed layers on the program compositor, sound that follows the picture, the T-bar and Rehearse.

Forces:

- **Alpha.** A stinger is usually a video with alpha (ProRes 4444, HEVC with alpha in a `.mov`, VP9 with alpha in `.webm`). ADR-0108 decision 6 takes video as NV12 or P010 (no alpha) and only animated pictures (GIF, APNG, WebP) as straight-alpha `Rgba8`. A full-frame stinger without alpha (H.264 that covers the whole canvas at the cut) is also common.
- **Timing.** The media source needs its lead (ADR-0108 decision 5, 150 ms) and a decode start before the first picture. A stinger that starts late shows the cut.
- **Cost (P13).** A chosen stinger that is not playing must cost nothing on the render or audio path.
- **OBS.** OBS asks the user for the transition point in milliseconds or frames and has no way to find it; its "track matte" mode needs a second video. It starts the stinger as soon as asked and may show the change before the video covers it.
- Contracts: the scene format and the control API (ADR-0014). ADR-0128 decision 13 says Stinger is a new value of `kind`, minor under ADR-0111.

## Decision

1. **Model.** `TransitionKind::Stinger` and `CollectionTransition.stinger: StingerSettings { source: Option<SourceId>, cut_ms: Option<u32>, cut_found_ms: Option<u32> }`. `source` names a media-file source (ADR-0108) of the collection. A source used as a stinger must not be in any scene; the collection refuses to place it in a scene while it is the stinger, and refuses to make a placed source the stinger. `cut_ms` is the user's cut point; `None` uses `cut_found_ms`, the point found by decision 3. A scene's `transition_in` may name `kind: stinger` (ADR-0135 decision 8); the stinger itself is the collection's. Removing the source clears `stinger.source`. Scene format: the next free number at merge.

2. **Alpha.** The media decode port gains one picture layout for video with alpha: straight-alpha sRGB `Rgba8` in system memory, or a native BGRA surface on the platforms that import one, under the same rules as ADR-0108 decision 4. Adapters must deliver it for ProRes 4444 and HEVC with alpha where the platform decodes them with alpha (AVFoundation), and must report `has_alpha` on the probe. A stinger file without alpha is accepted as a full-frame stinger. An adapter that cannot decode the file fails as ADR-0108 already says. This does not change ADR-0108's rule for ordinary video: NV12 and P010 stay the path for video without alpha.

3. **Cut point found once.** When a file becomes the stinger (or its path changes), a worker thread, never the control or render thread, decodes it from the start for at most 10 s or its whole length. It records the first picture whose alpha covers at least 99 % of the pixels (alpha ≥ 0.99) as `cut_found_ms`; for a file without alpha, or when no picture reaches it, half its duration. The result arrives on a channel and wakes the control loop, like ADR-0108's probe. Until it arrives, a transition uses half the probed duration. The user may set `cut_ms` instead; the UI shows the found point and offers "Use found point".

4. **Ready, then play.** A Stinger change starts like a Fade (ADR-0128 decision 2): the program change is applied to the collection, the incoming compositor is bound, and the transition waits. It also wakes the stinger source (Installed → Active) with `restart_when_shown` and `loop: false`. It runs when both the incoming picture is ready and the stinger's first picture is published, or after `STINGER_WAIT` (1 s) on the master clock. Then it plays:
   - From the start until the cut point: the outgoing layers, and above them one full-canvas layer of the stinger picture, fitted to the canvas (contain, centred), straight alpha.
   - From the cut point to the end: the incoming compositor's full-canvas layer, opaque, and the stinger above it.
   - The end is the stinger's duration (its play range, ADR-0108); then it goes to `Ending` as ADR-0128 decision 8 and the source goes back to Installed. Globals stay above it, as in every transition.
   A stinger that has not published a picture after `STINGER_WAIT` runs as a Cut and counts it; the transition never waits longer.

5. **Get ready in advance.** An optional `StingerSettings.prepare: bool` (default false) keeps the source Standby with its first pictures decoded while the collection's kind is Stinger, so the start needs no decode. It is the only state in which a stinger costs anything between transitions: one paused worker and its ring (ADR-0108 decision 10), no render-path node.

6. **Sound.** The stinger's sound plays on its own mixer strip, linked only while the transition runs, at the source's volume and mute; it is heard on the program mix and its outputs. Scene sound follows the picture at the cut point: the strips go from their outgoing to their incoming gains at the cut with the default mixer ramp, not over the whole duration.

7. **Other canvases.** A further canvas, or a follower, that changes with the main canvas cuts at the main canvas's cut point, without the stinger picture, in this phase. A canvas with its own stinger is not in this phase.

8. **T-bar and Rehearse.** The T-bar is refused for Stinger with `conflict` ("A stinger plays at its own speed."), and the rail's T-bar is disabled while the kind is Stinger. Rehearse is disabled for Stinger in this phase.

9. **API.** `collection.set_transition` accepts `kind: "stinger"` and `stinger: {source, cut_ms?, prepare?}` (`source` `null` clears it). `scene.list` and `scene.changed` carry `stinger` with `cut_found_ms` on the collection's transition. `transition.state` reports `kind: "stinger"`, and `duration_ms` is the file's play range. Refusals: `not_found` for an unknown source, `invalid_params` for a source that is not a media file or is placed in a scene, a `cut_ms` past the duration. Minor: the next free protocol number at merge.

10. **UI.** Stinger is a sixth entry of the rail's type `Select`. Its options popover holds the file (the source's path field and Browse), the cut point (`NumberField` in ms with "Use found point"), and "Get ready in advance" (`Toggle`). The exact layout is a proposal for the owner (rule 16).

11. **Cost (P13).** No stinger chosen, or chosen and not playing without `prepare`: no node, link, worker, ring or per-frame check. A playing stinger adds the source's picture and sound nodes and one layer for its duration. No change to `CompositorNode::process`, the shader or the mixer's per-sample path.

## Consequences

- Operators get a stinger that never shows the cut early (the ready gate), with a cut point found from the file instead of typed by trial and error, and sound that switches at the cut.
- The media port gains an alpha layout: the two platform adapters and the port spec change; the pure-Rust animated-picture adapter is unchanged.
- The collection gains a rule (a stinger source is not placed), with refusals in the model and the API.
- Phase 4 is split: 4a model, format, API and the alpha layout; 4b studio playback and sound; 4c UI. Each is its own PR; the UI part follows rule 16.
- Follow-ups: a stinger on a vertical canvas in its own shape; Rehearse of a stinger on the Preview picture; a track matte (second video) mode.
- Specs to update with each part: `scene-model.md`, `control-api.md`, `media-decode` port spec, `recording.md` (Transitions), `ui-design-system.md`.

## Alternatives considered

- **Stinger as a scene item shown for its duration** (a "timed show", ADR-0105). It would draw the stinger under the globals and need a scene edit per change; the cut would not wait for the picture.
- **Cut point typed only, as OBS.** Loses the main gain for little work saved; the found point is computed once, off the real-time paths.
- **Convert alpha video to NV12 plus a separate alpha plane.** A second YUV path in the compositor, which ADR-0108 decision 6 forbids.
- **T-bar scrubbing the stinger.** Needs frame-exact seeking for every T-bar move; refused for now (decision 8).

## Review

Independent review per ADR-0014: pending.
