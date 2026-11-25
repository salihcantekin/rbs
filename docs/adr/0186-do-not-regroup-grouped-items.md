# ADR-0186: Do not regroup items already in a group

- Status: Accepted
- Date: 2026-11-25
- Deciders: Salih (owner)
- Supersedes in part: ADR-0182 decisions 5 and 10 (group merging) and ADR-0096 decision 2 (companion creation for multiple placements)
- Exception: control API versioning for this change only, with the owner's approval below
- Builds on: [ADR-0184](0184-group-properties-batch-to-members.md), [ADR-0185](0185-scroll-a-group-as-one-picture.md)

## Context

ADR-0182 prohibits a group inside a group, but `group.create` also merges any
existing groups represented in its selection into a new group. ADR-0185 adds
group-owned Scroll settings per canvas, so a merge would have to choose which
groups' settings to keep. The owner clarified on 2026-11-25 that a selection
containing an already-grouped item must not be grouped again, including when
the selection also contains ungrouped items.

## Decision

1. **Group only ungrouped items.** Group and `group.create` must refuse the
   complete request if any selected item is already in a group. This applies to
   a single member selected for individual editing, a whole group, multiple
   groups and a group with ungrouped items. It must not merge groups, add the
   ungrouped items to an existing group, change any setting or create an undo
   record. This replaces ADR-0182's group-merging rule.
2. **Ungroup first.** Ungroup remains a separate undoable operation. Once the
   selected items are all ungrouped, they may be grouped as before. Ungrouping
   a scrolling group removes its group-owned settings with that group; Undo
   restores them and its members. A member's own Scroll is separate and is
   not changed by ungrouping or group Scroll.
3. **One rule at every entry point.** The scene model validates before issuing
   ids or mutating state. `group.create` returns `conflict` with a reason; the
   window disables Group with a reason telling the user to ungroup first. An
   older API client receives the same refusal, rather than silently merging or
   losing settings. The existing document format and valid groups are unchanged.
4. **Companion sound.** Expand a selected picture to its companion sound item
   in the same scene, using the existing `companion_of` source relation.
   After request decoding, validate the scene,
   then the blank name, then explicit item ids and their scene in request
   order, then the two-distinct-item minimum on the explicit selection, then
   expansion ambiguity, existing group membership and duplicate group name, in
   that order. Expansion does not satisfy the minimum. An unknown scene or
   item is `not_found`; a blank name, an item in another scene or too few
   distinct explicit items is `invalid_params`; ambiguity, existing
   membership or a duplicate name is `conflict`. In
   particular, an unknown scene wins over a blank name, and a blank name wins
   over an unknown item. For each selected picture with a same-scene
   companion placement, require exactly one same-scene picture placement and
   exactly one companion placement; otherwise refuse as ambiguous. A companion
   placed in a different scene is not pulled in. Deduplicate an explicitly
   selected companion with the one found by expansion. Validate both explicit
   and expanded items: any already grouped item refuses the entire request,
   including an implicitly added companion. Create the group and assign all of
   those items in the same model batch. Adding a new companion by source id is
   not a Group request. Require exactly one picture item placement across all
   scenes; if there are multiple, refuse with `conflict` before changing the
   source, items, ids or history. If there are none, retain the existing
   no-placement error. Place the new companion directly above the unique
   picture item in layer order, with its group tag when present, as one atomic
   undoable add; this preserves the group's contiguous block. Implement this
   with a transactional model operation or complete rollback, including ids
   and history on failure after staging. If the companion add is part of
   `source.add` with audio, failure leaves the entire request unchanged. This
   applies through the window and `source.set_companion_audio`, including old
   clients. Existing companions and their multi-placement pictures remain
   valid and are not changed on load. The one-companion and sound visibility
   rules of ADR-0096 remain; only the creation rule is superseded. Keep
   ADR-0182's companion removal and ungroup rules.
5. **Compatibility exception.** Protocol 1.108 documents the refusal for
   `group.create` and applies it to every client, including 1.104–1.107 clients.
   The protocol normally requires a major bump for a changed method meaning;
   regrouping refusal, companion expansion and multiple-placement companion-add
   refusal of decision 4 are owner-approved exceptions, like the explicitly
   listed compatibility exceptions in the control API versioning spec. All
   other versioning rules remain. Existing clients still connect and must
   handle `conflict` and the additional companion member; none is silently
   allowed to merge groups. `group.create` still answers `{group, name}`:
   callers use `group.list` or scene rows to discover the resulting members.

## Consequences

- The existing `group.create` behaviour changes for old clients; the control API
  specification and protocol 1.108 notes must state the exception. Update the
  scene-model and UI specs in the implementation change.
- A user who wants a different membership must ungroup and group again. Group
  settings are removed on ungroup, with Undo available; individual item settings
  stay intact.
- Test refusal for one grouped member, a whole group, two groups, and a group
  with ungrouped items, including groups loaded from a document and unchanged
  ids, layouts, settings and undo history. Test normal grouping, companion
  expansion and ambiguous placements, unknown scene with blank name, blank name
  with an unknown id, error precedence through duplicate name, a single
  picture whose companion cannot satisfy the minimum, companion
  add with multiple placements across and within scenes (including unchanged
  source, items, ids and undo history), a grouped picture's insertion order,
  failure after staging and complete rollback, ungroup and undo.

## Alternatives considered

- **Merge groups and ask which Scroll settings to keep.** Rejected by the owner:
  items already in a group must not be grouped again.
- **Merge and silently keep one group's settings.** Rejected: it would discard
  another group's settings without a choice.

## Review

Independent AI review, 2026-11-25: changes requested. The reviewer found that
the compatibility break needed a versioning strategy and that automatic
companion membership was not established by the current grouping path. Decisions
4 and 5 now specify both. A follow-up review requested explicit versioning of
companion expansion and an unambiguous repeated-placement rule; decisions 4 and
5 now state both. Owner approval of the targeted minor-version exceptions for
regrouping refusal and companion expansion: Salih, 2026-11-25. Another
review requested clarification of versioning, error precedence and the target
of a source-scoped companion add. The owner chose refusal for multiple picture
placements and approved extending the targeted 1.108 exception to that refusal.
The next review requested the ADR-0096 supersession, exact insertion order,
transactional failure handling and membership discovery; decision 4 and 5
now specify them. A further review requested precise partial supersession and
error precedence; the header and decision 4 now specify these. A further
review requested the minimum and duplicate-name precedence; decision 4 now
specifies both. Independent AI review, 2026-11-25: approve, no blocking
concerns. Owner approval: Salih, 2026-11-25. Implementation does not yet
follow this accepted ADR.
