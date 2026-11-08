# ADR-0081: Master strip in the Mixer

- Status: Proposed
- Date: 2026-11-08
- Deciders: Salih (owner)
- Spec: [scene-model](../specs/scene-model.md), [user-data](../specs/user-data.md), [control-api](../specs/control-api.md), [audio-graph](../specs/audio-graph.md), [ui-design-system](../specs/ui-design-system.md)

## Context

Every Mixer strip now has a fader and a mute button (PR #231), but the sum of all strips has none. The engine already has a master stage: `MixerParams::master` (a linear gain, ramped over 10 ms like a strip) and a master `LevelMeter` that the studio creates and reads for the stats (`Studio::master_meter`). Nothing sets the gain (it is always 1.0), and the UI shows no master level. A streamer needs one control that lowers everything that goes to the stream and the recording (a phone rings, a guest arrives), and a place to see the final level that the encoders get.

The mixer is per canvas (ADR-0039): each canvas has its own mix for its own outputs. So a master belongs to the canvas, not to the application.

## Decision

1. **Model.** `Canvas` gains `master: MasterAudio { volume: f32, muted: bool }` (`Copy`, `non_exhaustive`; `volume` linear `0.0..=MAX_SOURCE_VOLUME` (1.0), default 1.0; not muted). The master has no boost: like a source, it stops at 0 dB, and boost is an audio filter (the audio roadmap). `rbs-scene` gets `set_canvas_master_volume(canvas, volume)` (`InvalidVolume` out of range) and `set_canvas_master_muted(canvas, muted)`. Like source volume and mute, they are live controls: each returns an `Undo` with its own record kind that expects and replaces only `master`, and the application keeps them off the user's undo stack. Every other canvas record (rename, format, program, preview, globals) ignores `master` when it checks the state it expects and keeps the current `master` when it applies, as the source records do with `audio` (scene-model spec).
2. **Scene document format 7.** The key `master` on a canvas is optional, written only when it differs from the default (an object with `volume` and `muted`, each optional). Files of formats 1 to 6 load unchanged. A build that predates 7 refuses the file (unknown keys are denied). `scene-model.md` and `user-data.md` change in the same PR.
3. **Engine.** `Studio::set_mix` also sends the canvas's master: `MixerParams::master` is the volume, or 0 when muted. This is the existing light path (no rebind, no graph change, the mixer's 10 ms ramp keeps it free of clicks). **No mixer code changes**, so no hot path is edited; the audio-graph spec only gains a sentence saying who sets `master`. The master meter is post-master-fader (the mixer's rule: "the master meter the peak of the output"), so it shows what the encoders get, including a muted master as silence.
4. **Control API 1.15.** New commands `canvas.set_master_volume { canvas, volume }` and `canvas.set_master_muted { canvas, muted }` (Scope::Control), a `master { volume, muted }` field on canvas rows in `canvas.list` and `canvas.changed` (absent means the default), and a `master` entry in the `audio.levels` topic. `scene.changed` fires on a change. An older server answers `method_not_found` and omits the field, which readers treat as the default. The "Planned additions" list in `control-api.md` is updated in the same PR.
5. **Mixer UI (visual, owner approval first).** The master is drawn as a strip at the end of the Mixer, set apart by a divider: in the wide layout a vertical strip after the groups (and before "Hidden"), in the narrow layout a row under the groups, above the dBFS scale. It shows the name "Master" (not a source name, so it is never renamed or hidden), the icon of the speaker, the meter, the fader, the level in words and a mute button. It has no ⋯ menu and cannot be hidden: it is always shown while the studio runs, even when no source makes sound (it then shows silence and its fader still works, so the level can be set before the first microphone is added). The fader, scroll, keyboard, double-click reset and accessible names are the ones of the strips (`Slider` / `VSlider`, same fader law). The master meter has its own clip latch like the strips. All visuals use tokens and library components only; no new token is expected.
6. **Typed dB entry (strips and master).** Clicking the level in words turns it into a small `NumberField` in dB (`-60` to `0`, "-inf" or empty is silent); Return applies, Escape cancels. It uses the existing `NumberField`. Because it changes the layout of every strip, it is part of the same visual proposal.
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
- **Allow boost on the master.** Boost is an audio filter (owner, 2026-11-08), and a master boost hides clipping.

## Review

Independent review per ADR-0014: pending (scene format 7 and API 1.15 are public contracts).
