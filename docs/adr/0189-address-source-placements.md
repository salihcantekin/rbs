# ADR-0189: Address source placements independently

- Status: Proposed
- Date: 2026-11-25
- Deciders: Salih (owner)
- Supersedes in part: ADR-0065 decisions 5 and 9, for `global.add`

## Context

A source can be placed more than once in a scene. The Sources panel and
preview currently select by source id, while item layouts and many edit
methods belong to a placement. A source-id lookup can select or edit a
different placement from the one the user chose. Group Properties also needs
the complete set of item ids ([ADR-0184](0184-group-properties-batch-to-members.md)).
The owner approved independent selection and highlighting of placements, and
the explicit `global.add` conflict described below. The request contracts and
their effects are still proposed; neither this ADR nor the planned spec sections
authorize implementation.

On 2026-11-25, the owner decided that changing a shared capture region re-aspects
only the selected placement; the source pixels and region remain shared.

## Decision

1. The Sources panel, preview, Properties and their selection, menus, drag,
   grouping and attribute dialogs must carry item ids wherever the operation
   changes a placement. Each repeated placement is selected and highlighted
   separately. Properties must keep the distinction between source-owned
   controls (settings, filters, audio and playback) and item-owned controls
   (layout and other placement attributes): a source-owned change still has its
   existing shared-source effect.
2. The following requests gain an optional `item` selector. Existing fields
   and answers remain unchanged; `item` identifies the exact placement for each
   placement-scoped effect:

   | Method | Proposed request shape |
   | --- | --- |
   | `item.set_box` | `{source, item?, position, size, gesture?, seq?, canvas?}` |
   | `source.duplicate` | `{source, scene?, item?}` |
   | `source.copy_attributes` | `{source, scene?, canvas?, item?}` |
   | `source.reset` | `{source, scene?, canvas?, groups?, item?}` |
   | `look.apply` | `{source, look, item?}` |
   | `source.set_region` | `{source, region, item?}` |
   | `source.fit_chrome` | `{source, item?}` |

   `item` is the item's numeric id, not a source id. When present it must name
   an item placing `source`; if `scene` is also present, it must name that
   item's scene. A selected item in the canvas's global scene is addressed by
   its `item` id, with `scene` absent. An unknown item is `not_found`; a
   source/item or explicit-scene mismatch is `invalid_params`. A refusal never
   falls back to another placement or changes state.

   When `item` is absent, every method retains its existing resolution and
   defaults. In particular, legacy `Host::item_of` lookup selects the first
   matching item in the edited scene (preview in studio mode, otherwise
   program), then falls back to the canvas's global scene only when there is no
   edited-scene match. If both scenes contain that source, the edited-scene item
   wins. Explicit `scene` handling remains method-specific and unchanged;
   notably, existing methods that reject a `scene` for global sources continue
   to do so. `source.reset` may still reset source-only groups without an item.
   `canvas` continues to mean the existing `main`/`vertical` selector only on
   methods that already accept it; this ADR adds no new canvas field.
3. Keep the effects distinct:
   - Settings, filters, audio, playback, `region`, `chrome`, and other
     source-owned data remain shared by every placement of a source.
   - `look.apply` applies its optional style to the selected item, while its
     filters still change the shared source filter chain. `source.reset` and
     `source.copy_attributes` use the selected item for item attribute groups
     and the source for source attribute groups. `source.duplicate` copies the
     source and inserts the new item relative to the selected item.
   - `source.set_region` changes the shared source region. If picture size is
     known, the item-specific layout adjustment follows ADR-0178: keep that
     item's centre, area, rotation and flips and use the visible picture's
     aspect (region intersected with that item's crop). If picture size is
     unknown, save the region and do not change the item's box. This layout
     adjustment applies only to the selected placement, even though the source
     pixels and region remain shared. `source.reset` of the `region` group uses
     the same selected-placement adjustment. The layout adjustment is one part
     of the same undo step as the source-region edit. This follows the owner's
     decision of 2026-11-25; ADR-0178 predates repeated-placement targeting.
   - `source.paste_attributes` remains a multi-target operation addressed by
     its existing `items` list, not the single-item selector in this ADR. Under
     ADR-0133, item groups apply to each listed item while source groups apply
     once per source. The existing `region` group writes the shared region but
     does not re-aspect any item. Requests without a new opt-in must retain that
     behavior. Owner input is needed on these options: keep all paste requests
     as they are; add an opt-in to re-aspect every listed placement of each
     affected source; or add an opt-in naming one item from the list per
     affected source and re-aspect only that item. The opt-in request shape is
     also unresolved.
   - `source.fit_chrome` applies the stored `cover` or `fit` placement to the
     selected item's box, using the existing canvas and region-size rules; it
     does not change the shared region or chrome. `source.set_chrome` keeps its
     existing source-only behaviour and does not move an item.
   - `item.fit_text_box` remains source-wide: every placement keeps its centre
     and scale. `item.set_filled` keeps its shared source fill and
     item-specific border semantics.
4. `item.move {item, direction}` must move exactly the named item. It must not
   resolve the item's source and then move another placement. The existing
   `to_item` form and all other item-addressed operations, including their
   window callers, must preserve the chosen item id. Existing lock semantics
   remain: an item lock blocks pointer selection, move, resize and crop on the
   preview panel; Properties, control API, undo and layer-order edits may still
   change it. ADR-0184's
   explicit group move/resize lock rule remains specific to group operations.
5. Authorization and preflight must cover every effect, not just the request's
   first id. Keep the existing scope and effect classifications, including
   `source.reset`'s destructive permission. When an edit changes source-owned
   data, check the shared source's on-air effect; when it changes an item,
   check the selected item; a mixed operation checks both. In particular,
   `look.apply` checks the source filter effects and selected item style,
   `source.set_region` checks the shared region and its item-layout effect,
   and `global.add` checks the source and all placements it removes. The
   control thread validates the selector and affected state again immediately
   before mutation. All edit paths keep their existing serialized control
   thread; no new lock or check is added to a render or audio hot path.
6. Preserve operation atomicity and history behavior. A refused selector,
   permission check, semantic check or validation changes no collection,
   clipboard, history, gesture state or event. `source.set_region` records the
   source region and selected-item layout together as one undo step; `look.apply`
   remains one undo step across style and shared filters; `source.reset` and
   `source.duplicate` remain one undo step. `source.copy_attributes` remains a
   clipboard read/write only, with no scene undo. `item.set_box` keeps its
   existing gesture, sequence and per-call answer semantics; selector-aware
   calls in one gesture continue to target their exact item. Live effects stay
   outside undo as they do today.
7. For well-formed requests, apply the existing authorization gates before
   method-level selector lookup or mutation. Then resolve the named source and
   item, validate their relationship and any explicit scene, and validate
   method-specific type/range preconditions before recording an edit. Unknown
   source or item ids are `not_found`; an existing but mismatched source/item
   or item/scene pair is `invalid_params`; existing type/range errors keep
   their current kinds. On every refusal, the entire operation is unchanged.
   Malformed payloads (including unknown fields) remain `invalid_params` at
   request decoding. No error path may silently substitute the first placement
   when an explicit selector was supplied.
8. If `global.add` would remove more than one placement of its source from any
   one showable scene on that canvas, it must refuse with `conflict` before
   changing anything and explain that the extra placements in that scene must
   first be removed. One placement in each of several scenes retains the
   existing conversion. This refusal applies to old clients too, as the
   owner's explicit exception to the control API's major-version compatibility
   rule.
9. The UI must pass the selected `item` for placement-scoped actions, preserve
   it through preview/list selection, drag, context menus and Properties, and
   keep group Properties addressed by its scene, group and complete member
   item-id set. It must query `api.describe` before relying on the new fields.
   If the server does not support this contract, it must disable or refuse the
   placement-specific action with a clear reason; it must not retry the same
   action without `item`. Old requests without `item` keep their old server
   semantics.
10. Before implementation, update the control API, scene-model and UI specs
    with these exact shapes, selector/default-scene/global rules, shared-source
    and selected-item effects, authorization and lock semantics, error
    precedence, atomicity, undo/gesture behaviour, and old-server refusal.
    Record the owner's 2026-11-25 decision that shared region changes re-aspect
    only the selected placement while source pixels and region remain shared.
    Resolve how an opt-in region paste chooses which targeted placement boxes
    to re-aspect, without changing existing `source.paste_attributes` requests.
    Independently review these contracts before implementation. Use the next
    free protocol minor after 1.109 (currently 1.111, rechecked before
    implementation); do not advertise it until the full contract is
    implemented.

## Consequences

- Repeated placements can be edited without changing an unintended item.
- Existing source-only clients keep their targeting except for the expressly
  approved `global.add` refusal. Old servers cannot serve the new selection
  workflow without refusing unsafe placement actions.
- Tests must cover repeated placements, mismatched and stale selectors,
  precedence, permissions, locks, both canvases, undo and gestures, each
  affected operation, shared source effects, region/fitting effects, atomic
  global conversion and old-client/server behaviour.
- UI review and dark and light screenshots or an owner local test, followed by
  owner approval, remain required.

## Alternatives considered

- **Keep source-id selection.** Rejected: repeated placements are not
  distinguishable and may edit the wrong item.
- **Change source-wide settings per placement.** Rejected: these settings belong
  to the shared source and other placements already depend on them.
- **Choose one layout while making a multiply placed source global.** Rejected
  by the owner in favour of explicit refusal.

## Review

Independent review feedback received 2026-11-25: legacy `Host::item_of`
resolution and the ADR-0065 supersession references have been clarified.
Region-paste behavior remains an owner decision; a follow-up independent review
and owner approval are pending. ADR-0189 remains Proposed.
