# ADR-0091: Per-scene audio levels: a scene can remember its own volumes and mutes

- Status: Proposed
- Date: 2026-11-09
- Deciders: Salih (owner)
- Builds on: [ADR-0021](0021-scene-collection-model.md), [ADR-0059](0059-scenes-lock-and-source-audio-in-the-studio.md), [ADR-0061](0061-studio-mode-in-the-desktop-app.md), [ADR-0065](0065-global-sources-and-safe-source-removal.md), [ADR-0080](0080-hide-sources-in-the-mixer.md), [ADR-0081](0081-master-strip-in-the-mixer.md)
- Spec: [scene-model](../specs/scene-model.md), [audio-graph](../specs/audio-graph.md), [control-api](../specs/control-api.md), [user-data](../specs/user-data.md), [ui-design-system](../specs/ui-design-system.md)

## Context

A source's volume and mute are properties of the source (`Source.audio`, ADR-0059), the same in every scene that shows it. Streamers want the opposite for some scenes: music loud on "Starting soon" and low on "Talking", the game quiet on "Be right back", a microphone live on "Chat" and off on "Intermission". Today they move faders by hand at every switch, or build duplicate sources.

What exists: the studio applies volume and mute through `Studio::set_mix` (a light path, no rebind, ADR-0059) and the mixer ramps level changes over 10 ms, so a switch is click-free. A scene already carries per-item overrides of layout (`SceneItem.overrides`). Scene switches and transitions are live actions (ADR-0021, ADR-0061).

Constraints: existing collections must behave exactly as before; nothing per-frame may be added for people who do not use the feature (P13, rule 10); levels are user data and need a clear UI that shows which mode a scene is in.

## Decision

1. **Opt-in per scene, with explicit modes.** A scene is in one of two modes: **Shared levels** (the default; the sources' own levels apply, as today) or **Own levels** (the scene keeps its own volume and mute for the sources it chooses). A scene never changes mode by itself.
2. **Model.** `Scene.audio: Option<SceneAudio>`; `SceneAudio` is a map from `SourceId` to `{ volume: Option<f32>, muted: Option<bool> }` (same ranges as `SourceAudio`). Absent entry or absent field means "use the source's own value". `None` is Shared levels. Ids that no longer exist are dropped by the validating loader; deleting a source records its entries in the undo record so undoing the deletion restores them (ADR-0065). Live fader edits use dedicated live operations whose records expect and replace only `audio`, as ADR-0081 does for `master`, so they cannot cause `undo_conflict` with `scene.audio.own`/`shared`. The scene document gets an optional key, present only for scenes in Own levels; the scene format number is raised at the next free value at merge time (numbers are allocated in merge order, also for ADR-0086 and ADR-0093), and older builds refuse a file that uses it. Scene scope (`Canvas(id)` or `Shared`, U15) is unchanged; a shared scene's levels apply on whichever canvas shows it.
3. **Effective level.** The level the mixer gets for a source is `scene override ?? source value`, evaluated for the **program** scene (and the global scene of the canvas, ADR-0065, whose sources keep their own values unless the program scene has an override for them). In studio mode only the program scene's levels are heard; the preview scene's levels apply when it goes to program (its audio is not mixed in preview, as today). Nested scenes use the outermost program scene's overrides for their sources; a nested scene's own `audio` is ignored (the UI says so).
4. **Applying on a switch.** On `scene.switch` and on a transition, the studio computes the effective levels for the new program scene and sends them through `set_mix`, whose signature changes to take the program scene as well as the collection (callers on the live-edit path are updated). A transition applies the new levels when the program scene changes (a cut today); a future timed transition applies them at its start. A switch to the same scene changes nothing; switching the collection rebuilds the mix. The mixer's existing 10 ms ramp applies, so there is no click. No rebind, no new node. Hidden sources (ADR-0080) and sources not in the scene are unaffected by the scene's levels except through the rule of decision 3. When no scene is in Own levels there is no extra work at a switch beyond what it does today.
5. **Editing.** While a scene is in Own levels and on air, the Mixer's faders and mute buttons edit **that scene's** entry (the strip shows a small "scene" mark in this mode, and Properties > Sound shows which value is being edited); in Shared levels they edit the source, as today. Both are live changes like source volume today: sent at once, saved in the document, not on the undo stack (ADR-0059). Commands: `scene.audio.own` (turn Own levels on, **copying the current effective levels** of the scene's sources so nothing jumps; one undo step), `scene.audio.shared` (turn it off, discarding the overrides; one undo step, with the usual confirmation because it discards data), and `scene.audio.reset_source {source}` (remove one entry). Registry commands appear in the Mixer's scene header menu and the Scenes panel row menu (U3).
6. **Control API** (next free minor version): `scene.audio.get {scene}`, `scene.audio.set {scene, source, volume?, muted?}` (turns the scene to Own levels if it is not, which the reply states), `scene.audio.own`, `scene.audio.shared`; `scene.list` rows carry `own_audio: bool`, and `source.changed` events and rows carry the effective level and `overridden` so remote panels show what is heard. Class `control`. Clients that do not know the field see unchanged `source.*` behaviour; `source.set_volume` on a source while the program scene overrides it changes the source's own value and the reply says the scene override is still in force (`overridden_by_scene: true`).
7. **Zero cost.** A scene without the field adds nothing. Computing effective levels runs on the control thread at a switch, over the scene's sources.
8. **Visual change.** The Mixer scene header menu, the "scene" mark on strips and the Properties note are visual and follow rules 15 and 16: proposal first, then screenshots in three themes.

## Consequences

- Scenes can have their own mix without duplicating sources, and the behaviour for everyone else is exactly the same.
- The mode is visible (mark, note), so a fader is never silently editing something other than the user thinks.
- A fader adjusted in Own levels does not change the source's level elsewhere, which is the point and also something the UI must say.
- Format and compatibility notes in `scene-model.md` and `user-data.md`; `audio-graph.md` documents the effective-level rule; `control-api.md` the methods; glossary: "Scene levels".
- Fade lengths longer than the 10 ms ramp (a slow fade of the music between scenes) need a mixer change (hot path, P15) and are not part of this decision; `fade_ms` per scene is a possible later field.

## Alternatives considered

- **Per-item volume (volume on the scene item, like layout).** The same source in two scenes can differ, but it multiplies fields per item and cannot express "mute everything not listed"; per-scene entries are smaller and match how people think ("this scene's mix").
- **Always per-scene (no shared mode).** Breaks existing collections and makes a global change (one mic gain) a chore across scenes.
- **Snapshot/preset mixes recalled by command.** More general, but a mix that changes with the scene automatically is what the owner asked for; presets can be added on top.
- **Apply by switching which sources are Active.** Changes cost and startup latency, not just levels (P13 favours leaving sources as they are).

## Review

Pending: independent review per ADR-0014.
