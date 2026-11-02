# Spec: Scene model

- Status: Draft
- Owner crate(s): `rbs-scene`
- Related ADRs: [0013](../adr/0013-zero-cost-when-unused.md), [0015](../adr/0015-ui-design-rules.md), [0021](../adr/0021-scene-collection-model.md)

## Purpose

The scene collection is the document the user edits: canvases, sources, scenes and the items that place sources in scenes (glossary: Scene collection). This spec defines its model, the invariants every edit keeps, undo, and how a scene becomes the flat layer list the compositor draws. The UI, the control API, scripts and remote panels all edit it through the same operations (vision, "Declarative scenes").

## Scope

In scope: the in-memory model, validated edits with undo, scene scopes and per-canvas layout (UI rule U15), nested scenes, flattening, and which sources must be Active.

Out of scope, later changes:

- Turning a collection into a `GraphDescription` (source, filter and compositor nodes, layer lists for compositor nodes) and applying it through `rbs_graph::runtime::Controller`.
- The on-disk document format and its versioning, and the control API command schema. Both map one-to-one onto the model and operations here.
- Audio items and mixer routing, transitions, groups, animation, keep-warm policy.

## Contract

### Model (`rbs_scene::model`)

| Type | Contract |
| --- | --- |
| `CanvasId`, `SourceId`, `SceneId`, `ItemId`, `FilterId` | Issued in increasing order per kind, never reused, so an undo restores an object under its old id. `from_raw` / `get` convert for documents and commands; the document must also store the id counters, or ids could be reused after a reload |
| `Canvas` | Name, `Size`, `FrameRate`, `program` scene, `preview` scene (studio mode when set) |
| `Source` | Name, node `type_id`, `Settings`, ordered `filters`. One source can be placed in many scenes and canvases; it runs as one instance |
| `Filter` | Node `type_id`, `enabled`, `Settings` |
| `SceneScope` | `Canvas(id)`: only that canvas shows it, directly or nested. `Shared`: every canvas may show it (U15) |
| `Scene` | Name, `scope`, `items` bottom first |
| `SceneItem` | `content` (`Source` or nested `Scene`), `visible`, default `layout`, `overrides: BTreeMap<CanvasId, Layout>`; `layout_on(canvas)` picks the override or the default |
| `Layout` | Box at `position` (top-left, canvas pixels) of `size` (negative flips), rotated clockwise by `rotation` degrees about its centre; `crop` in source pixels; `opacity` 0..=1 |
| `Settings` | `BTreeMap<String, SettingValue>`; values `Bool`, `Int`, `Float` (finite), `Text`. Schemas come with the plugin SDK |

### Edits (`SceneCollection`)

Canvases: `add_canvas`, `remove_canvas`, `rename_canvas`, `set_canvas_format`, `set_program`, `set_preview`. Sources: `add_source`, `delete_source` (removes it from every scene), `rename_source`, `set_source_settings`, `add_filter`, `remove_filter`, `set_filter_enabled`, `move_filter`. Scenes: `add_scene`, `remove_scene`, `rename_scene`, `set_scene_scope`. Items: `add_item` (at an index or on top), `remove_item` (from this scene only), `move_item`, `set_item_visible`, `set_item_layout` (default or for one canvas), `clear_item_layout`.

Every edit returns an `Undo`. `SceneCollection::undo(record)` applies it and returns the record that redoes it. Records form a stack: applied in reverse order they restore every earlier state exactly. Every record carries the state it expects to find (for replacements, the whole current canvas, source, scene header or item; for moves, the current index), so a record used out of order is refused with `UndoConflict` instead of reverting later changes, never applied half-way. `Undo` is opaque for now; a serialisable operation type comes with the control API schema.

Names are trimmed of surrounding whitespace on input.

`set_program` and `set_preview` change the document (which scene a canvas shows is saved) and return an `Undo` like any edit, but they are live actions: the control API does not put them on the user's undo stack, so undoing a layout change can never cut the program (ADR-0021).

Equality of collections compares content and ignores id counters (undo does not wind them back).

### Invariants

Checked after every edit on the would-be result; a refused edit leaves the collection unchanged. Each is covered by a test in `crates/rbs-scene/tests/collection.rs`.

1. Names are non-blank and unique among canvases, among sources and among scenes.
2. Node type ids are reverse-DNS; float settings are finite.
3. Canvases are at least 1x1.
4. Every item refers to an existing source or scene; every override and canvas scope refers to an existing canvas.
5. Layouts are finite with opacity in 0..=1. Crop is only set on source items (a nested scene cannot be cropped by a flat layer list).
6. Nesting is acyclic, at most `MAX_NESTING_DEPTH` (16) scenes deep, and no scene flattens to more than `MAX_LAYERS` (4096) layers, counting hidden items (so showing an item never fails). Diamond-shaped nesting multiplies layers, so the bound is checked on every edit; it also bounds the recursion of `flatten`.
7. Scope (U15): a canvas shows only its own and shared scenes as program or preview. A scene scoped to canvas C may nest shared scenes and scenes of C; a shared scene may nest only shared scenes, because it can be shown anywhere.
8. Per-canvas layouts exist only for canvases that may show the scene (a scene scoped to canvas C has overrides for C only). Filter ids are unique across the collection.
9. Removing a canvas is refused while scenes are scoped to it; its per-canvas layouts are dropped (and restored by undo). Removing a scene removes the items that nest it and clears it from program and preview (restored by undo). Deleting a source removes all its items (restored in their original positions by undo).

A property test applies random sequences of every edit kind, checks that refused edits change nothing and every reachable state flattens on every canvas, then undoes everything back to the start and redoes it to the end, comparing every intermediate state. Another test checks that each kind of record is refused when used out of order.

### Flattening

`flatten(canvas, scene) -> Vec<FlatLayer>`, bottom first. A `FlatLayer` has the `source`, the item `path` from the shown scene to the source item, a `transform` (`Affine2`, same convention as `rbs-gpu`: maps the unit square of the cropped source to canvas pixels), the `crop` in source pixels (the compositor turns it into texture coordinates with the real source size) and `opacity`.

- The layout used for each item is `layout_on(canvas)`.
- Hidden items, everything inside a hidden nested scene, and items whose effective opacity is 0 are left out.
- A nested scene's canvas-sized coordinate space is scaled into its item's box; transforms compose, opacities multiply.
- `flatten` refuses a scene the canvas may not show (`ScopeViolation`).
- Opacity of a nested scene multiplies into each of its layers; this is not group opacity (overlapping layers inside a half-transparent nested scene show through each other). Nested content outside the nested scene's canvas area is not clipped to the item's box, and crop is not available on nested scenes. Group opacity, clipping and crop of nested scenes need render-to-texture for nested scenes, a later decision.
- Crop larger than the source is clamped by the compositor.

`visible_sources(canvas, scene)` is the set of sources of visible items (not inside a hidden nested scene), whatever their opacity: a fade from 0, or a source used for its audio, still needs it running. `wanted_sources()` is the union over every canvas of its program and preview scenes: the sources the scenes want Active for video (media graph spec, Lifecycle). Projectors, outputs and audio-only use add to it in the graph binder; a source wanted by none of them is Standby.

## Errors and failure behaviour

`SceneError` (non-exhaustive): unknown ids, `DuplicateName`, `EmptyName`, `BadTypeId`, `EmptyCanvas`, `InvalidLayout`, `InvalidSetting`, `CropOnScene`, `NestingCycle`, `NestingTooDeep`, `TooManyLayers`, `DuplicateFilter`, `ScopeViolation`, `IndexOutOfRange`, `CanvasHasScenes`, `UndoConflict`, `IdsExhausted`. Edits never panic.

## Performance requirements

Control thread only, user-paced. An edit clones the collection, applies the change and validates the result, which is O(size of the collection); collections hold hundreds of objects. Nothing here runs per frame: the graph binder recomputes layer lists only when the collection or the shown scenes change.

## Open questions

- Document format and schema technology (with the control API schema ADR). Loading needs a validating constructor from parts, including the id counters.
- Per-canvas visibility of an item in a shared scene (U15): the override could become `{layout, visible}`.
- How the graph binder represents per-compositor layer lists (node settings vs. a control message) and keeps node ids stable across edits.
- Audio: items that carry audio, per-source monitoring and mixer routing.
- Transitions between program scenes and "keep warm" for preview scenes.
