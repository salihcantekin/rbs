# ADR-0059: Scenes, item lock and source volume in the studio

- Status: Proposed
- Date: 2026-11-07
- Deciders: Salih (owner)
- Builds on: ADR-0021 (scene collection model), ADR-0037 (audio graph), ADR-0039 (scene source types), ADR-0044 (control API), ADR-0048 (preview handles), ADR-0051 (user data), ADR-0052 (API parity), ADR-0013 (zero cost when unused)

## Context

The main window design (ADR-0049, mockup E) has a Scenes panel with a `+` button and one row per scene, a lock button and a show/hide button on each Sources row, and a mute button and a volume fader per Mixer row. An audit of the engine on 2026-11-07 found:

- **Scenes.** The scene model ([scene-model.md](../specs/scene-model.md)) has any number of scenes, a program scene per canvas and the edits `add_scene`, `remove_scene`, `rename_scene` and `set_program`. The desktop app does not use them: its studio host picks one scene when it starts (`shown_scene`) and every command edits that scene. Nothing adds, removes, renames or switches scenes, and the control API has no `scene.*` method.
- **Lock.** The model has no lock.
- **Visibility.** The model has `SceneItem::visible` and `set_item_visible`; the binder already leaves hidden items out and moves their sources to Standby. The app gains a Show/Hide button in PR #166; the control API has no setter.
- **Volume and mute.** The mixer (`rbs_audio::MixerNode`, ADR-0037) already has per-strip gain and mute with ramps. `rbs_record::Studio` sends every strip at gain 1, the collection stores nothing, and neither the app nor the API can change it.
- **Rename.** The model has `rename_source`; the app and the API have no way to rename a source.

Other gaps (transitions other than a cut, a studio mode UI, scene thumbnails, scene order, hotkeys, audio monitoring, groups) are out of scope here.

## Decision

1. **Lock is an item flag.** `SceneItem` gains `locked: bool` (default `false`), and `SceneCollection::set_item_locked(item, locked) -> Undo` changes it. Lock means what it means in other broadcast tools: the item cannot be selected, moved, resized or cropped with the pointer on the picture panel (preview handles, ADR-0048). Properties, the control API, undo and layer order still change a locked item. Lock is saved with the collection (`locked = true` on the item in the document, absent when `false`) and has no effect on flattening, binding or the engine. Like visibility, it is per item, not per canvas.
2. **Volume and mute belong to the source.** `Source` gains `audio: SourceAudio { volume: f32, muted: bool }`. `volume` is a linear amplitude factor in `0.0..=1.0` (0 dB at most, like a fader; a boost is a later audio filter), checked by the invariants (`InvalidVolume` for a value outside the range or not finite). `SceneCollection::set_source_volume` and `set_source_muted` return an `Undo` like any edit, but with a record of their own (`SetSourceAudio`, which expects and replaces only `audio`). Every other source record (rename, settings, filters, `ReplaceSource`) ignores `audio` when it checks the state it expects and keeps the current `audio` when it applies, so a fader move or mute between an edit and its undo neither makes the undo conflict nor brings the old volume back. A source runs as one instance wherever it is placed, so it has one volume, not one per scene. The binder ignores `audio`: a volume change never makes a new source node. The document stores `volume` and `muted = true` on the source when they are not the defaults, and `locked = true` on items; the scene collection document becomes format 3 (format 2 bodies load unchanged), because the document refuses unknown keys and a build that knows format 2 must refuse a format 3 file without touching it, as ADR-0053 did for `style`.
3. **The studio sends them as mixer strips.** `rbs_record::Studio` builds each mixer strip from its source's `audio` (`Strip::new(producer, volume).with_muted(muted)`) on every `update`. A change of volume or mute alone takes a light path, `Studio::set_mix(collection)`: it rebuilds the mixer parameters from the strips the studio already holds and sends them through the existing parameter channel, without binding or applying the graph, so a fader drag costs one small message per change on the control thread and the mixer's ramp (10 ms) keeps it free of clicks. The UI sends at most one volume change per frame of its own event loop while dragging. A source with both video and audio outputs is still not mixed (`AudioOnVideoSource`, scene model spec); its volume is saved and applies once it is.
4. **The app shows and edits the program scene.** The studio host keeps one scene "in the Sources panel": always the canvas's program scene (no studio mode yet). New host commands, each with an `Update` the UI shows:
   - **Undo and redo never change the program.** The scene records keep which canvases showed a removed scene, so a redo of an add, or an undo of a remove, could put a scene back on air. The host remembers the program before applying an undo or redo record and sets it back afterwards (outside the undo stack) when the record changed it and that scene still exists.
   - `AddScene` (named "Scene", "Scene 2", ... unless a name is given, shared scope, empty), `RemoveScene`, `RenameScene` and `RenameSource` are edits on the user's undo stack (U4), with `EditKind`s of their own.
   - `SwitchScene` sets the program scene. Like `set_program` in the model (ADR-0021), it is a live action and is not on the undo stack.
   - `SetSourceVisible`, `SetSourceLocked` are item edits on the undo stack (`SetSourceVisible` is the command PR #166 adds; this ADR keeps it).
   - `SetSourceVolume` and `SetSourceMuted` are live actions and not on the undo stack: undoing a layout change must never unmute a microphone. A volume drag sends many changes; the host saves the collection once at the end like any other change.
   - **There is always a program scene on the studio's canvas.** The rules are per canvas and respect scope (U15): removing a scene is refused when the studio's canvas may show no other scene. Removing the scene on air first switches the program, outside the undo stack and before the studio updates (`Studio::update` refuses a canvas without a program), to the scene before it among those the canvas may show (else the one after it); the removal is then the undo step. Other canvases of the collection that showed the removed scene lose it as the model says (the app runs one canvas). If an undo or redo leaves the canvas without a program (undoing the add of the scene on air), the host switches to the first scene the canvas may show, outside the undo stack, so the program never goes black because of an undo.
   - The undo history is kept across scene switches (the records name objects by id, not by "the current scene").
5. **Control API 1.6.** New methods, all following the existing rules (each edit publishes its topic, refusals as below):
   - `scene.list` (`read`): `{scenes: [{id, name}], program}`, the scenes the studio's canvas may show, in the order the Scenes panel shows them (for now creation order; reordering is a later minor version).
   - `scene.add {name?}`, `scene.remove {scene}`, `scene.rename {scene, name}` (`control`, one undo step each), `scene.switch {scene}` (`control`, live). `not_found` for an unknown scene, `invalid_params` for an empty name, `conflict` for a taken name (as `tokens.create`) and for removing the last scene the canvas may show; switching to the program scene changes nothing. `scene.remove` of the scene on air switches first (decision 4); that switch is not undone by `edit.undo`.
   - `item.set_visible {item, visible}`, `item.set_locked {item, locked}` (`control`, one undo step each).
   - `source.rename {source, name}` (`control`, one undo step).
   - `source.set_volume {source, volume}` and `source.set_muted {source, muted}` (`control`, live, not undoable): `invalid_params` for a volume outside `0..=1` or not finite.
   - `source.list` rows and `source.changed` stay one row per source item of the program scene (a source placed twice has two rows with their own `item`), and gain that item's `visible` and `locked` and the source's `volume` and `muted`. A new topic `scene.changed` carries the `scene.list` answer whenever a scene is added, removed or renamed, or the program changes.
   - Effects (ADR-0052 decision 4, when effects join the schema): `scene.list` `read`; the undoable ones `edit`; `scene.switch`, `source.set_volume` and `source.set_muted` `edit` with the on-air check (they change what is on air, but nothing is lost and another call reverses them); `scene.add`, `scene.remove`, `scene.rename` and the item edits `edit`, with the on-air check when they touch the program scene (`scene.remove` of the scene on air always does). `item.set_locked` is not enforced on the API side: the API may move a locked item, as Properties may.
6. **Zero cost.** Nothing runs per frame or per audio block that did not before: the flags are read on the control thread when the collection changes, and the mixer already applies gains per strip. A hidden or switched-away source goes Standby as before (binder). A scene that is not on air costs nothing (no thumbnails here: rendering scenes that are not on air needs its own decision, because it makes their sources Active).

## Consequences

- `SceneItem` and `Source` gain public fields; both are `#[non_exhaustive]` and built through constructors, so callers outside `rbs-scene` keep compiling. `SceneError` (non-exhaustive) gains `InvalidVolume`. Scene documents written by older versions load unchanged (the new keys are optional); older versions reading a newer document ignore unknown keys only if the reader does: the user data spec says what happens.
- The app and API can now remove the scene on air, so the host must never let the program become empty (decision 4); tests in `rbs-app` cover removing the program scene, removing the last scene and undoing the add of the scene on air.
- Spec updates in the same change: scene-model (lock, source audio, invariants, edits), user-data (document keys), control-api (1.6), ui-design-system (Scenes panel, lock and mute behaviour), audio-graph if needed.
- UI wiring of the Scenes panel, the lock button, the mute button and the fader follows in the app's Slint files after PR #166 (which owns those files now) is merged.
- Follow-ups: scene order and duplicate; scene thumbnails; studio mode (preview scene) with a Transition button; per-canvas visibility (scene model open question).

## Alternatives considered

- **Volume per scene item.** OBS and the mixer both treat volume as a property of the source, which runs once however many scenes place it; per-item volume would need per-scene mixer strips for one input.
- **Volume and mute as source settings (`Settings` keys).** No model change, but every change would make the binder replace the source node (settings change, ADR-0039), which reopens the device.
- **Mute on the undo stack.** Matches "every edit undoable", but an undo of the last move could then unmute a microphone on air; mute and volume are live controls like the program switch.
- **Mute on the undo stack as a record that undo skips.** Keeps one stack, but the history would then hold steps the user never sees, a skipped record would still sit between its neighbours' expected states (the conflict of decision 2), and redo order would become hard to explain. A record of its own that other records ignore is simpler.
- **Lock enforced by the model (refuse layout edits of locked items).** Would also block Properties and the API, which in other tools still move locked items, and every caller would need an unlock-edit-lock dance.

## Review

Independent review per ADR-0014.

- Reviewer: independent AI session (given only the ADR, principles, specs and code). Date: 2026-11-07. Verdict: changes requested; all resolved in this revision.
- Concerns and resolutions:
  1. Live volume and mute would make earlier source records conflict, or come back on undo, because `ReplaceSource` compares the whole source: audio has its own record and other source records ignore and keep it (decision 2); test `volume_and_mute_never_conflict_with_or_come_back_through_other_source_records`.
  2. Undo and redo could still change the program through the scene records' saved program: the host restores the program after every undo and redo (decision 4); tested in `rbs-app`.
  3. Compatibility claim wrong (the document refuses unknown keys): format 3 with an identity upgrade from 2 (decision 2).
  4. Program-scene rules underspecified for scope and several canvases: stated per canvas with scope, switching before the studio updates (decision 4).
  5. A fader drag through `Studio::update` binds and applies the graph each time: light path `Studio::set_mix` and one change per UI frame (decision 3).
  6. API wording: empty name `invalid_params`, taken name `conflict`; scene order is panel order, not promised as id order; rows are per item (decision 5).
  7. ADR-0052 effects incomplete for `scene.remove` on air: stated (decision 5).
  8. Missing alternative (mute as a record undo skips): added.
- Optional suggestions taken: `EditKind`s and step names for scene edits; fader law in the UI spec; `item.set_locked` not enforced by the API, noted for the parity test; the 0..=1 limit lives in the model only (the mixer's `sanitize` accepts more).
- Owner: pending.
