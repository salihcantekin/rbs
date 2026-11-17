# ADR-0142: A companion sound leaves a scene with its picture

- Status: Accepted
- Date: 2026-11-17
- Deciders: Salih (owner)
- Spec: [scene-model](../specs/scene-model.md), [control-api](../specs/control-api.md)
- Supersedes in part: [ADR-0065](0065-global-sources-and-safe-source-removal.md) decision 7 (Remove from scene removes one item only). Builds on [ADR-0096](0096-capture-audio-companions-and-camera-microphone.md) decision 2 (a companion is heard only while its picture is drawn; deleting the picture deletes its companions, unchanged) and [ADR-0133](0133-duplicate-reset-and-paste-source-attributes.md) decision 4 (Duplicate scene)

## Context

A picture source with "Capture audio too" has a companion sound source (ADR-0096). Adding the companion places an item of it in the picture's scene, and Duplicate scene (ADR-0133) copies both items, so a picture placed in several scenes usually has a companion item next to it in each scene.

Remove from scene (ADR-0065 decision 7, `SceneCollection::remove_item`, host `remove_from_scene`, `item.remove`) removes only the item that was picked. Two results follow, both found while checking scene duplication on 2026-11-17:

1. **A silent row is left behind.** The picture is removed from scene A but is still placed in scene B. The companion's item stays in A, and because a companion is heard only while its picture is drawn in the same result (`visible_sources`, ADR-0096 decision 2), A now lists a sound source that makes no sound.
2. **Another scene changes.** When the removed item was the picture's last placement, the picture is deleted, and ADR-0096 deletes its companions with it. A companion item that is still in another scene disappears from that scene, which the user did not touch.

The owner chose on 2026-11-17 that the companion sound leaves a scene together with its picture, and that the sound in the other scenes stays.

## Decision

Words used here: a scene **draws** a picture when the picture has an item in that scene, or in a scene nested in it at any depth. Every item counts, hidden or in standby, and a nested scene counts whatever its own item's state, so the rule never depends on visibility. A companion is a source whose `companion_of` names the picture.

1. **Remove from scene takes the companions' items in that scene.** When Remove from scene removes an item of a picture source from scene S, and S no longer draws that picture afterwards, every item placed directly in S of every companion of that picture must be removed in the same undo record. When S still draws the picture (another item of it, directly or through a nested scene), only the picked item is removed.
2. **Other scenes do not change.** Items in any other scene must not change, including scenes that nest S. Known limit: when the picture is removed from a nested scene N and its companion sits in a scene that nests N, that companion row stays and is silent. The user removes it by hand. The same holds downwards: a companion item in a scene nested in S stays when the picture leaves S, and is silent unless something else draws the picture there. Following nesting either way would change scenes the user did not open.
3. **A companion left placed nowhere is deleted.** When decision 1 removes a companion's last item in any scene (global scenes included), that companion is deleted in the same undo record, with its filters and fader, as ADR-0065 decision 7 already does for any source. Undo restores it exactly. The picture then has no companion, and "Capture audio too" shows off.
4. **The picture's last placement.** When the picked item was the picture's last placement in any scene (global scenes included), the picture is deleted as before, and ADR-0096 decision 2 deletes its companions with every item they still have. Once the picture is gone those companions would be silent everywhere, so nothing that stays on air loses its sound.
5. **The global scene.** Removing a global picture (`global.remove` without `keep`) is Remove from scene on the canvas's global scene and follows decisions 1 to 4: companion items in the global scene go with it. Today `global.add` moves only the picture into the global scene and leaves its companions' items in the scenes. Changing that is out of scope here and is listed as follow-up work. "Draws" does not count the global scene: ADR-0065 refuses a global source in the scenes of its canvas, so a picture can be global and also in S only when S is shown on another canvas. Then removing it from S takes S's companion items although the global placement still makes them heard on the other canvas; this is accepted with the follow-up below. Second known limit, until that follow-up: when a global picture is removed but is still placed elsewhere (in a scene only another canvas shows), its companion rows in the ordinary scenes stay and go silent.
6. **One method in the model.** A new `SceneCollection` method (for example `remove_from_scene(item)`) must own decisions 1 to 4, including the last-placement rule that the host's `remove_from_scene` decides today. It returns whether the picked source was deleted, every source deleted, every item removed in any scene (the picked one, companion items in S, and, under decision 4, every item of the deleted companions), and one undo record. The model's `Undo` stays a plain operation with no label. The host labels the history entry from the result: the picture's name, Delete when the picture was deleted, else Remove (also when only a companion is deleted under decision 3). The host, the UI and the control API must call it and must not reimplement any part of it. `SceneCollection::remove_item` stays as it is, because `global.add`, `global.remove` with `keep` and rollbacks use it to move single items.
7. **Control API.** `item.remove` keeps its parameters. Its answer adds `removed_items: [item]`, every item removed in any scene including the picked one, and `deleted_sources: [source]`, every source deleted (the picture and companions); both are additive fields. This changes what the call removes, so it takes the next free protocol minor at merge (main + 1), and `control-api.md` states the new meaning under that minor. A client of an older minor still gets correct `scene.changed` and `source.changed` events.
8. **On-air check.** The extra items are in the same scene as the picked item, so they are on air exactly when it is, and the existing `item.remove` check covers them. In decision 4, deleting the companions can remove companion items in other scenes that are on air. That is the behaviour of today's last-placement delete, and this ADR does not change it.
9. **Own levels.** A scene's own level entry for a removed companion is kept, as for any removed item (ADR-0091). A companion deleted under decisions 3 or 4 loses its entries with it (`Op::RemoveSource`), and undo puts them back. Undo restores the scene exactly.
10. **What stays as it is.** Delete source (`source.delete`) keeps its meaning. Removing a companion's item on its own removes only that item. Hide and show, Duplicate source and Duplicate scene are unchanged. No scene format change.
11. **Zero cost (P13).** The extra work happens only inside the remove call: a walk over one scene, the scenes nested in it and the sources' hints. Nothing is added to the render loop, the audio thread or `Node::process`, and no hot path is touched.

## Consequences

- After its picture is removed, a scene's Sources list no longer keeps a silent companion row, apart from the known limits (nested scenes upwards and downwards in decision 2, and the global cases in decision 5). Removing a picture from one scene never changes another scene's items, except through the last-placement delete, which is unchanged.
- Undo restores the picture and its companion items in one step, as the user saw them.
- A companion placed only next to the removed picture is deleted with it (decision 3). Its fader and filters come back only through undo.
- The confirm question "Remove X from this scene?" does not name the sound. The row's result and undo make it visible. Naming the sound in the question would be a visible text change that needs the owner's approval (rule 16), so it is left for a later UI proposal.
- Follow-up outside this ADR: `global.add` should move a picture's companions into the global scene with it.
- The implementation PR must contain:
  - The new model method, with unit and property tests in `rbs-scene` for these cases:
    - a picture in two scenes;
    - a picture placed twice in one scene;
    - a picture in a scene nested in S, and a picture directly in S that is also in a nested scene;
    - a companion in a scene that nests the picture's scene, and one in a scene nested in it (the known limits);
    - a global picture, also one still placed elsewhere;
    - a hidden picture item in a nested scene ("draws" ignores visibility);
    - two companions, and two items of one companion in S;
    - the companion's last placement (decision 3);
    - the picture's last placement;
    - Own levels;
    - undo, then redo, then undo, restoring every index;
    - `remove_item`, `global.add` and `global.remove` with `keep` unchanged.
  - Host tests through `item.remove` (`removed_items`, `deleted_sources`, and the history label Remove with the picture's name when only a companion is deleted) and through the Sources list command.
  - `scene-model.md` and `control-api.md` updated, and the protocol minor set to main + 1.

## Alternatives considered

- **Keep today's behaviour.** It leaves the silent row and the change to other scenes, which the owner rejected.
- **Do it in the host only.** The model is where the undo record and the invariants are checked. A model method keeps every caller, including future ones, consistent.
- **Change `remove_item`.** It would drop companion items while `global.add` or `global.remove` with `keep` moves a picture.
- **Use the picture's whole result, nesting upwards too.** It would change scenes that nest S, which the user did not touch.
- **Keep a companion that is placed nowhere.** It would be a source the user cannot see in any Sources list. ADR-0065 decision 7 deletes such sources.
- **Turn a still-placed companion into a normal sound source when its picture is deleted.** It would start making sound when its picture is deleted, which would surprise the user.

## Review

Independent review per ADR-0014. Record: reviewer, date, verdict, main concerns and how each was resolved.

Round 1, independent AI session (2026-11-17): changes requested, 9 concerns. 1: "same scene" did not match the audibility rule for nested scenes; now "draws", with the known limit stated (decisions 1, 2). 2: global scene; now decision 5, with `global.add` moving companions as follow-up, and decision 4 reworded. 3: a companion left placed nowhere; now deleted (decision 3). 4: changing `remove_item` would break moves; now a new model method that also owns the last-placement rule (decision 6). 5: the answer changes meaning; now `removed_items` and a new minor (decision 7). 6: on-air check; now stated (decision 8). 7: Own levels; now stated (decision 9). 8: header now supersedes ADR-0065 decision 7. 9: test list extended.

Round 2, independent AI session (2026-11-17): changes requested (text only; design holds; all round 1 resolutions checked against the code). 1: global picture still placed elsewhere leaves silent rows; now the second known limit in decision 5, Consequences fixed. 2: `removed_items` now lists items in any scene. 3: `deleted_sources` added. 4: "draws" counts every item whatever its visibility, with a test. 5: deleted companions' level entries and the undo label stated (decisions 6, 9).

Round 3, independent AI session (2026-11-17): changes requested (text only; the design holds; round 2 resolutions confirmed in the text and against the code). 1: a companion in a scene nested in S; now named in decision 2 with a test. 2: undo label when only a companion is deleted; now Remove with the picture's name (decision 6), tested. 3: a picture also global on another canvas; now stated in decision 5.

Round 4, independent AI session (2026-11-17): changes requested, 1 concern: the undo label belongs to the host, not the model's `Undo`. Resolved in decision 6; the label test moved to the host tests. Round 3 resolutions confirmed in the text and against the code.

Round 5, independent AI session (2026-11-17): approve. The round-4 resolution matches the code (`Undo(Op)` has no label; the host labels the history entry), and no decision contradicts another.

Owner approval: Salih, 2026-11-17 ("kabul").
