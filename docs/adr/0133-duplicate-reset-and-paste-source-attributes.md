# ADR-0133: Duplicate sources, reset them to defaults, and paste chosen attributes onto other sources

- Status: Proposed
- Date: 2026-11-16
- Deciders: Salih (owner)
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0053](0053-item-style-and-video-filter-chains.md), [ADR-0059](0059-scenes-lock-and-source-audio-in-the-studio.md), [ADR-0062](0062-built-in-video-filters-and-properties-tabs.md), [ADR-0065](0065-global-sources-and-safe-source-removal.md), [ADR-0082](0082-audio-filter-chains-on-sources.md), [ADR-0096](0096-capture-audio-companions-and-camera-microphone.md), [ADR-0126](0126-measured-sync-offset-and-device-memory.md)
- Spec: [scene-model](../specs/scene-model.md) (new "Copies, resets and attribute groups" section), [control-api](../specs/control-api.md) (`source.duplicate`, `scene.duplicate {sources}`, `source.reset`, `source.copy_attributes`, `source.paste_attributes`), [ui-design-system](../specs/ui-design-system.md) (context menus, Paste attributes and Reset dialogs), [glossary](../ui/glossary.md) ("Duplicate source", "Attributes", "Paste attributes", "Reset to defaults")

## Context

The owner asked for three things (project thread, 2026-11-16):

1. Duplicate a source and duplicate a scene, if RBS cannot do it yet.
2. Put every setting of a source (for example a copy) back to its defaults.
3. Copy chosen settings of one source onto others, as DaVinci Resolve's Paste Attributes does.

What exists on main (7589237):

- **Duplicate scene exists.** `SceneCollection::duplicate_scene` and the registry command `scene.duplicate` (control API `scene.duplicate`) copy a scene with new scene and item ids, the **same sources** (references, not copies), the same nested scenes and layouts, and the scene's own levels. It is one undo step and does not go on air. Editing a source in the copy (its filters, volume, settings) changes it in the original too, because sources are shared by every scene that places them (glossary: Source).
- **Placing a source again exists** ("Existing source", `item.add {source}`): a second item of the same source. This is a reference too.
- **Duplicate source does not exist.** There is no way to get an independent source with the same settings, filters and sound.
- **Reset exists only per part:** `item.reset_transform` (flips, rotation, own size), a filter's "Reset to defaults" (its values), `source.reset_display_chrome`. There is no reset of a whole source.
- **Copy and paste of settings does not exist.** The nearest is a Look (glossary): a built-in preset of shape, border and filters, copied onto the selected source in one undo step.
- The Sources panel has one selected source; there is no multi-selection.
- Every edit of the collection returns an `Undo`; the host records one or several of them as one step of the shared undo stack (`History::record`). `Op::Batch` undoes several edits as one.

OBS for comparison: "Duplicate" of a scene asks for a name and copies items as references (sources are shared); a source can be pasted as a "Paste (Reference)" or "Paste (Duplicate)". Filters can be copied and pasted from one source to one other source; the transform can be copied and pasted to one item. There is no reset of a whole source, no choice of what to copy, and no paste onto several sources at once. A webcam duplicated in OBS on Windows opens the device twice and the second one usually stays black.

## Decision

### 1. Attribute groups

A source's state is split into named **attribute groups**. The groups are the only vocabulary for reset (decision 4) and paste (decision 5), in the engine, the control API and the UI. Each group says where it lives and what its default is:

| Group id | UI name | Lives on | Contents | Default (what a new source of the type gets) | Applies to |
| --- | --- | --- | --- | --- | --- |
| `transform` | Position and size | item | `layout.position`, `layout.size`, `layout.rotation`, and the item's per-canvas overrides of those three | the layout a new source of its kind gets (as `item.reset_transform`) | items of picture sources |
| `crop` | Crop | item | `layout.crop` | no crop | items of picture sources |
| `opacity` | Opacity | item | `layout.opacity` | 1.0 | items of picture sources |
| `style` | Shape and border | item | `layout.style` (shape, border, shadow) | rectangle, no border, no shadow | items of picture sources |
| `video_filters` | Picture filters | source | the picture filters, in order | none | picture sources |
| `audio_filters` | Sound filters | source | the audio filters, in order | none | sources with sound |
| `sound` | Volume and monitoring | source | `audio.volume`, `audio.muted`, `audio.monitor` | full volume, not muted, monitor off | sources with sound |
| `sync_offset` | Sync offset | source | `audio.sync_offset_ms` | 0 ms | sources with sound |
| `glide` | Slide | source | `glide` override (ADR-0100) | none (collection glide) | picture sources |
| `options` | Source options | source | the type's settings **except its identity keys** (`adapter`, `target` and any key the type's schema marks as identity) | the type's schema defaults, identity keys kept | sources of the **same type** only |
| `region` | Capture region | source | `region` (ADR-0103) | whole picture | same type only |
| `chrome` | Display frame | source | `chrome` (ADR-0104) | off | display sources only |
| `separate_recording` | Separate recording | source | `record_separately` (ADR-0086) | off | all sources |

Never part of a group, so never copied or reset: the name, the identity keys (which device, window, display, file or URL), the hints (`target_app`, `companion_of`, `standby`), `visible`, `locked`, `show_for_ms`, the scene's own levels (ADR-0091) and the source's place in scenes.

A group that does not apply to a target is skipped for that target, never an error, and the answer says so (decision 6). Item groups apply to one item: a crop is cut in source pixels, so when both sources' picture sizes are known and differ, `crop` is scaled in proportion to the picture size; otherwise it is copied as pixels. `transform` keeps the target's aspect ratio when the target's picture size is known (the box takes the copied width and the height that keeps the target's aspect), so pasting a 16:9 camera's place onto a 4:3 camera does not stretch it; `item.fit` stays the way to stretch.

The table is `rbs_scene::AttributeGroup` (a closed enum with `ALL`, `id()`, `applies_to(&Source, Option<&SceneItem>)`), the single place a new group is added. A new source field that is user-set must be placed in a group or listed as "never part of a group" in the spec; a test fails when `Source` or `Layout` gains a field the table does not mention (an exhaustive destructuring in the test).

### 2. Duplicate source

`SceneCollection::duplicate_source(item, name) -> (SourceId, ItemId, Undo)` and control API `source.duplicate {item}` (`control` scope):

- A **new, independent source**: new `SourceId`, a free name (`"<name> 2"`, `"<name> 3"`, the rule of `unique_scene_name`), the same type, settings, filters (each with a new `FilterId`), sound, glide, region, chrome and separate recording. Hints are copied except `standby`, so the copy starts active.
- It is placed in the item's scene **directly above the original item**, with the same layout, overrides, visibility and lock. Other scenes that place the original do not get the copy.
- A **global source** (ADR-0065) gives a global copy: a new item in the canvas's global scene above the original.
- **Companions** (ADR-0096: "Capture audio too"): each companion of the original is duplicated too, its `companion_of` hint pointing at the copy, placed above its own original item.
- One undo step (`Op::Batch` of the inserts), named "Duplicate source <name>". The copy becomes the selected source in the UI.
- **Devices.** A copy of a camera, microphone or capture card names the same device. Until decision 8 exists, opening it twice may fail on platforms that open such a device exclusively; the copy then shows the existing health state "busy" (ADR-0077) and nothing else breaks. The UI says so in the command's tooltip for device sources.

### 3. Duplicate scene with copied sources

`scene.duplicate` gains an optional `sources: "shared" | "copied"` (default `"shared"`, today's behaviour, unchanged). With `"copied"`, every source placed directly in the scene (not those of nested scenes, not global sources) is duplicated as in decision 2 and the new scene's items point at the copies; a source placed twice in the scene gets one copy placed twice. Nested scenes stay references. One undo step. UI: the scene row's context menu and the Scene menu list "Duplicate scene" and "Duplicate scene with new sources".

### 4. Reset to defaults

`source.reset {item, groups?}` (`control` scope) sets each chosen group to the default of decision 1 (all applicable groups when `groups` is absent), for that item and its source. One undo step ("Reset <name> to defaults"). Groups already at their default are skipped and listed as `unchanged` in the answer, so a reset that changes nothing records no undo step and answers `changed: []`.

The `options` default comes from the type's settings schema (the same defaults a new source of the type gets, `Values::defaults`), keeping the identity keys, so a reset camera stays the same camera. `sync_offset` resets to 0 ms, not to the remembered device offset of ADR-0126; the UI leaves it unchecked by default (decision 7).

### 5. Copy and paste attributes

- **The attribute clipboard is host state**, not the system clipboard: `StudioHost` keeps `Option<AttributeSnapshot>`, a copy of every group's value of one item and its source, taken at copy time (later edits of the original do not change it, and it survives the original's deletion). It is dropped when the collection changes. With nothing copied it is `None`: no timer, no thread, no per-frame work (P13).
- `source.copy_attributes {item}` (`control` scope) takes the snapshot and answers `{source, type, groups}`: the groups the snapshot holds a non-default value for.
- `source.paste_attributes {to: [item, ...], groups, filters?: "replace" | "append"}` (`control` scope) applies the chosen groups of the snapshot to every target in **one undo step** ("Paste attributes onto 3 sources"). `filters` (default `"replace"`, as Resolve) says whether pasted filters replace the target's filters of that kind or are added after them; pasted filters get new `FilterId`s. Targets may be in any scene. A source group pasted onto two items of the same source is applied once. Pasting onto the source the snapshot came from is allowed (it restores the snapshot).
- Refused (`not_found`) when nothing was copied, a target does not exist, or `groups` is empty or names an unknown group.

### 6. Answers and events

`source.reset` and `source.paste_attributes` answer per target `{item, source, changed: [group], skipped: [{group, reason}]}` with the reasons `not_applicable` (for example `options` on another type) and `unchanged`. Each publishes `scene.changed` and `source.changed` as the edits it is made of already do. Control API version: the next free minor at merge (1.68 if nothing else lands first); `scene.duplicate` without `sources` keeps its 1.6 meaning.

### 7. UI (proposal, needs the owner's visual approval before merge, rule 16)

- **Source row context menu** (and the Source menu): "Duplicate source", then "Copy attributes" and "Paste attributes...", then "Reset to defaults...". Paste is disabled with the reason "Copy a source's attributes first." while the clipboard is empty. Shortcuts: Ctrl+Shift+C copy attributes, Ctrl+Shift+V paste attributes (Cmd on macOS), Ctrl+D duplicate source, if the shortcuts spec has them free.
- **Paste attributes dialog** (one modal, compact, library components only): a header "Paste attributes from <name>"; on the left the groups as `Checkbox` rows, grouped Picture / Sound / Source, each group the snapshot holds at its default shown dimmed with "(default)"; groups that apply to no target disabled with the reason in the help (i). On the right "Paste onto": the sources of the scene on air as `Checkbox` rows (the selected source pre-checked, the original marked "copied from"), and a "Same type only" `Toggle`. Under the filters groups a `SegmentedControl` Replace | Add after. `DialogActions`: Cancel, Paste. The groups chosen last time are remembered for the session.
- **Reset to defaults dialog**: the same left column for one source, only the groups that differ from the default checked and enabled; groups at default listed dimmed; `sync_offset` unchecked by default. `DialogActions`: Cancel, Reset.
- **Scene row menu and Scene menu**: "Duplicate scene" keeps its meaning; "Duplicate scene with new sources" is added under it.
- All sizes and colours are theme tokens; any missing token is added in the same change (rule 15).

### 8. Later: one device open for several sources (own ADR)

A beyond-OBS follow-up, not decided here: when two sources name the same device and the same mode, the binder could give them **one capture node** and fan its picture out to each source's own filter chain, so "same camera, two looks" (a wide shot and a cropped close-up with other filters) works on every platform with one open device and one upload. It touches the binder and the source node graph (hot path, P15), so it needs its own ADR, measurements and review.

## Consequences

- Duplicate, reset and paste are plain collection edits built from existing ops (`InsertSource`, `InsertItem`, `ReplaceSource`, `SetSourceAudio`, `ReplaceItem`, `Op::Batch`); undo and redo work as for any edit, and no new persisted field exists, so the **scene format does not change**.
- The control API gets four methods and one parameter; a Stream Deck or script can duplicate, reset and paste exactly as the UI does (API parity).
- The attribute table becomes a contract: every future user-set field of `Source` or `Layout` must be classified, enforced by a test.
- The binder sees a duplicate as a new source: a duplicated device source opens the device again until decision 8 exists.
- Tests: `rbs-scene` unit and property tests (duplicate then undo restores the collection exactly; paste then undo; reset of a default source changes nothing; every group round-trips; crop scaling), host tests for one undo step per command and the clipboard's lifetime, API conformance tests.

## Alternatives considered

- **Duplicate as a reference only** (OBS's "Paste (Reference)"). RBS already has it ("Existing source"); the owner asked for a copy that can be changed on its own.
- **System clipboard for attributes.** Would let users paste between collections or instances, but needs a text format for every group and exposes internal ids; a later "Save attributes as a look" (glossary: Look) covers sharing, and the host snapshot keeps the API stateless for callers.
- **Paste from the live source instead of a snapshot.** Simpler, but surprising: an edit of the original after Copy would change what Paste does, and deleting it would disable Paste. Resolve and OBS both paste what was copied.
- **One "copy everything" without groups.** Does not cover the main use (copy only the filters, or only the size) and is what Duplicate already does.
- **Multi-selection in the Sources panel instead of a target list in the dialog.** A larger UI change across panels, the preview and every source command; the target list gives "paste onto several" now and keeps working when multi-selection comes.

## Review

Pending. Independent review per ADR-0014 is required (new control API methods, a new scene-model contract). The account's AI review quota is exhausted until 2026-11-20 14:00 (Turkey time); the owner was asked to review it himself or wait for an independent AI session.
