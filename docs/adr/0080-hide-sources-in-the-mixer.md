# ADR-0080: Hide sources in the Mixer

- Status: Proposed
- Date: 2026-11-08
- Deciders: Salih (owner)
- Spec: [scene-model](../specs/scene-model.md), [user-data](../specs/user-data.md), [control-api](../specs/control-api.md)

## Context

The Mixer lists every source that makes sound in the program scene. A microphone or a music bed that the user never touches still takes a strip, and with many sources the Mixer becomes crowded. The user wants to put such sources away without muting them: a hidden source is still heard.

Where the flag lives decides what a workspace switch does. A flag in the workspace file would show the microphone again when another workspace is applied. The owner chose (2026-11-08) that the flag belongs to the source, like lock and mute, so a hidden microphone stays hidden in every workspace.

## Decision

1. **Model.** `SourceAudio` gains `mixer_hidden: bool` (default `false`). Hidden is a view choice only: it never changes volume, mute, sync offset or what the mixer renders. `rbs-scene` gets `set_source_mixer_hidden(source, hidden)` with its own undo record, like `set_source_sync_offset`. A change that sets the value already stored records nothing.
2. **Scene document format 6.** The key `mixer_hidden` is optional, written only when `true`, and a source without it is visible. A build that predates format 6 refuses the file, as for earlier format bumps. Specs `scene-model.md` and `user-data.md` change in the same PR.
3. **Control API 1.13.** New command `source.set_mixer_hidden { source, hidden }` (Scope::Control) and a `mixer_hidden` field on the source data. It takes the light path of mute: no graph rebind, no audio glitch. The existing `scene.changed` topic fires.
4. **Mixer UI.** Hidden sources are not drawn as strips or rows. After the visible groups the Mixer shows a section "Hidden (n)" with the section id `mixer.hidden` ([ADR-0078](0078-saved-section-state-and-shared-section-components.md)), closed by default and shown only when `n > 0`. Each hidden source is a compact row with its name, a level indicator is not drawn (no per-frame work for hidden sources in the Mixer), and a Show button. Each visible strip or row has a Hide button. The wide and narrow layouts share the same behaviour. A source that clips while hidden is not signalled (a later ADR may add a clip dot on the section header).
5. **Meters cost nothing for hidden sources.** The Mixer does not read or publish meter values for hidden sources (the Mixer's 30 Hz refresh builds fewer rows). The audio path is unchanged.
6. **Sources panel.** Not part of this decision. The same flag can drive a later filter there.
7. **Global sources.** The flag is on the global source, so it applies in every scene that uses it.

## Consequences

- A hidden microphone stays hidden across workspaces, scenes and restarts, and a collection shared with another machine carries it.
- Scene format 6 and API 1.13 are public contracts and need the independent review below.
- The Mixer audio plan (faders, master strip, ducking indicator) shares the `MeterRow` fields; this change adds only `hidden`, and the layout of strips is otherwise untouched.
- A user can forget that a hidden source is still audible. The "Hidden (n)" header is the reminder and stays visible while `n > 0`.

## Alternatives considered

- Store the flag in the workspace file: a workspace switch would bring the source back, which the owner rejected.
- Hide by muting: changes what is heard and loses the volume intent.
- A per-scene flag on the scene item: the same microphone would need hiding in every scene.

## Review

Pending: independent review (ADR-0014) and the owner's approval of the visual proposal (rules 15 and 16).
