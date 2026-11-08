# ADR-0081: Master strip in the Mixer

- Status: Accepted
- Date: 2026-11-08
- Deciders: Salih (owner)
- Spec: [scene-model](../specs/scene-model.md), [user-data](../specs/user-data.md), [control-api](../specs/control-api.md), [audio-graph](../specs/audio-graph.md), [ui-design-system](../specs/ui-design-system.md)

## Context

Every Mixer strip now has a fader and a mute button (PR #231), but the sum of all strips has none. The engine already has a master stage: `MixerParams::master` (a linear gain, ramped over 10 ms like a strip) and a master `LevelMeter` that the studio creates and reads for the stats (`Studio::master_meter`). Nothing sets the gain (it is always 1.0), and the UI shows no master level. A streamer needs one control that lowers everything that goes to the stream and the recording (a phone rings, a guest arrives), and a place to see the final level that the encoders get.

The mixer is per canvas (ADR-0039): each canvas has its own mix for its own outputs. So a master belongs to the canvas, not to the application.

## Decision

1. **Model.** `Canvas` gains `master: MasterAudio { volume: f32, muted: bool }` (`Copy`, `non_exhaustive`; `volume` linear `0.0..=MAX_SOURCE_VOLUME` (1.0), default 1.0; not muted). The master has no boost: like a source, it stops at 0 dB, and boost is an audio filter (the audio roadmap). `rbs-scene` gets `set_canvas_master_volume(canvas, volume)` (`InvalidVolume` out of range) and `set_canvas_master_muted(canvas, muted)`. Like source volume and mute, they are live controls: each returns an `Undo` with its own record kind that expects and replaces only `master`, and the application keeps them off the user's undo stack. Every other canvas record (rename, format, program, preview, globals, and `Op::ReplaceCanvas`, whose check compares the whole `Canvas`) ignores `master` when it checks the state it expects and keeps the current `master` when it applies, as the source records do with `audio` (scene-model spec); the comparison masks `master` the way it masks a source's `audio`. The master belongs to the canvas: removing a canvas removes it, and undoing the removal restores the canvas with its master from the record; nothing else needs cleanup. `from_parts` validation checks `volume` (finite, in `0..=MAX_SOURCE_VOLUME`, else `InvalidVolume`); `scene-model.md` (validation list) is extended. The master is shared by preview and program: studio mode and the held program copy (ADR-0061, ADR-0063) have no master of their own, like source audio.
2. **Scene document format 7.** The key `master` on a canvas is optional, written only when it differs from the default (an object with `volume` and `muted`, each optional). Files of formats 1 to 6 load unchanged. A build that predates 7 refuses the file (unknown keys are denied). `scene-model.md` and `user-data.md` (canvases table and the sentence "Format 1 to 6 load unchanged") change in the same PR.
3. **Engine.** The studio runs one canvas (`Studio::new(canvas)`), and its mixer reads that canvas's master: `mix_of` / `mixer_params` in `rbs-record` `studio.rs` take the master gain (the volume, or 0 when muted) next to the strips, so every send carries it: the first send at start, the sends of `update` (graph edits) and `Studio::set_mix`. A loaded file therefore starts at its saved master, not at 1.0. `set_mix` reads `collection.canvas(self.canvas)`; its name and docs change to say it rebuilds the volume and mute of the strips and the master. This is the existing light path (no rebind, no graph change, the mixer's 10 ms ramp keeps it free of clicks). **`rbs-audio/src/mixer.rs` does not change** (`MixerParams::master`, its ramp and the master meter exist and sanitize any `f32`); `rbs-record` is not on the [hot-path list](../architecture/hot-paths.md), and the audio graph spec only gains a sentence saying who sets `master`. A canvas the studio does not run keeps its master in the model and applies it when it runs. The master meter is post-master-fader (the mixer's rule: "the master meter the peak of the output"), so it shows what the encoders get, including a muted master as silence.
4. **Control API 1.15.** The API serves the studio's canvas, so the commands take no canvas id: `canvas.set_master_volume { volume }` (finite, `0..=1`, else `invalid_params`) and `canvas.set_master_muted { muted }` (Scope::Control, like the other `canvas.*` controls), live and not undoable like `source.set_volume`, with the same effect class and on-air rule as `source.set_volume` and `source.set_muted` (control-api spec), since a master mute silences what is sent. `scene.list` and `scene.changed` gain `master { volume, muted }` (absent means 1.0, not muted); `scene.changed` is the one topic that announces a change; the existing one-change-per-frame coalescing of volume changes applies. The `audio.levels` payload gains an optional `master { peak_db, rms_db }` next to its per-source list (not a fake source id): the studio canvas's master meter, sampled at the client's rate like the sources. An older server answers `method_not_found` and omits the fields, which readers treat as the default. The "Planned additions" list in `control-api.md` is updated in the same PR. No other pending ADR claims 1.15 (checked 2026-11-08; 1.14 is ADR-0080).
5. **Mixer UI (visual, owner approval first).** The master is drawn as a strip at the end of the Mixer, set apart by a divider: in the wide layout a vertical strip after the groups (and before "Hidden"), in the narrow layout a row under the groups, above the dBFS scale. It shows the name "Master" (not a source name, so it is never renamed or hidden), the icon of the speaker, the meter, the fader, the level in words and a mute button. It has no ⋯ menu and cannot be hidden: it is always shown while the studio runs, even when no source makes sound (it then shows silence and its fader still works, so the level can be set before the first microphone is added). The fader, scroll, keyboard, double-click reset and accessible names are the ones of the strips (`Slider` / `VSlider`, same fader law). The master meter has its own clip latch like the strips. All visuals use tokens and library components only; no new token is expected.
6. **Typed dB entry is not part of this decision.** Clicking the level to type a value changes every strip's layout and is a separate visual change; it gets its own proposal to the owner and no ADR (UI only). This ADR's review therefore does not wait for it.
7. **Zero cost.** The master adds no node and no thread: it is a parameter of the existing mixer. The master meter is read by the existing 30 Hz meter timer while the Mixer shows meters.

## Consequences

- One control lowers or silences everything a canvas sends out, and its level is visible next to the strips.
- Scene format 7 and API 1.15 are public contracts and need the independent review below.
- A muted master silences the stream and the recordings of that canvas. The master mute button therefore uses the same accessible text pattern as the strips ("Mute Master", "Unmute Master") and its muted state is shown by icon, shape and text, not by colour alone (U7). A stream with a muted master still shows its on-air state as before.
- Per-output mixes and buses (a different master per output) stay out of scope; the model leaves room for them (`master` is per canvas today, a bus model would supersede it with its own ADR).

## Alternatives considered

- **One global master for the application.** Breaks the rule that a vertical canvas streamed elsewhere does not get the main canvas's audio; a global fader would move both. Rejected (owner chose per canvas, 2026-11-08).
- **Store the master in the user's settings, not the scene document.** Then a collection moved to another machine would sound different and the control API could not read it with the scene. Rejected.
- **A master gain filter on the encoder input.** Another node per output for something the mixer already does, and the master meter would be before it. Rejected.
- **Allow boost on the master.** Boost is an audio filter (owner, 2026-11-08), and a master boost hides clipping. Note that the sum of strips can exceed 0 dBFS and a master capped at 0 dB can only lower it; a limiter or compressor on the master is an audio filter for the filter roadmap, not part of this ADR.
- **The master as a mixer strip of a special source kind.** It has no producer node, so it would need a fake source in every scene and the binder would have to special-case it. Rejected.

## Review

Independent review per ADR-0014 (scene format 7 and API 1.15 are public contracts).

Round 1, independent AI session, 2026-11-08: changes requested (minor, direction sound). Concerns and resolutions:
1. `canvas.list` does not exist; `canvas.changed` versus `scene.changed`. -> Decision 4: `scene.list` and `scene.changed` gain `master`, one topic.
2. `audio.levels` shape. -> Decision 4: optional `master` field, not a source id.
3. Single-canvas studio, `set_mix` without a canvas id, `mixer_params` and the first send and `update`. -> Decision 3 names the functions and the start and update paths.
4. Hot path status of `studio.rs`. -> Decision 3: `rbs-record` is not on the list; `mixer.rs` unchanged.
5. `Op::ReplaceCanvas` equality, canvas removal. -> Decision 1.
6. Validation and `user-data.md`. -> Decisions 1 and 2.
7. Preview and held program copy. -> Decision 1.
8. Typed dB entry bundled. -> Split out (decision 6).
9. Missing alternatives. -> Added.
10. Version number. -> Checked (decision 4).
Verified correct by the reviewer: `MixerParams::master`, its ramp and the post-fader master meter exist; the undo-record pattern matches ADR-0059 and ADR-0080; format 6 precedent. Tests required in the implementation PR: a fader move between a canvas rename and its undo, removing and undoing a canvas, a saved master applied at start.
Round 2, same reviewer, 2026-11-08: approve. All ten concerns resolved; the single-canvas API claim verified. Three wording points applied: command names `canvas.set_master_*`, the effect class and on-air rule stated, coalescing stated. Owner approval of the ADR and the Mixer visual: Salih, 2026-11-08.
