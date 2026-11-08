# ADR-0080: Hide sources in the Mixer

- Status: Proposed
- Date: 2026-11-08
- Deciders: Salih (owner)
- Spec: [scene-model](../specs/scene-model.md), [user-data](../specs/user-data.md), [control-api](../specs/control-api.md)

## Context

The Mixer lists every source that makes sound in the program scene. A microphone or a music bed that the user never touches still takes a strip, and with many sources the Mixer becomes crowded. The user wants to put such sources away without muting them: a hidden source is still heard.

Where the flag lives decides what a workspace switch does. A flag in the workspace file would show the microphone again when another workspace is applied. The owner chose (2026-11-08) that the flag belongs to the source, like lock and mute, so a hidden microphone stays hidden in every workspace.

## Decision

1. **Model.** `SourceAudio` (`Copy`, `non_exhaustive`) gains `mixer_hidden: bool` (default `false`, no range). Hidden is a view choice only: it never changes volume, mute, sync offset, the mix or the API `audio.levels` topic. `rbs-scene` gets `set_source_mixer_hidden(source, hidden)`, which reuses `Op::SetSourceAudio` like `set_source_muted` (no new record kind). Like mute, it is a live control: the application keeps it off the user's undo stack, and the record only keeps `audio`-only edits conflict-free.
2. **Scene document format 6.** The key `mixer_hidden` is optional, written only when `true`; a source without it is visible. Files of formats 1 to 5 load unchanged, with no repair step. A build that predates format 6 refuses the file (unknown keys are denied). `scene-model.md` and `user-data.md` (sources table, and the sentence "Format 1 to 5 load unchanged") change in the same PR.
3. **Control API.** New command `source.set_mixer_hidden { source, hidden }` (Scope::Control) and a `mixer_hidden` field in `source.list`, `source.changed` and `scene.sources`. It takes the light path of mute: no graph rebind, no audio glitch. `scene.changed` fires. The protocol minor is **1.14** (1.13 shipped with ADR-0077); the "Planned additions" list in `control-api.md` is updated in the same PR. An older server answers `method_not_found` and omits the field, which readers treat as `false`.
4. **Mixer UI.** Hidden sources are not drawn as strips or rows. After the visible groups the Mixer shows a section "Hidden (n)" with section id `mixer.hidden` ([ADR-0078](0078-saved-section-state-and-shared-section-components.md)), closed by default and shown only when `n > 0`. Each hidden source is a compact row with its name and a Show button; no level indicator is drawn. Each visible strip or row has a Hide button. Wide and narrow layouts behave the same. A hidden source that clips gives no signal (a later ADR may add a clip dot to the header).
5. **Meters.** `set_heard` filters hidden sources out of the slots and the `MeterRow`s with the same filter, so `tick_meters` keeps its 1:1 indexing. Hidden sources are not read for the Mixer, and their peak holders are reset when shown. When every source is hidden, `has_meters` is false and the 30 Hz timer stops.
6. **Global sources.** The flag is on the global source, so it applies in every scene that uses it. A hidden global source moves to "Hidden (n)" (not its `mixer.global` group); `n` counts the hidden sources of the program scene's heard set, global ones included.
7. **Sources panel.** Not part of this decision; the same flag can drive a filter there later.

## Consequences

- A hidden microphone stays hidden across workspaces, scenes and restarts, and a collection shared with another machine carries it.
- Scene format 6 and API 1.14 are public contracts and need the independent review below.
- The Mixer audio plan (faders, master strip, ducking indicator) shares the `MeterRow` fields; this change adds only `hidden`, and the layout of strips is otherwise untouched.
- A user can forget that a hidden source is still audible. The "Hidden (n)" header is the reminder and stays visible while `n > 0`.

## Alternatives considered

- Store the flag in the workspace file: a workspace switch would bring the source back, which the owner rejected.
- Hide by muting: changes what is heard and loses the volume intent.
- A per-scene flag on the scene item: the same microphone would need hiding in every scene.

## Review

Round 1, independent AI session (2026-11-08): changes requested (API minor clash with ADR-0077, undo record, garbled UI sentence, meter filtering, global sources, API fields). All resolved in the text above.

Round 2, independent AI session (2026-11-08): approve. Implementation reminders: remove a hidden source from the slots before the next `tick_meters`; register `mixer.hidden` in the saved section state. The owner approved the behaviour proposal (2026-11-08); approval of the visual result (rule 16) and the Accepted status are still pending.
