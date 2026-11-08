# ADR-0078: Saved section state and shared section components in every panel

- Status: Proposed
- Date: 2026-11-08
- Deciders: Salih (owner)
- Spec: [ui-design-system](../specs/ui-design-system.md#workspace-file)

## Context

[ADR-0076](0076-component-standards-and-panel-spacing.md) gave Properties tabs, collapsible sections and spacing floors, and said other windows would follow and that the open or closed state of a section would be saved later. The state is now a Slint global that is lost on restart. Settings cards, the Sources groups ("This scene", "Global") and the Mixer each draw their own headers.

## Decision

1. **Section ids.** Every collapsible section has a stable id of 1 to 64 characters from `a-z`, `0-9`, `.`, `_` and `-`, named `<panel>.<section>` (`properties.crop`, `sources.global`, `mixer.global`, `settings.recording.files`). An id is never reused for another meaning.
2. **Saved in the workspace file.** `closed_sections` is a sorted list of the ids the user closed, at most 256. Absent or empty means everything is open, so an existing file loads unchanged and the format number stays 3. A build that predates the key refuses the file (the parser rejects unknown keys), so it is written only when a section is closed. Applying a different workspace applies its list; this includes the Settings cards, so what is folded in Settings follows the workspace (a simple rule; a later ADR may move `settings.*` ids to an app-level file).
3. **Not layout.** Opening or closing a section is not in the layout undo history, and undo and redo keep the current list. Reset workspace keeps it too. A locked workspace allows it, like collapsing a stack. Like collapsing a stack, a change is saved with the workspace file the same way as any layout change.
4. **Defaults.** Every section starts open, except Sources > Global, which stays closed as today ([ADR-0065](0065-global-sources.md)). A default-closed section is stored the other way round: its id is listed when the user opens it, which the Sources panel does through the inverse id `sources.global-open`.
5. **Shared components.** Settings cards and the Sources groups use `SectionHeader` and the spacing tokens of ADR-0076 and draw no titles or gaps of their own. A page with foldable Settings cards has one small Collapse all / Expand all button next to its title. The Mixer follows in a later change with the same components; its direction is decided here: it picks its layout from the panel shape: a wide, low panel draws vertical meters side by side with each group's header above its columns (a closed group shrinks to its header button); a tall, narrow panel draws horizontal meters with full-width headers. The section ids are the same in both.

## Consequences

- A panel looks the same everywhere, and what the user folds stays folded after a restart and per workspace.
- The workspace file gains one optional key; the schema and the spec change in the same PR.
- The Properties `Sections` global is replaced by the saved state.

## Alternatives considered

- Save the state in a separate UI-state file: the state is part of how a workspace looks and should travel with it when shared.
- Store open sections instead of closed ones: new sections would start closed in old files.
- Put toggles in the layout undo history: a chevron is not a layout change and would make undo surprising.

## Review

Round 1, independent AI session (2026-11-08): changes requested. Resolved in the same change: the first answer of a new window could stay stale (the windows count now bumps the revision, and the busy fallback is the section's default); Reset workspace keeps the list (test added); Switch takes the other workspace's list (test added); the Mixer layout is marked as a follow-up; Settings cards follow the workspace (stated above); the id-list test scans every `ui/*.slint` file. Saving works as for collapsing a stack. Noted, not changed: a failed `set_section_closed` is silent (about 40 ids exist against a cap of 256). A round 2 review and the owner's approval are needed before Accepted.
