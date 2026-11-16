# ADR-0133: Duplicate sources, reset them, and paste chosen attributes onto other sources

- Status: Proposed
- Date: 2026-11-16
- Deciders: Salih (owner)
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0053](0053-item-style-and-video-filter-chains.md), [ADR-0059](0059-scenes-lock-and-source-audio-in-the-studio.md), [ADR-0062](0062-built-in-video-filters-and-properties-tabs.md), [ADR-0065](0065-global-sources-and-safe-source-removal.md), [ADR-0077](0077-source-health-retarget-and-settings-window.md), [ADR-0082](0082-audio-filter-chains-on-sources.md), [ADR-0084](0084-source-transform-commands-and-source-context-menu.md), [ADR-0091](0091-per-scene-audio-levels.md), [ADR-0095](0095-audio-ducker-and-key-tap.md), [ADR-0096](0096-capture-audio-companions-and-camera-microphone.md), [ADR-0103](0103-source-region.md), [ADR-0104](0104-display-chrome-insets.md), [ADR-0126](0126-measured-sync-offset-and-device-memory.md)
- Spec: [scene-model](../specs/scene-model.md) (new "Copies, resets and attribute groups" section), [scene-sources](../specs/scene-sources.md) (identity and option keys per source class), [control-api](../specs/control-api.md) (`source.duplicate`, `scene.duplicate {sources}`, `source.reset`, `source.copy_attributes`, `source.attributes_clipboard`, `source.paste_attributes`), [ui-design-system](../specs/ui-design-system.md) (Source context menu, Paste attributes and Reset source dialogs), [shortcuts](../specs/shortcuts.md), [glossary](../ui/glossary.md)

## Context

The owner asked for three things (project thread, 2026-11-16):

1. Duplicate a source and duplicate a scene, if RBS cannot do it yet.
2. Put the settings of a source (for example a copy) back to their defaults.
3. Copy chosen settings of one source onto others, as DaVinci Resolve's Paste Attributes does.

What exists on main (7589237):

- **Duplicate scene exists** (control API `scene.duplicate`, protocol 1.43; registry `scene.duplicate`). `SceneCollection::duplicate_scene` copies a scene with new scene and item ids, the **same sources** (references), the same nested scenes and layouts, and the scene's own levels. One undo step; it does not go on air. Editing a source in the copy changes it in the original too, because sources are shared by every scene that places them (glossary: Source).
- **Placing a source again exists** ("Existing source", `item.add {source}`): a reference too.
- **Duplicate source does not exist.**
- **Reset exists only per part:** `item.reset_transform` (ADR-0084), a filter's "Reset to defaults" button, `source.reset_chrome` (sets the insets from the display's work area).
- **Copy and paste of settings does not exist.** The nearest is `look.apply`: a built-in Look copied onto a source, composed by the host from ordinary edits (`scene-model.md`, Looks), checked first, one undo step.
- The Sources panel has one selected source; there is no multi-selection.
- Volume, mute, monitor and sync offset are **live controls, not undo steps** (ADR-0059, ADR-0062 decision 10, `scene-model.md`): their record expects and replaces only `audio`, the host throws its `Undo` away, and every other record ignores `audio`.
- Source types have no settings schema: `rbs-sources` has only `known_keys(type_id)` per class. `Values::defaults` exists for filters only. `rbs-scene` does not depend on `rbs-sources`.

OBS for comparison: duplicating a scene copies items as references; a source can be pasted as a reference or a duplicate; filters can be copied from one source to one other; a transform can be copied to one item. There is no reset of a whole source, no choice of what to copy, and no paste onto several sources at once. A webcam duplicated in OBS on Windows opens the device twice and the second one usually stays black.

## Decision

### 1. Attribute groups

A source and its item are split into named **attribute groups**, the one vocabulary for reset (decision 5) and paste (decision 6) in the engine, the control API and the UI.

| Group id | UI name | Lives on | Contents | Default | Applies to | Undo |
| --- | --- | --- | --- | --- | --- | --- |
| `transform` | Position and size | item layout | `position`, `size`, `rotation` | as `item.reset_transform` (ADR-0084: rotation 0, no flips, the box at the picture's own size after crop, centre kept) | items of picture sources | step |
| `crop` | Crop | item layout | `crop` | no crop | items of picture sources | step |
| `opacity` | Opacity | item layout | `opacity` | 1.0 | items of picture sources | step |
| `style` | Shape and border | item layout | `style` (shape, border, shadow) | rectangle, no border, no shadow | items of picture sources | step |
| `video_filters` | Picture filters | source | filters of picture node types, in order, with `enabled` and settings | none | picture sources | step |
| `audio_filters` | Sound filters | source | filters of audio node types, in order, with `enabled` and settings | none | sources with sound | step |
| `options` | Source options | source | the class's **option keys** (decision 2) | the option values a new source of the type gets | sources of the **same type** that have option keys | step |
| `region` | Capture region | source | `region`, and for a display source also `chrome` (written together, as `source.set_chrome`) | whole picture, no Display chrome | same type only | step |
| `glide` | Glide | source | the `glide` override (ADR-0100) | none (the collection glide) | picture sources | step |
| `record_separately` | Record separately | source | the `record_separately` feeds the target has (`video` for a picture, `audio` for sound, `video_as`) | off | all sources | step |
| `sound` | Volume and monitoring | source | `audio.volume`, `audio.muted`, `audio.monitor` | full volume, not muted, monitor off | sources with sound | **live** |
| `sync_offset` | Sync offset | source | `audio.sync_offset_ms` | 0 ms | sources with sound | **live** |

**Never part of a group** (never pasted or reset; Duplicate copies them as decision 3 says): the name, the identity keys (decision 2), the hints (`target_app`, `target_title`, `companion_of`, `standby`), `audio.mixer_hidden` (a view choice, [ADR-0080](0080-hide-sources-in-the-mixer.md)), `Source.id`, `type_id`, the item's `id`, `content`, `visible`, `locked` and `show_for_ms` (`overrides` follow the canvas rule below), the scene's own levels (ADR-0091) and the source's places in scenes.

**Live groups.** `sound` and `sync_offset` stay live controls, as their direct edits are. A reset or paste writes them through the live record outside the undo step, the way `scene.remove` switches the program outside its step. Undo of that step never restores them, so it can neither unmute a microphone on air nor be refused because a fader moved. Live groups are **never chosen by default**: the dialogs leave them unchecked, and an API call without `groups` means the non-live groups only, so a reset never unmutes a microphone or takes a "Monitor only" source onto the outputs unless asked by name. The live writes run only after the undo step was applied, so a refused target leaves the faders alone. The dialogs say "Not undone by Undo" in their help, and the API answer lists them under `live` (decision 7). `sound` writes the source's own volume and mute; in a scene in Own levels the heard level is the scene's entry, so a paste may change nothing audible there (the help says so).

**Item groups and canvases.** The snapshot holds the copied item's layout on the copy call's canvas (`canvas?`, default the main canvas). A paste writes the target item's layout on that canvas: its override when it has one for that canvas, else its default layout (the rule of every `item.*` edit, ADR-0084 decision 5). Other overrides are untouched. Positions and sizes are canvas pixels, copied without scaling between canvases of different sizes.

**Different picture sizes.** When `crop` and `transform` are both pasted, crop is applied first. `crop` is scaled in proportion when both pictures' sizes are known and differ, else copied as pixels. `transform` takes the copied width and the height that keeps the target's cropped aspect ratio when the target's size is known, keeps the sign of each size (a flip stays a flip), and takes position and rotation as they are; `item.fit` stays the way to stretch. When the target's picture size is unknown (it has not run in this session), `transform` and `crop` are skipped for it with the reason `no_picture_size`, and reset skips `transform` the same way. `region` and `chrome` are copied as full-picture pixels; when the target's picture size is known and the region does not fit in it, the group is skipped with the reason `out_of_picture`.

**Locked items** are skipped for item groups with the reason `locked`; their source groups still apply. A refused group never stops the other targets.

**Where the logic lives.** `rbs_scene::AttributeGroup` (closed enum, `ALL`, `id()`, the table above as data, and `applies_to(&Source, Option<&SceneItem>, kinds: SourceKinds)` where the host computes `SourceKinds { picture, sound }` and the filter kinds from its node descriptors; `rbs-scene` stays free of `rbs-sources` and the filter catalogue) is the single place a group is added. A test destructures `Source`, `SourceAudio`, `SceneItem`, `Layout`, `RecordSeparately` and `ItemStyle` exhaustively and fails when a field is in neither the table nor the "never" list. Duplicate, reset and paste are **host compositions** of ordinary edits (`InsertSource`, `InsertItem`, `ReplaceSource`, `ReplaceItem`, `SetSourceChrome` in one `Op::Batch`), as `look.apply` is: checked first for every target, then applied all or nothing as one `History::record`, plus the live writes.

### 2. Identity and option keys per source class

`rbs-sources` gets `identity_keys(type_id)` and `option_keys(type_id)` next to `known_keys` (a classification only), and `scene-sources.md` gets the table. Every known key is either identity or option:

| Class | Identity keys (never pasted or reset) | Option keys |
| --- | --- | --- |
| Camera capture | `adapter`, `target` | `width`, `height`, `fps` |
| Display and window capture | `adapter`, `target` | `cursor`, `width`, `height`, `fps` |
| Output loopback audio | `adapter`, `target`, and `exclude_own_process` on a companion (`companion_of` set: "System sound" of a display must keep RBS's own sound out) | `exclude_own_process` on a plain loopback source |
| Audio input | `adapter`, `target` | none (no `options` group) |
| Still (image, sample, colour, gradient) | all its keys: the picture is what it shows | none |
| Media file | `adapter`, `path` | `loop`, `restart_when_shown` |
| Browser | `url` | `width`, `height`, `fps`, `transparent` |
| Plugin source types | all keys, until the manifest declares option keys | none |

Option defaults depend on the canvas (a display's `fps` is the canvas frame rate, a browser's size is the canvas size capped at 1920x1080, a camera gets `CAMERA_SIZE`), so they are computed in `rbs-app` by one function `option_defaults(type_id, canvas)` that the Add source path (`new_source`, `add_browser_page`) and reset both call; the default is that value, or the key left out where it writes none. The canvas is the studio's canvas. A pasted camera mode the target camera lacks is written anyway and fails only when it opens, shown as its `Unavailable` state (ADR-0077). A reset or paste of `options` is a settings change, so it replaces the source node, with break-before-make on air (`scene-model.md`); the dialogs' (i) help says the device reopens.

### 3. Duplicate source

Control API `source.duplicate {source, scene?}` (`control` scope), composed by the host (decision 1). **Addressing** (the same for decisions 5 and 6): `scene` is the scene being edited by default (the preview in studio mode, else the program) and the source's first item in it is the one used; a source that is global on the studio's canvas uses its item in the global scene and `scene` must be absent (`invalid_params` otherwise); a source not placed in the scene answers `not_found` for duplicate and copy, while a reset of source groups only needs no item (item groups are then `not_applicable`).

- A **new source** with a new `SourceId` and a free name by `unique_name` (`"<name> 2"`, `"<name> 3"`). It copies the type, all settings, every filter (new `FilterId`s, `enabled` and settings kept), `audio` (all of it, `mixer_hidden` included), `glide`, `region`, `chrome` and `record_separately`. Hints: `target_app` and `target_title` are copied; `standby` is copied (a source the user deactivated stays deactivated); `companion_of` is decided below.
- **One new item** in that scene, directly above the copied item, with the same layout, every per-canvas override, `visible`, `locked` and `show_for_ms`. Other scenes that place the original do not get the copy. If the scene is in Own levels, the copy gets the original's entry (through `InsertSource.levels`).
- A **global source** (ADR-0065) gives a global copy: a new item in the canvas's global scene above the original; its companion copy goes into the global scene too.
- **Companions** (ADR-0096). Duplicating a picture source also duplicates its companion, whose copy gets `companion_of` set to the picture copy and one item only in the same scene: directly above the original companion's item there if it has one, else directly above the picture copy. Duplicating a companion on its own drops `companion_of`: the copy is an independent sound source.
- **Key links** (ADR-0095). Within the set copied in one operation (a picture and its companion here; a whole scene in decision 4), every filter setting the host's filter catalogue marks as `FieldKind::Source` (the ducker's `key_source` and any later one; `rbs-scene` gets the remap as a plain map of setting values) that names a source of the set is rewritten to name its copy; one naming a source outside the set is kept.
- **Exclusive devices.** A copy of a camera or capture-card source would open the device a second time at once, because every source placed on a showable scene is warm (`scene-model.md`). The copy of a source whose class is Camera capture therefore starts in **standby** (the `standby` hint). The class, not a capability, is the key because no adapter reports exclusive opening today; decision 9 or a later `exclusive_open` capability replaces it. Properties shows a **derived** note (never stored) while the source is in standby and another active source has the same identity keys: "Activate it to use it. <name> uses the same device." Activating it is the user's choice; on a platform that opens the device exclusively one of the two then shows `Unavailable` with fault `Busy`. While the copy stays in standby the original keeps its device; once both are active, which one opens first after a restart is not specified.
- One undo step (`Op::Batch`), `EditKind::Duplicate` with the original's name ("Undo duplicate Camera"). The answer is `{source, item, active, companions: [{source, item}]}` (`active` false for a standby copy). In the window the copy becomes the selected source.

### 4. Duplicate scene with new sources

`scene.duplicate` gains `sources?: "shared" | "copied"` (default `"shared"`: unchanged). With `"copied"`:

- The set copied is the union of the sources placed directly in the scene and their companions, each copied once (decision 3's rules, including the standby rule for cameras). The wire parameters become their own struct (today `SceneRef`, shared with `scene.remove` and `scene.switch`, refuses unknown keys). Global sources and sources inside nested scenes are not copied; nested scenes stay references.
- Each item of the new scene points at the copy of its source; a source placed twice gets one copy placed twice.
- The new scene's own levels (if any) are re-keyed from each original to its copy; key links are remapped within the set.
- One undo step; the answer adds `sources: [{from, to}]`.

UI: the scene row's context menu and the Scene menu list "Duplicate scene" and "Duplicate scene with new sources" (registry `scene.duplicate?sources=copied`).

### 5. Reset source

`source.reset {source, scene?, canvas?, groups?}` (`control` scope; the item is the source's first item in `scene`, as in decision 3) sets each chosen group to its default (all applicable non-live groups when `groups` is absent; the window's dialog sends them explicitly). Groups at their default are skipped with the reason `unchanged`; a reset that changes nothing records no undo step. `sync_offset` resets to 0 ms, not to the offset remembered for the device (ADR-0126); the dialog leaves it and `sound` unchecked by default. `region` on a display source turns Display chrome off and clears the region (not `source.reset_chrome`'s work-area insets). The undo step is `EditKind::Reset` with the source's name ("Undo reset Camera").

### 6. Copy and paste attributes

- **The attribute clipboard is shared host state.** `StudioHost` keeps `Option<AttributeSnapshot>`: every group's value of one item and its source, taken at copy time. Later edits of the original do not change it and it survives the original's deletion. It is one clipboard for the window and every API session: a Stream Deck that copies replaces what the window copied, and the other way round. It is cleared when the scene collection is switched (`collection.switch`, `ui.collection.*`), never by an edit. Empty, it costs nothing (P13).
- `source.copy_attributes {source, scene?, canvas?}` (`control`) takes the snapshot and answers `{source, name, type, groups}`, `groups` being those whose copied value is not the default. The topic `source.attributes_clipboard.changed` carries the `source.attributes_clipboard` body (or `null`) and is published on copy and on clear.
- `source.attributes_clipboard {}` (`read`) answers that body, or `null` while empty. The window's Paste command follows this state, not its own.
- `source.paste_attributes {items: [item], groups: [group], filters?: "replace" | "append", canvas?}` (`control`) applies the chosen groups to every target in **one undo step**, plus the live writes. `items` follows ADR-0084's multi-target shape; targets may be in any scene. A source group named by two items of the same source is applied once.
- **Filters.** `"replace"` (default, as Resolve) replaces the target's filters of that kind; `"append"` adds after them. When the target's chain of that kind has the same type sequence as the snapshot's, `"replace"` keeps the target's `FilterId`s and writes only `enabled` and settings, so a live source is updated through its parameter channel with no rebuild and no audio dip (ADR-0082 decision 6); otherwise the pasted filters get new ids. `unchanged` is decided ignoring ids, so pasting a source's own snapshot back onto it changes nothing.
- `EditKind::PasteAttributes`, the name being the snapshot's source ("Undo paste attributes of Camera"); the dialog's button says how many sources, with plural forms (U8).

### 7. Answers, errors and version

- `source.reset` and `source.paste_attributes` answer `{targets: [{source, item, changed: [group], live: [group], skipped: [{group, reason}]}]}`, reasons `not_applicable`, `unchanged`, `locked`, `no_picture_size`, `out_of_picture`.
- Errors follow the existing kinds: `not_found` for an unknown source, item or scene; `invalid_params` for an empty or unknown `groups` value, unknown keys, or `filters` other than the two values; `conflict` for a paste while the clipboard is empty.
- Each publishes `scene.changed` and `source.changed` as its edits already do.
- **Effects** (`control-api.md`, irreversible-action permission): `source.duplicate` and `scene.duplicate {sources:"copied"}` are `edit` without the on-air check (a copy is never on air), and copying a Browser source falls under the network rule; `source.reset` and `source.paste_attributes` are `edit` with the on-air check, and `live` when a live group would change; `source.copy_attributes` is `edit` without the on-air check (it changes no collection); `source.attributes_clipboard` is `read`.
- Control API: the next free minor at merge (1.68 if nothing else lands first). An older server answers `method_not_found` for the new methods; `scene.duplicate {sources}` on a server before that minor answers `invalid_params` (unknown key).

### 8. UI (needs the owner's visual approval before merge, rule 16)

- **Registry entries:** `source.duplicate` (Ctrl+D), `source.copy_attributes` (Ctrl+Shift+C), `ui.source.paste_attributes` (Ctrl+Shift+V, opens the dialog), `ui.source.reset` (opens the dialog), `scene.duplicate?sources=copied`. Ctrl is Command on macOS (`shortcuts.md`); none of the three shortcuts is reserved or used. They act on the selected source; Ctrl+D is bound to `source.duplicate` only (chords have no per-panel scope).
- **Source context menu** (16 items at most, `MAX_CONTEXT_ITEMS`; 13 are used, 15 for a source right-clicked on a picture, which adds a separator and Open projector): "Duplicate source" goes into the existing Source submenu; one new submenu "Attributes" directly after the Source submenu, in the same separator block (no new separator, so 14 or 16), holds "Copy attributes", "Paste attributes..." and "Reset source...". A submenu drops disabled items today, so Paste is absent while the clipboard is empty (the command search shows it disabled with the reason "Copy a source's attributes first."). The slot test counts the picture extras.
- **Paste attributes dialog** (one modal, library components only, compact): title "Paste attributes of <name>"; left, the groups as `Checkbox` rows under Picture / Sound / Source, a group the snapshot holds at its default dimmed with "(default)", a group no checked target accepts disabled with the reason in (i), the live groups with the note "Not undone by Undo" in (i); under the filter groups a `SegmentedControl` Replace | Add after. Right, "Paste onto": one `Checkbox` row per item of the **scene being edited** (a source placed twice shows twice, with its position), the selected one checked, the copied one marked "copied", and a "Same type only" `Toggle`. `DialogActions`: Cancel, "Paste onto 3 sources". The groups chosen last are remembered for the session.
- **Reset source dialog:** the same left column for one source, the non-live groups that differ from their default checked, the others dimmed, `sound` and `sync_offset` unchecked. `DialogActions`: Cancel, Reset. The name avoids clashing with a filter's "Reset to defaults" button.
- Glossary rows with an "avoid" column for "Duplicate source", "Duplicate scene with new sources", "Attributes", "Copy attributes", "Paste attributes", "Reset source" and the group names, in the same change.
- All sizes and colours are theme tokens; a missing token is added in the same change (rule 15).

### 9. Later: one device open for several sources (own ADR)

A beyond-OBS follow-up, not decided here: two sources naming the same device and mode could share **one capture node**, its picture fanned out to each source's own filter chain, so "same camera, two looks" (a wide shot and a cropped close-up with other filters) works on every platform with one open device and one upload, and the standby rule of decision 3 goes away. It touches the binder and the source graph (P15), so it needs its own ADR, measurements and review.

## Consequences

- Duplicate, reset and paste are made of existing ops (`InsertSource` with `levels`, `InsertItem`, `ReplaceSource`, `ReplaceItem`, `SetSourceChrome`, `Op::Batch`), composed by the host, plus the live audio record; undo works as for any edit. No persisted field is added (standby is an existing hint), so the **scene format does not change**.
- New: `rbs_scene::AttributeGroup`; `rbs-sources` identity and option keys; `rbs-app` `option_defaults`; host compositions; `EditKind::{Duplicate, Reset, PasteAttributes}`; six API surfaces; registry entries and two dialogs.
- The attribute table is a contract: every future user-set field of the listed structs must be classified, enforced by a test.
- Tests: `rbs-scene` unit and property tests (duplicate then undo restores the collection exactly; levels re-keyed; key links remapped inside the set only; companions copied once; guard test); host tests (one step per command; undo of a paste after a fader move succeeds and leaves the fader alone; locked and unknown-size skips; filter ids kept for the same chain; clipboard cleared on collection switch only; all-or-nothing on a refused target; a reset without groups leaves a muted microphone muted; a reset of a display's sound companion keeps `exclude_own_process`); API conformance tests; a context-menu slot test.

## Alternatives considered

- **Duplicate as a reference only.** RBS already has it ("Existing source"); the owner asked for a copy that changes on its own.
- **Sound on the undo stack.** Would undo volume with the paste, but contradicts ADR-0059 and ADR-0062 (undo must never unmute a microphone on air) and a fader move would block every older undo. A masked record could fix the second, not the first.
- **System clipboard.** Would paste between collections or instances, but needs a text format for every group and exposes ids; a later "Save attributes as a look" covers sharing.
- **Paste from the live source (`from: item`) instead of a snapshot.** No shared state, but an edit of the original after Copy would change what Paste does and deleting it would disable Paste. Resolve and OBS paste what was copied. A `from` parameter can be added later without breaking anything.
- **Copy the device copy active.** Simple, but on Windows the original or the copy shows nothing, depending on open order after a restart.
- **Multi-selection in the Sources panel instead of a target list.** A larger change across panels and every source command; the list gives "paste onto several" now and keeps working when multi-selection comes.

## Review

- Round 1, independent AI session, 2026-11-16, changes requested (`source-attributes/review-0133-round1.md` in the project files). Resolutions:
  1. Sound and sync offset on the undo stack: now live groups outside the step (decision 1).
  2. `options` had no schema: identity and option keys per class in `rbs-sources` and `scene-sources.md` (decision 2); host composition.
  3. Own levels and key links: re-keyed and remapped within the copied set (decisions 3, 4).
  4. Context menu slots: Duplicate in the Source submenu, one Attributes submenu (decision 8).
  5. Transform default, canvas, overrides, locked, unknown size: defined (decision 1).
  6. Companions: rules for picture, companion alone and scene copy (decisions 3, 4).
  7. Region and chrome: one group, written together (decision 1, 5).
  8. `mixer_hidden`, `enabled`, feed masking, wider guard test: done (decisions 1, 3).
  9. API: `invalid_params`/`conflict`, `items`, `{source, scene?}`, answers, protocol 1.43, shared clipboard with read method and event, cleared on collection switch (decisions 3-7).
  10. Logic location and `applies_to` inputs: `SourceKinds`, host compositions (decision 1).
  11. Filter ids kept for the same chain (decision 6).
  12. Camera copies start in standby; ADR-0077 terms (decision 3).
  13-15. Glossary terms, `EditKind`s, plural labels, edited scene, shortcuts committed, crop-then-transform order, `unique_name`, `standby` kept.
- Round 2, independent AI session, 2026-11-16, changes requested (`source-attributes/review-0133-round2.md`). Resolutions:
  1. Reset could unmute or un-monitor on air: live groups are never chosen by default, in the dialogs or the API (decision 1, 5).
  2. Canvas-dependent option defaults and the companion's `exclude_own_process`: `option_defaults(type_id, canvas)` in `rbs-app` shared with the Add path; `exclude_own_process` is identity on a companion (decision 2).
  3. Effect classes stated (decision 7).
  4. Global sources and sources not in the edited scene: addressing rule (decision 3).
  5. Context menu slots with picture extras, Paste absent in a submenu, Ctrl+D source only (decision 8).
  6. Duplicate is a host composition; `rbs-scene` gets plain data (decision 1, 3).
  7. Old server answers `invalid_params`; own params struct; `source.attributes_clipboard.changed` on copy and clear (decisions 4, 6, 7).
  8. Standby note derived, restart order unspecified, class key explained (decision 3).
  9. Region and chrome as pixels, `out_of_picture` (decision 1).
  10-12. Snapshot canvas, glossary rows, "Activate", Own levels note, full "never" list, live writes after the step, `active` in the answer, reopen help.
