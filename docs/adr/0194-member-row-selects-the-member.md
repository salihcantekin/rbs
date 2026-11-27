# ADR-0194: A member row in the Sources list selects that member

- Status: Accepted
- Date: 2026-11-25
- Deciders: Salih (owner)
- Supersedes in part: ADR-0182 decision 9, for clicks in the Sources list only
- Builds on: [ADR-0182](0182-source-groups.md), [ADR-0184](0184-group-properties-batch-to-members.md)

## Context

ADR-0182 decision 9 makes a click on a group member, in the Sources list or on
the picture, select the whole group; Ctrl/Cmd+click selects the member alone.
With ADR-0184 group Properties, a click on any member row therefore shows the
group. The owner found editing one member's own properties too hard: the
member's row looks like an ordinary source row but cannot be selected on its
own without a modifier the user does not discover.

## Decision

1. **Sources list.** A plain click, Return or Space on a member row selects
   that member alone, and Properties shows its individual Properties. A click
   on the group row selects the group, and Properties shows the group
   (ADR-0184); the group row ignores Ctrl/Cmd and Shift. On member rows,
   Ctrl/Cmd+click and Shift+click keep their ordinary multi-selection meaning:
   the member is added to or removed from the selection like any other row.
   The special case that Ctrl/Cmd+click on a member of a selected whole group
   selects that member alone is removed; it now drops that member, leaving the
   remaining members (one left is a single selection) with no group target. As the ui-design-system spec and
   `properties_target::whole_group` already say, a selection that is exactly
   one group's members targets the group, so Ctrl/Cmd+clicking the second
   member of a two-member group, or a Shift range covering exactly one group's
   block, switches Properties to the group.
2. **Other member-row actions.** Right-click keeps the selection if the member
   is part of it, otherwise selects the member alone; the menu acts on that
   selection (U3). Delete or Backspace on a member row removes only that member,
   even while the whole group is selected; a group left with one member
   dissolves (ADR-0182 decisions 4 and 8). The whole group is removed only from
   the group row. Dragging a member row and Alt+Up/Down move only that member
   within its block; moving it out is refused (`GroupNotContiguous`). Dragging
   the group row moves the group. Double-click selects the member alone, then
   opens its source settings. Member rows carry an accessible hint saying a
   member row selects that member and the group row selects the group (U7).
3. **Picture.** A press, including a right-click, on a member in any picture
   (Preview, Program, Vertical) selects
   the whole group, and dragging moves the group (ADR-0182 decision 9), except
   when that member is already selected alone: then handles, drags and arrow
   keys edit that member alone. A press on any other member selects its group.
   The picture's Ctrl/Cmd keeps its existing no-snap meaning; selecting a
   member alone from the picture is not offered (the list provides it).
4. A member selected alone keeps its group tag; its edits are single-item edits
   and do not batch to the group.

## Consequences

- One click reaches either target from the list; the picture keeps the
  group-first behaviour except for a member the user chose in the list.
- `select_one` is shared by the Sources list and the pictures today; split it so
  the list path selects the member and the picture paths keep the group.
- Rewrite the ui-design-system sentences on Ctrl/Cmd+click on a selected group
  member and on group-row selection; replace the two `control_click_*` tests in
  `multi_select.rs`.
- Tests: a plain click or Return on a member row gives the member's own
  Properties; a group-row click gives the group; a picture press on an
  unselected member selects the group; a picture press on a member selected
  alone keeps it, also for a right-click and on the Vertical picture;
  Ctrl/Cmd+click on a member of a selected group leaves the
  rest with no group target; Delete on a member row removes only that member.
- UI review checklist; the owner waived screenshots for group Properties on
  2026-11-25 because only existing components are used.

## Alternatives considered

- **Keep Ctrl/Cmd+click for a member.** Rejected by the owner: hard to find.
- **Member also on the picture.** Rejected by the owner: a press on an
  unselected member on the picture would then move it alone, breaking moving
  the group as one. Only a member the user chose in the list edits alone there.

## Review

- Independent AI review, 2026-11-25: changes requested. Findings: picture
  behaviour after a member-row click, the picture's Ctrl/Cmd, the removed
  Ctrl/Cmd special case, the cited rule's source, group-row modifiers, other
  member-row actions and specific tests. Decisions 1-3 and the Consequences
  now address each.
- Independent AI re-review, 2026-11-25: approve, no blocking findings. Four
  wording fixes (Vertical picture, picture right-click, the one-left case and
  splitting `select_one`) are applied above.

Owner approval: Salih, 2026-11-25, after the independent review.
