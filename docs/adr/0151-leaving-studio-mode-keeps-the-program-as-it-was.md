# ADR-0151: Leaving studio mode keeps the program as it was on air

- Status: Accepted
- Date: 2026-11-18
- Deciders: Salih (owner)
- Supersedes in part: ADR-0063 decision 4 (its sentence on leaving studio mode), ADR-0098 decision 2 (a hold that ends because studio mode ends)
- Builds on: ADR-0061 (studio mode), ADR-0063 (hold the program while its scene is edited in preview), ADR-0098 (a hold lasts while its scene is on air), ADR-0059 (undo of scene edits)

## Context

The owner reported (2026-11-18): in studio mode a change made in Preview reaches Program only with a transition, as intended, but when studio mode is turned off without a transition the change is applied to Program anyway. It must not be: Program must stay as it was.

This is what ADR-0063 decision 4 decided: "Leaving studio mode ends the hold, so the program then shows the scene as it is, as it would after a transition." The edits are made to the real scene; the program shows a held copy (ADR-0063 decision 2). When the hold ends, the program shows the real scene, edits included. The owner now wants the opposite.

Only the held case is affected. When the preview scene is another scene than the program scene and nothing of the program scene is held, its edits are not in the program, and leaving studio mode keeps the program scene on air as before.

## Decision

1. **Leaving studio mode puts the held program scene back as it is on air.** When studio mode ends (the toolbar or menu toggle, `canvas.set_preview {scene: null}`) while the program scene is held, the host replaces the real scene's items with the held copy's items, with their original ids (the copy's item id minus the copy's base id, ADR-0063 decision 2), before the hold ends. Program shows the same picture before and after; the edits made in Preview since the hold began are dropped from the scene. A transition before leaving puts them on air as before (ADR-0063 decision 3), and then nothing is dropped.
2. **What is put back.** Only what is held: the scene's items, in order, with their layouts, per-canvas overrides, visibility, show-for time and lock. As in ADR-0063 decision 4, source settings, filters, volume, mute and scene levels are not held, so they are not put back. An item whose source or nested scene was deleted while held has already left the copy (ADR-0063 decision 2) and does not come back.
3. **One undo step.** Putting the scene back is one undo step (`EditKind::DiscardPreview`, the scene's name), so Undo (the button, Ctrl/Cmd+Z or `edit.undo`) brings the dropped edits back. Outside studio mode that puts them on air, which is then the user's explicit action. The step also keeps the history consistent: the steps recorded while editing in Preview undo against the scene as it was edited, which they find again after this step is undone. When the scene equals the copy (nothing was edited, or every edit was undone) nothing is recorded.
4. **A primitive for it in the scene model.** `SceneCollection::set_scene_items(scene, items)` replaces a scene's items as one invertible record (`Op::SetSceneItems`). An item that is not in the scene now keeps its id only when the collection issued that id and no other item uses it (the rule undo already uses to restore an item), and no id may appear twice; otherwise the edit is refused with `UndoConflict` and changes nothing. An item that stays in the scene keeps its content (as `ReplaceItem` requires). The collection's invariants are checked as for every edit. The record expects the scene's whole item list, so it undoes only in order, like every record (last in, first out). If the host's put-back is refused (the copy no longer fits, which ADR-0063 decision 2 already guards against), studio mode still ends and the scene keeps its edits, as before this ADR.
5. **Order.** The host ends studio mode first (`set_preview(None)`) and puts the scene back only when that succeeded, so a refused leave changes nothing.
6. **Zero cost.** Nothing runs outside the moment studio mode ends with a hold (P13). No new thread, timer or per-frame work.

7. **Protocol 1.92, a stated exception.** The control API spec counts a change of meaning as major. Like ADR-0098 (1.31), this ADR asks the owner to accept a minor version: the method, its parameters and its answer are unchanged, a client sees the result in `scene.changed` and `source.changed` as for any edit, and a major version would cost every client a migration for one outcome of one live action. The exception is listed in the spec's versioning paragraph. `canvas.set_preview` stays `live`: what is on air does not change when the mode ends; the put-back only makes the scene match it.

## Consequences

- Studio mode never changes the program without a transition, also when it ends.
- Out of scope: the preview scene's edits are saved with the document while a hold runs (the hold lives only in the run, ADR-0063 decision 2). A scene collection switch or quitting the app while held saves the edited scene, and reopening the collection shows the edits on air. Putting the scene back on those paths too is a possible follow-up.
- A user who meant to keep the edits and forgot the transition gets them back with one Undo; the control API documents this.
- Control API: protocol 1.92 (decision 7). Specs: control API (versioning exception, method table, Studio mode section, undo section, the 1.92 section), scene model (`set_scene_items`).
- Tests: rbs-scene contract tests for `set_scene_items` (round trip through undo and redo, refused ids and content); an rbs-app API test that leaving without a transition keeps the program, that one `edit.undo` brings the edits back and the steps made in Preview then undo without a conflict, and that leaving after a transition keeps the edits.

## Alternatives considered

- **Keep the edits in the scene and keep showing the copy after studio mode ends.** Outside studio mode the program is the scene itself (ADR-0061); a copy on air with no way to transition it would be a hidden second state.
- **Ask on leaving** ("Put the Preview changes on air?"). A question on every toggle slows a live operator down; the owner asked that leaving simply keep the program. Undo covers the rare mistake.
- **Not undoable.** Simpler, but the edits would be lost for good, and the undo steps made in Preview would then conflict with the scene put back.
- **Several primitive records (remove, insert, replace, move per item) in one step.** No new scene-model record, but restoring an item with its own id, its overrides and its lock needs the same internal insert anyway, and the order of the records is easy to get wrong.

## Review

Independent review per ADR-0014, 2026-11-18, independent AI session given the ADR, the ADRs it builds on, the specs and the diff: approve with changes, 8 findings, all resolved in this revision.

1. Blocking: a minor version for a change of meaning needs a stated exception: decision 7, and the control API versioning paragraph.
2. Blocking: the scene model spec did not list `set_scene_items`: added with its rules.
3. Stale control API text (method table, undo section, effect class): updated; the effect stays `live` (decision 7).
4. A refused `set_preview` after the put-back left the edits dropped with studio mode still on: decision 5, the host now leaves first.
5. A kept id could change content: refused with `UndoConflict`, with a test.
6. Saving while held keeps the edits in the document: stated as out of scope in Consequences.
7. Decision 3 was untested: the app test now undoes the Preview steps after the put-back.
8. Wording on whole-list expectation: decision 4.

Owner acceptance, 2026-11-18: Salih approved the merge of PR #556 ("merge et"), which accepts this ADR, including the minor-version exception of decision 7.

Follow-up, 2026-11-18 (owner report after the merge): entering or leaving studio mode played the collection's transition with the same picture (a Slide brought the same window in again), because the studio saw the program change between the scene and its held copy, two scene ids. This was not new with this ADR; it is a fix of ADR-0063's implementation, with no contract change. The host now runs such an update with `Studio::update_same_picture` (recording spec, transitions): a program change only between a scene and its held copy starts no transition, no glide and no `transition.state`. A transition of a scene to itself (one copy to the next, ADR-0063 decision 3) still runs.
