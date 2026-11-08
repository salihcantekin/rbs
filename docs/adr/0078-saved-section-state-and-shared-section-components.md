# ADR-0078: Saved section state and shared section components in every panel

- Status: Proposed
- Date: 2026-11-08
- Deciders: Salih (owner)
- Spec: [ui-design-system](../specs/ui-design-system.md#workspace-file)

## Context

[ADR-0076](0076-component-standards-and-panel-spacing.md) gave Properties tabs, collapsible sections and spacing floors, and said other windows would follow and that the open or closed state of a section would be saved later. The state is now a Slint global that is lost on restart. Settings cards, the Sources groups ("This scene", "Global") and the Mixer each draw their own headers.

## Decision

1. **Section ids.** Every collapsible section has a stable id of 1 to 64 characters from `a-z`, `0-9`, `.`, `_` and `-`, named `<panel>.<section>` (`properties.crop`, `sources.global`, `mixer.global`, `settings.recording.files`). An id is never reused for another meaning.
2. **Saved in the workspace file.** `closed_sections` is a sorted list of the ids the user closed, at most 256. Absent or empty means everything is open, so an existing file loads unchanged and the format number stays 3. A build that predates the key refuses the file (the parser rejects unknown keys), so it is written only when a section is closed. Applying a different workspace applies its list.
3. **Not layout.** Opening or closing a section is not in the layout undo history, and undo and redo keep the current list. A locked workspace allows it, like collapsing a stack.
4. **Defaults.** Every section starts open, except Sources > Global, which stays closed as today ([ADR-0065](0065-global-sources.md)). A default-closed section is stored the other way round: its id is listed when the user opens it, which the Sources panel does through the inverse id `sources.global-open`.
5. **Shared components.** Settings cards, the Sources groups and the Mixer groups use `SectionHeader` and the spacing tokens of ADR-0076 and draw no titles or gaps of their own. The Mixer picks its layout from the panel shape: a wide, low panel draws vertical meters side by side with each group's header above its columns (a closed group shrinks to its header button); a tall, narrow panel draws horizontal meters with full-width headers. The section ids are the same in both.

## Consequences

- A panel looks the same everywhere, and what the user folds stays folded after a restart and per workspace.
- The workspace file gains one optional key; the schema and the spec change in the same PR.
- The Properties `Sections` global is replaced by the saved state.

## Alternatives considered

- Save the state in a separate UI-state file: the state is part of how a workspace looks and should travel with it when shared.
- Store open sections instead of closed ones: new sections would start closed in old files.
- Put toggles in the layout undo history: a chevron is not a layout change and would make undo surprising.

## Review

Pending independent review (ADR-0014).
