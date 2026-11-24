# ADR-0180: Sources can be grouped, and a group moves, resizes and hides as one

- Status: Proposed
- Date: 2026-11-24
- Deciders: Salih (owner)
- Spec: [scene-model](../specs/scene-model.md), [ui-design-system](../specs/ui-design-system.md) (Sources panel), [control-api](../specs/control-api.md)
- Builds on: [ADR-0173](0173-companion-sound-nested-under-its-picture.md) (`ListRow` disclosure arrow and `size.list.indent`), [ADR-0131](0131-obs-import.md) (OBS groups are flattened today), [ADR-0084](0084-source-transform-commands-and-source-context-menu.md) (item commands)

## Context

The owner asked (2026-11-24) for grouping: several sources used as one, moving and resizing together, with Group and Ungroup in the Arrange menu and an open/close row in the Sources list like the companion sound row. Selecting several rows in the list and moving or scaling them together already exists (multi-selection, `rbs_ui::group_transform`, one undo step through a shared gesture), but it lasts only while selected. A nested scene already composes transforms, but it is a separate scene with its own list entry, an extra concept for the user, and ADR-0131 rejected groups as nested scenes for that reason.

## Decision

1. **A group is a tag on items, not a new layer type.** Each scene stores `groups: Vec<Group { id: GroupId, name }>` and each `SceneItem` gets `group: Option<GroupId>`. Flattening, the compositor and the binder do not change: members stay ordinary layers, so a group costs nothing per frame (P13).
2. **One level, contiguous.** A group has two or more members of one scene, on one canvas scope. Members are contiguous in layer order (grouping moves them next to the topmost member, keeping their order); Move up/down/front/back on a group moves the block. A group cannot contain a group. A global-scene item and a scene item cannot share a group.
3. **Group edits are batches of member edits.** Move, resize (proportional, as multi-selection does today), nudge, flip, rotate about the group centre, show/hide, lock and remove apply to every member as one undo step. There is no stored group transform: the group box is always the box covering its members. Group opacity, clipping and group filters are not offered (they need render-to-texture; scene-model spec, nested scenes).
4. **Selection.** Clicking a member in the Sources list or on the picture selects the whole group; Ctrl/Cmd+click on a member of a group selects that member alone, to edit it. A member edited alone keeps its group.
5. **Sources list.** A group is one row with the disclosure arrow (ADR-0173), its name, the combined Show/Hide and Lock, closed by default, open state kept in memory. Members sit under it at `size.list.indent`. Rename is the usual row rename. New tokens: none expected; a new token needs the `ui-review` procedure.
6. **Commands.** Registry entries `item.group` and `item.ungroup` in the Arrange submenu of the source context menu and the Source menu, enabled with two or more items selected (Group) or when the selection is or contains a group (Ungroup). Control API: `group.create {items, name?}` -> `{group}`, `group.remove {group}` (ungroup, members stay), `group.rename {group, name}`, `group.list {scene?}`; `scene.sources` rows gain `group`. Each is one undo step.
7. **Format.** The scene collection document format is bumped by one (the next free number on main when implemented); older documents load with no groups. Invariants (checked on every edit): members exist and belong to the scene, a group has at least two members, a group id is unique in the collection, nesting a scene or deleting a source updates its groups (a group left with one member is dissolved, restored by undo).
8. **OBS import.** A group in an OBS collection becomes a group here instead of being flattened; its transform is still composed into the children as today, so the picture is unchanged (ADR-0131 decision for `group` is replaced by this for the grouping information only).
9. **Plugin-ready.** Groups know nothing about source kinds; plugin sources can be grouped. Nothing here adds a closed list.
10. **Tests.** Property test additions for the invariants and undo; geometry reuses `group_transform` tests; a list-ordering test like `sound_nest.rs`; OBS import golden for a group.

## Consequences

- Users get OBS-style groups with proportional resize and one undo step, and groups can be reused across scenes by copying (a later change may copy a group with its scene items).
- A group's picture is the same as its members' pictures: overlapping semi-transparent members still show through each other until render-to-texture exists.
- Public contracts change: scene format, control API (new methods, `scene.sources` field), the Sources panel and menus. This needs the independent review of ADR-0014 and the owner's approval; implementation follows only after that.
- Spec updates in the same change: scene-model (groups, invariants), control-api, ui-design-system (Sources panel, Arrange), obs-import.
- Align and distribute (geometry exists) are a separate, smaller change and work on any selection, grouped or not.

## Alternatives considered

- **Group as a nested scene.** Reuses the composition math, but adds an invisible scene to the collection and list and an extra composite per group (ADR-0131 rejected it); crop and style are refused on nested scenes.
- **A group item with its own stored transform.** Allows group opacity later, but every member's box would be relative to the group, so ungrouping and cross-scene copies need conversions, and every layout read changes.
- **Keep only the temporary multi-selection.** Cheapest, but it cannot hide, lock or reorder a set, and the owner asked for the open/close row.

## Review

Independent review per ADR-0014: pending.
