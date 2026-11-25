# ADR-0184: Group Properties target the group and batch to its members

- Status: Accepted (decisions 3, 4 and 5 superseded in part by ADR-0185)
- Date: 2026-11-25
- Deciders: Salih (owner)
- Supersedes: ADR-0182 decisions 7 and 8 in part

## Context

ADR-0182 makes a group a tag on ordinary scene items. Selecting a group currently
selects its members with the first member as the primary source, so Properties
shows that member's fields and its edits affect only that item. This is inconsistent
with the owner's requested group flow: group properties and actions should belong to
the selected group and affect its members.

The existing group contract already batches transforms, visibility, locking and
removal, but explicitly leaves `show_for_ms` per item and does not define group-wide
Scroll edits. Groups may contain different source kinds or nested scenes. Source
settings belong to shared source objects, while timed show, scroll and transforms
belong to scene items or their layouts. See [ADR-0182](0182-source-groups.md),
[scene-model](../specs/scene-model.md), [ADR-0105](0105-timed-show.md) and
[ADR-0150](0150-scrolling-layers-in-the-compositor.md).

## Decision

1. When a group is selected, Properties must target that group, identify it by its
   group name, and show group-scoped controls instead of presenting the first member
   as though it were the sole target. Selecting one member explicitly for individual
   editing continues to show that member's Properties.
2. A group control is available only when its operation is valid for every member.
   It must apply to every member as one group action; the UI must not silently skip
   unsupported members. An unavailable control must explain which requirement is
   not met. Group members with different values must be shown as mixed until the
   user edits the value, at which point the chosen value applies to all.
3. This applies to item- and layout-scoped properties and actions, including timed
   show duration and Show now, and Scroll settings and its live controls. Saved
   multi-item changes must form one undo step; live actions remain outside undo.
   Layout edits target the canvas selected in Properties, creating the edited
   canvas's override where needed and preserving the default and other canvases.
   Visibility, locking and removal retain their group-wide behaviour. Moving,
   nudging or resizing a group with any locked member must be refused as a whole,
   with the lock identified; unlike ADR-0182 decision 7, unlocked members do not
   move on their own. Group rotation, opacity, clipping and filters remain
   unavailable as in ADR-0182.
4. Source-owned settings and filters are not group properties: changing them edits a
   shared source and can affect placements outside this group. They remain available
   when an individual member is selected. Controls unsupported by any member, such
   as Scroll when the group contains a nested scene, are unavailable for the group.
5. A group Scroll live action must be unavailable when any affected run is shared
   with an item outside the group, with the reason shown. Preflight must also refuse
   a group action if a member cannot perform it, before changing any run. Do not
   split shared runs or change the existing per-item live Scroll contract.
6. Add a versioned server-side `group.apply` control API operation for the
   group-scoped edits and live actions offered by Properties and for group
   transforms. Its request identifies the scene, group, expected member item ids,
   selected canvas where relevant, and the typed operation and its parameters.
   The host must compare the expected membership to the current group, validate
   every member and any shared runs, and execute the whole action on the control
   thread; stale membership or any refusal changes nothing and is reported to the
   window. Saved changes produce one undo record; live actions produce none.
   Existing single-item methods keep their contracts. The current
   `item.set_show_for` has no gesture, and a gesture alone cannot make separate
   item calls atomic; the duration edit must use this group operation. Define the
   operation variants, response, error and older-server behaviour in the control
   API spec before implementation.

## Consequences

- Properties represents the selected group instead of an arbitrary primary member.
  Mixed-value state and a group-specific properties view are required.
- Group edits must remain atomic from the user's perspective: one undo step for a
  saved edit, and all eligible members for a live action.
- Update the scene-model, control-api, UI design-system and glossary specs
  alongside implementation.
- Tests must cover group targeting, mixed and common values, mixed source kinds,
  unsupported operations, one-step undo, failed validation without partial changes,
  canvas isolation, locked-member refusal, stale membership and live actions
  across all members including shared runs.
- UI review, dark and light screenshots, a keyboard walkthrough and owner approval
  are required before merge.

## Alternatives considered

- **Keep the first member as the Properties target.** Rejected because edits that
  appear to target a selected group affect only one member.
- **Show a member's values and apply edits to only compatible members.** Rejected
  because it silently leaves part of the selected group unchanged.
- **Make source settings group-wide.** Rejected because a source can be shared by
  placements outside the group; those settings are not group-scoped.

## Review

Independent AI review, 2026-11-25: changes requested. Findings: Scroll live actions
can affect shared runs outside the group; timed-show duration lacks a batching API;
and canvas scope was unspecified. Decisions 3, 5 and 6 now address those concerns;
the owner selected blocking unsupported operations and shared outside runs on
2026-11-25.

Independent AI follow-up review, 2026-11-25: changes requested. The first three
findings were resolved; group transforms conflicted with locked-member behaviour,
and individual API calls could not provide atomic group edits. Decisions 3 and 6
now block the whole transform when any member is locked and require a server-side
group operation. The owner selected the locked-member behaviour on 2026-11-25.
Independent AI follow-up review, 2026-11-25: approved, no blocking findings.
The reviewer confirmed the locked-member rule and server-side atomic group
operation resolve the remaining concerns. The control API spec must define the
operation variants and wire behaviour before implementation.

Owner approval: Salih, 2026-11-25, after the independent review.
