# Spec: Scene model

- Status: Draft
- Owner crate(s): `rbs-scene`
- Related ADRs: [0013](../adr/0013-zero-cost-when-unused.md), [0015](../adr/0015-ui-design-rules.md), [0021](../adr/0021-scene-collection-model.md), [0039](../adr/0039-scene-source-types.md)

## Purpose

The scene collection is the document the user edits: canvases, sources, scenes and the items that place sources in scenes (glossary: Scene collection). This spec defines its model, the invariants every edit keeps, undo, and how a scene becomes the flat layer list the compositor draws. The UI, the control API, scripts and remote panels all edit it through the same operations (vision, "Declarative scenes").

## Scope

In scope: the in-memory model, validated edits with undo, scene scopes and per-canvas layout (UI rule U15), nested scenes, flattening, which sources must be Active, and binding a collection to a media graph description.

Out of scope, later changes:

- Applying the bound description through `rbs_graph::runtime::Controller` and delivering layer lists to running compositor nodes (engine wiring in `rbs-app`).
- The on-disk document format and its versioning, and the control API command schema. Both map one-to-one onto the model and operations here.
- Audio filters, per-source monitoring and mixer buses, transitions, groups, animation, keep-warm policy. Which node type a built-in source is and how instances are built: [scene sources](scene-sources.md).

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

### Graph binding (`rbs_scene::binder`)

`SceneBinder::new(types)` takes a `NodeTypes` lookup (`NodeTypeId` to `Arc<NodeDescriptor>`, from the adapter registry or plugin manifests). `sync(collection, graph)` updates a `GraphDescription` to match the collection and returns a `BindReport`.

- **Ownership.** The binder owns the nodes it creates and the links it makes (filter chains, links into each compositor's layers pin), and never touches anything else. The application attaches outputs (stream, recording, preview panels, projectors) to `compositor(canvas, role)` and may link other inputs of a compositor; those links survive every sync. If someone else removes a bound node, the next sync binds that object again from scratch (and removes filter nodes that would be orphaned).
- **Nodes.** One node per source, however many scenes and canvases place it. A source whose settings change gets a new source node of the same type and partition, so the application builds an instance for the new settings; its filter nodes stay and the chain is linked to the new node (ADR-0039). `BindReport` lists `created_sources`, `retired_sources` (also nodes someone else removed), `replaced_sources` (`(source, old, new)` on a settings change, also listed in the two lists before), `created_filters` and `retired_filters` (also removed by someone else): the application adopts an instance for each created node into its partition and takes retired instances back after the runtime released them. An exclusive device cannot open the new instance while the old one runs, so for replaced sources the application applies break-before-make (scene sources spec). One node per filter. One compositor node (type `org.rbs.compositor`, kind `Compositor`) per canvas and `Role`: `Program`, and `Preview` while studio mode is on. Node ids stay the same while their object exists (and, for sources, while their settings are unchanged), so the runtime carries running nodes across swaps (ADR-0020). Nodes of removed sources, filters and compositors are moved to Standby and removed with their links, including links the application attached to a removed compositor; `BindReport` lists `retired_compositors`, and a newly created compositor has `created: true`, so the application can re-attach outputs.
- **Wiring.** A source node must be kind `Source` with a video output; a filter node kind `VideoFilter` with video input and output; nodes are wired through their first video pins. The compositor's layers pin is its first many-input video pin (`BoundCompositor::layers_pin`). A source's enabled, bound filters are chained in order; the chain's last node is the source's *producer*. Each compositor's layers pin gets one link per distinct producer of the sources visible in its scene (`visible_sources`, so a source faded to opacity 0 stays linked and scheduled, and a fade does not change the graph); links it no longer needs are removed.
- **States.** Sources in `wanted_sources()` and their enabled, bound filters are set Active; other sources and disabled filters are Standby. Bound compositors are Active, and are scheduled only when an output is attached. These are wanted states; the runtime decides what runs (ADR-0020).
- **Layers.** `BindReport::compositors[(canvas, role)]` is a `BoundCompositor`: its node, `layers_pin`, and `BoundLayer`s bottom first (the flattened layer plus its `producer` node). `resolve(compositor, plan)` gives each layer the index of its link among the compositor's scheduled links on the layers pin, which is the order `rbs_graph::runtime::Inputs` presents them in. Callers pass the plan the runtime runs, `ApplyReport::plan`, which leaves out nodes whose `prepare` failed. Layers whose producer is not scheduled are left out, so the remaining indices stay correct for that plan. A layer list sent to a running compositor through a parameter channel can meet an older or newer schedule, so the index is only a hint: the node checks it against `layer.producer` with `Inputs::source` and falls back to `Inputs::find` ([ADR-0023](../adr/0023-node-parameter-channels.md)).
- **Audio** (only with `SceneBinder::with_audio(partition)`, ADR-0039; `partition` is an audio partition, never `PartitionId::RENDER`). One mixer node per canvas (type `org.rbs.audio.mixer`, kind `AudioMixer`, first many-input audio pin, an audio output) in that partition, created by the sync that first sees the canvas and kept while the canvas exists (`SceneBinder::mixer(canvas)`; `BindReport::mixers` with `created`; `retired_mixers` when the canvas is removed, with every link the application attached). A canvas without a program scene has a silent mixer. A source node with an audio output and no video output (audio-only) goes into that partition; a source with video stays in the render partition. Each mixer gets one link from the first audio output of each audio-only source visible in its canvas's **program** scene (`visible_sources`), so a source only in a preview scene is not on air and one canvas's audio never reaches another canvas's output. `BoundMixer::inputs` lists them in source id order; the producer is the source node, which is in the mixer's partition, so it is the strip's producer (ADR-0037). The application keeps strip settings per `SourceId` and rebuilds strips from `inputs` after every sync, because a settings change gives the source a new node id. A source with both video and audio outputs binds only its video and is reported `AudioOnVideoSource` on every sync (its audio would cross a partition queue, whose reader the mixer sees as the producer). Audio sources are never layers. Filters on a source without a video output are skipped (`FilterOnAudioSource`). Mixer states: Active. A missing or unusable mixer type is `BindError::NoMixer`. Without `with_audio`, audio-only sources are `NoVideoOutput`, as before; the settings rebind and the new report fields apply either way.
- **Problems.** A source or filter whose type is not installed, or has the wrong kind or no usable pins, is left out and reported as a `BindProblem`; the rest is bound.
- **Errors.** `BindError::NoCompositor` if the compositor type is missing or unusable; graph refusals are `BindError::Graph`. On error neither the graph nor the binder changes (sync works on copies).
- `sync` is idempotent: a second call with the same collection changes nothing.

Tests in `crates/rbs-scene/tests/binder.rs` cover audio sources feeding the mixer from program scenes only, each canvas mixing only its own program, a source with video and audio binding only its video, nodes removed by someone else reported retired, filters on audio sources, a missing mixer type, changed settings giving a new source node and keeping its filters, created and retired reports, a mixer removed by someone else, idempotence with audio, node reuse across canvases, stable ids, hidden sources going Standby and losing their link, a source at opacity 0 staying linked and scheduled, filter chains following enable and removal, links the binder did not make surviving, rebinding after external removal, created and retired compositors, the plan scheduling exactly what the program needs, `resolve` with an unscheduled producer and with a second video input, unknown types, idempotence, and random edit sequences. After each random edit the graph is checked against expectations computed from the collection, not from the binder: source and filter states, exact chain links, compositor links, and no unaccounted nodes; the same run with audio also checks partitions, mixer links and inputs, and that the created and retired reports alone track exactly the bound source nodes, including nodes someone else removes.

## Errors and failure behaviour

`SceneError` (non-exhaustive): unknown ids, `DuplicateName`, `EmptyName`, `BadTypeId`, `EmptyCanvas`, `InvalidLayout`, `InvalidSetting`, `CropOnScene`, `NestingCycle`, `NestingTooDeep`, `TooManyLayers`, `DuplicateFilter`, `ScopeViolation`, `IndexOutOfRange`, `CanvasHasScenes`, `UndoConflict`, `IdsExhausted`. Edits never panic.

## Performance requirements

Control thread only, user-paced. An edit clones the collection, applies the change and validates the result, which is O(size of the collection); collections hold hundreds of objects. Nothing here runs per frame: the graph binder recomputes layer lists only when the collection or the shown scenes change.

## Open questions

- Document format and schema technology (with the control API schema ADR). Loading needs a validating constructor from parts, including the id counters.
- Per-canvas visibility of an item in a shared scene (U15): the override could become `{layout, visible}`.
- How layer lists reach running compositor nodes (a control message at the swap, or node settings), and when compositor `reserve_layers` runs for a larger list.
- Descriptors are looked up once per bound node; replacing an installed node type needs a rebind policy (for example remove and re-add the affected nodes).
- Audio: per-source monitoring, mixer buses, keeping preview audio warm, audio filters.
- Transitions between program scenes and "keep warm" for preview scenes.
