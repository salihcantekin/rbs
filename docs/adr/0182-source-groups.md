# ADR-0182: Sources can be grouped, and a group moves, resizes and hides as one

- Status: Proposed
- Date: 2026-11-24
- Deciders: Salih (owner)
- Spec: [scene-model](../specs/scene-model.md), [ui-design-system](../specs/ui-design-system.md) (Sources panel), [control-api](../specs/control-api.md)
- Builds on: [ADR-0173](0173-companion-sound-nested-under-its-picture.md) (`ListRow` disclosure arrow and `size.list.indent`), [ADR-0131](0131-obs-import.md) (OBS groups are flattened today), [ADR-0084](0084-source-transform-commands-and-source-context-menu.md) (item commands)

## Context

The owner asked (2026-11-24) for grouping: several sources used as one, moving and resizing together, with Group and Ungroup in the Arrange menu and an open/close row in the Sources list like the companion sound row. Selecting several rows in the list and moving or scaling them together already exists (multi-selection, `rbs_ui::group_transform`, one undo step through a shared gesture), but it lasts only while selected. A nested scene already composes transforms, but it is a separate scene with its own list entry, an extra concept for the user, and ADR-0131 rejected groups as nested scenes for that reason.

## Decision

1. **A group is a tag on items, not a layer type.** A scene stores `groups: Vec<Group { id: GroupId, name }>` and each `SceneItem` has `group: Option<GroupId>`. Flattening, compositor and binder do not change; members stay ordinary layers, so a group costs nothing per frame (P13). `GroupId` is a new counter in `IdCounters` (stored in the document, never reused, as every id). Scene-header records (`ReplaceScene`, `set_scene_*`) carry and expect `groups`. `duplicate_scene` and `duplicate_source` issue fresh `GroupId`s.
2. **Shape.** A group has two or more members, all items of its scene (source or nested-scene items), all scene items or all global-scene items of one canvas scope, never both. One level: a group cannot contain a group. Names are non-blank and unique in the scene.
3. **Invariants** (checked on every edit, each with a `SceneError` variant and a test): every `item.group` names a group of the same scene and every group member points back (`UnknownGroup`); at least two members (`GroupTooSmall`); members are contiguous in layer order (`GroupNotContiguous`); no scene/global mix (`GroupScope`); names (`DuplicateName`).
4. **Existing edits keep the block.** Moving a member within the block is allowed; moving it out of the block is refused (`GroupNotContiguous`; Ungroup first, or move the group). `add_item` at an index inside a block, paste and `duplicate_source` place the new item directly above the block; a companion sound is placed above its picture's block and is not a member unless grouped. Removing, deleting or removing-from-scene a member leaves the group; a group left with one member is dissolved in the same step. Nesting a scene updates nothing else.
5. **Group and Ungroup.** Group takes the selected items (and the members of any selected group; groups merge into one) and moves them next to the topmost one, keeping their order. This lifts them over non-members that sat between them; the user sees that in the picture and Undo restores the exact order (accepted, not prevented). Ungroup removes the tag and keeps the order.
6. **Undo.** A new `Op::SetGroups { expect, set }` for the group list, used inside `Op::Batch` with `ReplaceItems`, so a group edit, a dissolve and the member edits of one gesture are one undo step. `RemoveSource` / `InsertSource` records restore items' `group` and the list.
7. **Group edits are batches of member edits.** Move, nudge, proportional resize, flip, show/hide, lock and remove apply to every member as one undo step on the edited canvas: each member gets a per-canvas override as a multi-selection edit does today, starting from its default layout. There is no stored group transform; the group box is the box covering the members. Scroll, style lengths and crop follow a resize as in multi-selection. Group rotation is not offered; resizing members that are rotated is as approximate as OBS import (ADR-0131). Locked members stay where they are while the others move. Group opacity, clipping and filters are not offered (they need render-to-texture; scene-model spec).
8. **Mixed state.** The group row's eye shows hidden only when every member is hidden; a click hides all when any is visible, else shows all. Lock works the same. `show_for_ms` and cues stay per item and are untouched. Delete or Backspace on a group row asks to remove all its items from the scene (one step, the ADR-0142 remove question); Ungroup is a separate command.
9. **Selection and Sources list.** Clicking a member in the list or on the picture selects the whole group; Ctrl/Cmd+click selects that member alone to edit it, and it keeps its group. A group is one row with the disclosure arrow, combined Show/Hide and Lock, closed by default, open state in memory only. Members sit under it at `size.list.indent`; a companion sound is nested under its picture inside the group (one more indent), and `sound_nest.rs` orders within each group block. The group row shows no member marks.
10. **Commands and API.** Registry entries `item.group` and `item.ungroup` in Arrange (enabled for two or more items, or a group). Control API (minor protocol bump): `group.create {items, name?}` -> `{group}`, `group.remove {group}` (ungroup), `group.rename {group, name}`, `group.list {scene?}` -> rows `{group, name, scene, items}`; `scene.sources` rows gain `group`. Errors: `not_found` for unknown ids, `invalid_params` for items of two scenes, global and scene items together, or fewer than two items. All are undoable like `item.*`; existing `item.*` methods act on the one item named (group behaviour is the UI's batch), and an `item.move` that would leave a block is refused. The existing scene-change events cover them.
11. **Format.** The document format is bumped by one (the next free number when implemented). It is written only when a scene has a group, so older builds still load other files (as formats 15/16 did); an older reader refuses a file with groups. User-data spec: item key `group`, scene table `[[groups]]`. In the `AttributeGroup` split, `group` belongs to none (never copied, pasted or reset with attributes).
12. **OBS import.** A group becomes a group; its transform is still composed into the children, so the picture is unchanged. This partly supersedes only the `group` row of ADR-0131's mapping table (the hide rule maps to per-item `visible`; crop on a group item stays approximate). ADR-0131 gets a note pointing here when this is accepted.
13. **Plugin-ready, zero cost.** Groups know nothing about source kinds; plugin sources group like any other; no closed list grows; nothing runs per frame.
14. **Tests.** The property test (random edits, undo to start, redo) and the out-of-order record test gain group edits and `Op::SetGroups`; invariants in `crates/rbs-scene/tests`; a list-ordering test; an OBS import golden; geometry reuses `group_transform`.

## Consequences

- OBS-style groups with proportional resize and one undo step; copying a group to another scene is a later change.
- A group's picture equals its members' pictures: overlapping semi-transparent members still show through each other until render-to-texture exists.
- Public contracts change (scene format, control API, Sources panel, menus): independent review (ADR-0014) and owner approval first; implementation after.
- Specs updated in the same change: scene-model (groups, invariants, errors), user-data, control-api, ui-design-system (Sources panel, Arrange), obs-import, and ADR-0131's note.
- Align and distribute (geometry exists) stay a separate, smaller change on any selection.

## Alternatives considered

- **Group as a nested scene.** Reuses the composition math, but adds an invisible scene to the collection and list and an extra composite per group (ADR-0131 rejected it); crop and style are refused on nested scenes.
- **A group item with its own stored transform.** Allows group opacity later, but every member's box would be relative to the group, so ungrouping and cross-scene copies need conversions, and every layout read changes.
- **Keep only the temporary multi-selection.** Cheapest, but it cannot hide, lock or reorder a set, and the owner asked for the open/close row.

## Review

Independent review per ADR-0014, 2026-11-24, fresh AI session: changes requested, 13 findings (storage and ids, invariants, edits that split a block, lifted order, undo ops, per-canvas layouts, mixed state, selection cases, two-level list, format, ADR-0131 partial supersede, API details, tests). All taken into the text above. Second review and owner approval: pending.
