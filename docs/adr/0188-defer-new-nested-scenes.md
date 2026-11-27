# ADR-0188: Scenes cannot contain scenes

- Status: Accepted
- Date: 2026-11-25
- Deciders: Salih (owner)
- Supersedes in part: ADR-0021 decisions 2, 3 and 7; ADR-0051 decision 6; ADR-0131 decisions 4, 6 and 7; ADR-0185 decision 1; architecture principle P9

## Context

The scene model supports one scene as an item of another, including scenes
imported from OBS. Group Scroll would need to compose such scenes recursively
([ADR-0185](0185-scroll-a-group-as-one-picture.md)). The owner said there are no
existing nested scenes to preserve and chose to drop nesting and that extra
group Scroll complexity.

## Decision

1. A scene must not contain a scene item. Keep `ItemContent::Scene` as a
   compatibility variant, but reject it with `SceneError::NestingUnsupported` on
   `from_parts`, direct additions, scene-item replacement, paste, both scene
   duplication paths and every other user edit or batch. Preflight before
   publishing changes, consuming IDs, adding history or side effects. Undo,
   redo and snapshots built from valid state cannot introduce a nested item.
   Hold, transition and replay snapshots must be built from valid state too.
   The existing layer limit remains; the nesting-depth limit no longer governs
   valid collections. `from_parts` checks for scene items before its other model
   invariants, so scope, cycle, depth and layer-limit errors do not hide this
   refusal. Its display reason is "Scenes cannot be placed inside other scenes."
   The scene spec must list every entry point and error.
2. A document containing a nested-scene item must be refused with an explicit
   error, without overwriting or silently changing it. Documents without such
   items must continue to load. Surface the dedicated model error through
   `BodyError::Scene` and `DataError::Invalid`; collection switching must fail
   without replacing the current collection, and startup must report the error
   and leave the file untouched. The serializer's `to_body` must validate before
   producing a body and return `BodyError::Scene(NestingUnsupported)` on this
   invariant failure; no backup, move or write follows that failure. It must not
   repair or remove items.
3. OBS import must skip each nested-scene placement and report it as skipped,
   identifying the containing scene and the reason, including inside OBS
   groups. Each report entry identifies the referenced scene as `obs_name`, the
   containing scene in `scenes`, uses `Outcome::Skipped` and `Area::Scene`, and
   says "Scenes cannot be placed inside other scenes." This also applies inside
   an OBS group. `SceneChoice::All` imports every scene separately;
   `SceneChoice::Only` imports exactly the selected scenes and no longer
   implicitly adds their nested scenes. Count groups after omissions: a group
   with fewer than two surviving items becomes separate items with the existing
   `Approximated` group entry, in addition to each skipped placement entry. The
   importer must not silently flatten the nested scene into its parent.
4. Group Scroll composes groups of source items; it does not need nested-scene
   composition. This supersedes ADR-0185's requirement to compose nested-scene
   members and groups inside nested scenes. Its other group Scroll semantics
   remain unchanged.
5. **Compatibility exception.** At the owner's explicit direction, this
   removal is an exception to P9's previous-major support and ADR-0051
   decision 6's mandatory repairing upgrade: even older formats with a nested
   scene are refused. No format number is added solely for this removal,
   because no new document field is written; otherwise preserve the existing
   conditional format selection, including group Scroll's format 31. Older
   readers can still read supported files at formats they know. Older software
   may create nested files which this version rejects. No project may be
   silently rewritten or lose nested items. `rbs-scene` crates are unpublished;
   retaining the enum variant avoids a Rust source break but its acceptance
   semantics change. The existing public `item.add` accepts sources, not scene
   items. `scene.duplicate`, clipboard/paste and undo operate only on valid
   supported state and continue to work for that state; an attempted unsupported
   model edit exposed by a control method returns `invalid_params` (-32602)
   with the reason above. `collection.switch` retains its existing asynchronous
   outcome: load and validate the target before closing views or changing the
   running studio or selected collection id; on failure the original selection,
   collection and runtime remain, pending clears and the error line reports
   the refusal. `import.obs.check` and `import.obs.run` use the new report and
   selection semantics for all clients. The owner explicitly approves this
   change in behaviour for existing clients as an exception to P9 and the
   control API's major-version rule. Document it in protocol 1.109, after the
   already reserved 1.108; do not advertise 1.109 until all 1.108 and 1.109
   changes are implemented.
6. Before implementation, update the scene model, OBS import, user-data,
   control API, compositor and UI contracts to specify the exact error,
   old-client behaviour and unsupported-document case. Remove their conflicting
   nesting and recursive import requirements. Review those contracts
   independently before implementation; this is a separate gate.

## Consequences

- An unexpected existing project with nested scenes is not opened and needs
  conversion before it can be used; there is no automatic conversion.
- An OBS import can omit content that was visible through a nested scene; its
  report must make that loss explicit.
- Group Scroll no longer needs to compose nested-scene members. Its other
  resource, snapshot and review requirements remain.
- Tests must cover document refusal without data loss, undo and redo, held
  copies, each edit and copy path, and OBS report and omission.

## Alternatives considered

- **Keep allowing new nested scenes.** Rejected by the owner for now because of
  the additional complexity.
- **Keep existing nested scenes but forbid new ones.** Rejected by the owner
  after confirming there are no existing nested scenes to preserve.
- **Import nested scenes without reporting or flatten them silently.** Rejected
  because this would conceal a change in the imported composition.

## Review

Independent AI review, 2026-11-25: changes requested. The reviewer requested
explicit compatibility, model and loader errors, all edit and copy paths,
snapshot invariants, OBS scene selection, and group Scroll contract changes.
The owner approved exceptions to old-document and old-client compatibility and
the mandatory repairing upgrade on 2026-11-25. Follow-up reviews requested
precise error, serialization, API and import contracts and error precedence;
this revision addresses them.

Independent AI follow-up review, 2026-11-25: approved, no decision-level
blockers.

Owner approval: Salih, 2026-11-25, after the independent review.

Independent AI contract review, 2026-11-25: approved after correcting the
control API's copy and on-air wording and the media-graph compositor description.
The implementation must still satisfy the reviewed contracts and their tests.

Owner clarification, 2026-11-27 (correction in place, decision unchanged): this ADR
was drafted as a deferral. The owner confirmed that was a misunderstanding: RBS
does not want scenes inside scenes at all, now or later. The decisions above
already refuse every nested-scene path, so they stand. Only the wording changes:
"defer" and "deferred" mean "not supported". The file keeps its name so links
stay valid. A future nested-scene feature would need a new ADR.
