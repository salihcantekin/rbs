# ADR-0195: Inactive scenes and filters for the Scenes and Sources lists

- Status: Proposed
- Date: 2026-11-26
- Deciders: Salih (owner)
- Builds on: [ADR-0059](0059-scenes-lock-and-source-audio-in-the-studio.md), [ADR-0163](0163-one-command-path-for-the-window-and-the-api.md)

## Context

A source can be hidden, but a scene cannot be taken out of play or out of the list. A show with many scenes needs both: scenes that are prepared but not for tonight must not be reached by a hotkey or an automation by mistake, and the Scenes list should be able to hide them. The Sources list also grows long and mixes kinds; users want to see only text, only windows, and so on, and to show or hide the hidden sources. The Scenes panel is narrow (owner, 2026-11-26), so the control must fit the panel header.

## Decision

1. **Scene state.** A scene is **Active** (default) or **Inactive**. `Scene` gains `active: bool`; `SceneCollection::set_scene_active(scene, active) -> Undo` is one undo step. The document stores `active = false` on the scene only when it is inactive, and the format version rises as ADR-0059 did for `locked`, so an older build refuses the file untouched.
2. **What Inactive means.** An inactive scene is skipped by "next scene" and "previous scene", by hotkeys and automations that switch by position, and by the control API's `scene.switch` when it is not addressed by id. Switching to it by id, or from the Scenes list, still works, so the user can edit it and the owner can still go live with it on purpose. The scene on air or in preview cannot be made inactive (`SceneOnAir` error, like removal, ADR-0059). Inactive changes nothing in flattening, binding or cost: the scene was not on air before either (P13).
3. **Control API.** `scene.set_active {scene, active}` (`edit`, with the on-air check); `scene.list` rows gain `active`. Window operations go through `studio_host::Command::Api` (ADR-0163); no new window-only command.
4. **Scenes panel.** The header gets a **filter button** (funnel icon) before the existing header buttons, with one switch, "Show inactive scenes" (default on). A dot on the button says the filter differs from the default. Each row gets a **power button** (a new `power` icon) on the right: it shows on hover and on inactive rows, its tooltip and accessible name say "Make {scene} inactive" or "Make {scene} active", and an inactive row is dimmed. The row menu has the same command (U3). The eye is not used, because it already means show or hide a source.
5. **Sources panel.** The header gets the same filter button. Its menu lists one checkbox per **source category**, "All" and "None" shortcuts, and a separate switch "Show hidden sources" (default on). A category is declared by the source type through its registry entry (a category id and its display name), not by a closed list in the engine or the host (rule 19, P16); a plugin type brings its own or falls into "Other". The filter changes only the list. A filtered-out source stays on air as it was, and a filtered-out source that is selected stays listed until the selection changes.
6. **Persistence.** The filter settings are view state of each panel, not part of the scene document. They are saved per panel and per scene collection with the other panel layout (the panels' saved state, ADR-0057 and the user-data spec), and a collection without saved filters shows everything.
7. **Names.** "Active/Inactive" for scenes and "Visible/Hidden" for sources are glossary terms; the glossary gains the first pair. A group row filters with its members: it is listed while any member is.

## Consequences

- Scene model, user-data document, control API, scene-switch callers (menu bar, shortcuts, automations, AI tools) and the Scenes and Sources panels change; one crate per change where possible. A conformance test covers "next scene" skipping inactive scenes, the on-air refusal, undo, and the document round trip.
- The ui-design-system spec gains the filter button, the power button and the dimmed row; the glossary gains Active/Inactive.
- Needs a source-type category in the type registry; until a type declares one it is "Other". Types that exist today are classified in the same change.
- Visual change: dark and light screenshots go to the owner before merge (rule 16).

## Alternatives considered

- **Eye button for scenes.** Same look as Sources, but the eye already means "show a source"; the same icon with two meanings was rejected by the owner.
- **Only a row menu command.** No extra row width, but not discoverable and slower than one click.
- **Hidden as a second scene state.** More flexible, but two flags are too much for a narrow panel; the list filter covers hiding.
- **A fixed category enum for sources.** Simpler, but it is a new closed list in the engine (rule 19).
- **Saving the filters in the scene document.** They would travel with a shared collection and enter undo; they are view state.

## Review

Pending: independent review per ADR-0014 (a human maintainer or a fresh AI session given only this ADR, the specs and the code).
