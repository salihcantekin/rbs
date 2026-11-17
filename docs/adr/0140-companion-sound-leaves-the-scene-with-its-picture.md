# ADR-0140: A companion sound leaves a scene with its picture

- Status: Proposed
- Date: 2026-11-17
- Deciders: Salih (owner)
- Spec: [scene-model](../specs/scene-model.md), [control-api](../specs/control-api.md)
- Supersedes in part: [ADR-0096](0096-capture-audio-companions-and-camera-microphone.md) decision 2 (what Remove from scene does to companions); builds on [ADR-0065](0065-global-sources-and-safe-source-removal.md) decision 7 (Remove from scene) and [ADR-0133](0133-duplicate-reset-and-paste-source-attributes.md) decision 4 (Duplicate scene)

## Context

A picture source with "Capture audio too" has a companion sound source (ADR-0096). Adding the companion places an item of it in the picture's scene, and Duplicate scene (ADR-0133) copies both items, so a picture placed in several scenes usually has a companion item next to it in each scene.

Remove from scene (ADR-0065 decision 7, `SceneCollection::remove_item`, host `remove_from_scene`, `item.remove`) removes only the item that was picked. Two results follow, both found while checking scene duplication on 2026-11-17:

1. **A silent row is left behind.** The picture is removed from scene A but is still placed in scene B. The companion's item stays in A, and because a companion is heard only while its picture is drawn in the same result (`visible_sources`, ADR-0096 decision 2), A now lists a sound source that makes no sound.
2. **Another scene changes.** When the removed item was the picture's last placement, the picture is deleted, and ADR-0096 deletes its companions with it. A companion item that is still in another scene disappears from that scene, which the user did not touch.

The owner chose on 2026-11-17 that the companion sound leaves a scene together with its picture, and that the sound in the other scenes stays.

## Decision

1. **Remove from scene takes the companions' items in that scene.** When Remove from scene removes an item of a picture source, and that scene has no other item of the same picture, the items in the same scene of every companion of that picture (`companion_of` equal to the picture) must be removed in the same undo record. Items of the companions in other scenes must not change. When the scene still has another item of the picture, only the picked item is removed.
2. **One rule in the model.** The rule must live in `rbs-scene` (`SceneCollection`), so the host, the UI and the control API all get it from the same call. The host must not reimplement it. One undo record restores every removed item at its old index.
3. **The last placement.** When the picked item was the picture's last placement in any scene (global scenes included), the picture is deleted as before. By decision 1 its companions' items in that scene are already gone. Its companions are deleted with it (ADR-0096 decision 2, unchanged), and that now only affects companion items the user placed on their own in a scene without the picture, where the sound was already silent.
4. **What stays as it is.** Delete source (`source.delete`) keeps its meaning: the picture and its companions go from every scene. Removing a companion's item on its own removes only that item. Hide and show do not change items. Duplicate source and Duplicate scene are unchanged.
5. **Control API.** `item.remove` keeps its parameters and its answer `{source_deleted}`; the extra items it removes are reported by the existing `scene.changed` and `source.changed` events. Because the meaning changes, the change takes the next free protocol minor at merge (main + 1) and `control-api.md` says what it removes. A client of an older minor only sees fewer rows after the call.
6. **No scene format change.** Items and the `companion_of` hint are stored as before.
7. **Zero cost (P13).** The extra work happens only inside the remove call: a scan of one scene's items and the sources' hints. Nothing is added to the render loop, the audio thread or `Node::process`; no hot path is touched.

## Consequences

- The Sources list of a scene never keeps a silent companion row after its picture is removed, and removing a picture from one scene never changes another scene's items.
- Undo restores the picture and its companion items in one step, as the user saw them.
- The confirm question "Remove X from this scene?" does not name the sound. The row's result and undo make it visible. Naming the sound in the question would be a visible text change that needs the owner's approval (rule 16), so it is left for a later UI proposal.
- Follow-up work in the implementation PR: `SceneCollection::remove_item` (or a new `remove_from_scene` in the model) with property and unit tests in `rbs-scene` (picture in two scenes, picture placed twice in one scene, last placement, companion placed alone, global scene, undo and redo); host tests through `item.remove` and the Sources list command; `scene-model.md` and `control-api.md` updated; protocol minor main + 1.

## Alternatives considered

- **Keep ADR-0096 as it is.** Leaves the silent row and the change to other scenes that the owner rejected.
- **Do it in the host only.** The control API and the UI already share `remove_from_scene` in the host, but the model is where the undo record and the invariants are checked. A model rule keeps every caller, including future ones, consistent.
- **Turn a companion that is still placed elsewhere into a normal sound source at the picture's deletion.** It would start making sound when its picture is deleted, which is surprising. With decision 1 the case only remains for companions placed alone, which were silent already.
- **Remove the companion's items from every scene.** It changes scenes the user did not touch, the problem this ADR fixes.

## Review

Independent review per ADR-0014. Record: reviewer, date, verdict, main concerns and how each was resolved.
