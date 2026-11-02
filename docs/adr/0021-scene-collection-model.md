# ADR-0021: Scene collection as a validated model with scoped scenes and inverse-edit undo

- Status: Accepted
- Date: 2026-11-02
- Deciders: Salih (owner)

## Context

The UI, the control API, scripts and remote panels all edit scenes (vision, "Declarative scenes"; ADR-0008). UI rule U15 asks for several canvases: scenes belong to one canvas or are shared, and a source can be placed on several canvases with an independent layout. `rbs-gpu` draws a flat layer list and leaves scene tree evaluation to `rbs-scene` (gpu-compositor spec). The media graph needs to know which sources must be Active (ADR-0013). The scene document format is a public contract (ADR-0014), so the model it serialises must be decided first.

## Decision

1. `rbs-scene` holds a `SceneCollection` of canvases, sources (with filter chains), scenes and scene items, keyed by typed ids that are never reused. It depends on `rbs-core` and `rbs-graph` (for `NodeTypeId`) only.
2. Sources are shared objects; an item places a source (or a nested scene) in one scene. Deleting a source removes all its items; removing an item touches one scene (glossary).
3. Scenes have a scope: `Canvas(id)` or `Shared`. A canvas shows only its own and shared scenes; a shared scene may nest only shared scenes. Each item has a default layout plus optional per-canvas overrides, so a shared scene can be laid out differently on a horizontal and a vertical canvas while using one source instance.
4. Every edit is all-or-nothing: it is applied to a copy, the whole copy is validated against the invariants of the [scene model spec](../specs/scene-model.md), and only then committed.
5. Undo uses inverse operations, not snapshots: each edit returns an `Undo` record holding the primitive operation that reverses it together with the state it expects to find, and applying a record returns its redo. Records are used as a stack; a record used out of order is refused (`UndoConflict`), never applied over later changes. The operations are the intended basis of the engine command log; they become a public, serialisable type with the control API schema.
6. Switching program and preview is document state (saved with the collection) and returns an `Undo` like other edits, but it is a live action: the control API keeps it off the user's undo stack, so undoing a layout change never cuts the program.
7. The collection bounds itself: nesting at most 16 scenes deep and at most 4096 flattened layers per scene, checked on every edit, so flattening is bounded in time and stack depth whatever a document or remote client sends.
8. Flattening produces the compositor's layer list (bottom first, nested scenes expanded, per-canvas layouts applied, hidden items left out). It uses the same affine convention as `rbs-gpu` but its own type, so `rbs-scene` does not depend on the GPU crate. Crop stays in source pixels and is converted by the compositor node.
9. `wanted_sources()` (sources of visible items in any canvas's program or preview, whatever their opacity) defines which source nodes the scenes want Active for video; projectors, outputs and audio-only use are added by the graph binder.
10. Binding to the media graph and the on-disk format are separate later decisions; the format maps one-to-one onto this model.

## Consequences

- The UI and API can be built against one model with one set of validation rules; a refused edit never leaves partial state.
- Undo and redo are exact (property-tested over random edit sequences) and cheap to store.
- Edits cost O(collection size) because of the copy; fine for user-paced edits on the control thread, and simpler to keep correct than in-place rollback. If very large collections appear, validation can be made incremental without changing the API.
- Crop is not supported on nested scenes, nested content is not clipped to the item's box, and a nested scene's opacity multiplies into each layer (not group opacity). All three need render-to-texture for nested scenes, a later decision.
- Per-canvas visibility of an item in a shared scene is not supported yet; the per-canvas override can grow a `visible` field without breaking callers.
- Adding a `SceneScope` variant later means revisiting the scope rules (`allowed_on`, `may_nest`); the enum is `#[non_exhaustive]` so callers are prepared.
- Public types are `#[non_exhaustive]` with constructors where they are built by callers (`Layout`, `Crop`), so fields such as blend mode, audio settings or keep-warm can be added.

## Alternatives considered

- **Snapshot undo** (store the whole previous collection): trivially correct, but memory grows with collection size times history length, and snapshots do not form a command log for remote clients.
- **One scene list per canvas, duplicating sources**: simpler scoping, but two instances of the same camera or capture cost twice and can disagree; U15 asks for one source with independent layout.
- **Separate layout objects per (item, canvas) for every canvas**: more explicit, but every new canvas would need a layout for every item; default plus overrides needs none.
- **Flattening on the GPU side**: would make `rbs-gpu` depend on the scene model; the GPU crate deliberately draws flat lists only.

## Review

Independent AI session per ADR-0014, 2026-11-02 (got only the proposal, specs and code). Verdict: changes requested; all concerns addressed in the same change:

1. Blocking: out-of-order undo of whole-object replacements was silently applied (undoing a rename could revert a later cut). Every replacement and move now carries the expected current state and is refused with `UndoConflict` otherwise; test `out_of_order_undo_is_refused_not_applied` covers each record kind.
2. Blocking: `flatten` cost was unbounded (diamond nesting is exponential; deep chains could overflow the stack). Added `MAX_NESTING_DEPTH` (16) and `MAX_LAYERS` (4096) as invariants; test `nesting_depth_and_layer_count_are_bounded`.
3. Program/preview cuts on the undo stack: Decision 6.
4. `Undo` is opaque: wording in Decision 5 changed; the public operation type comes with the control API schema.
5. Wire/document needs: ids have `from_raw`; the spec says documents must store id counters; a validating loader is an open question.
6. `wanted_sources` dropped transparent sources and was described too broadly: now based on visibility, wording narrowed (Decision 9); test `transparent_but_visible_sources_stay_wanted`.
7. Nested opacity and clipping semantics documented (Consequences, spec).
8. Overrides for canvases that can never show the scene are refused; per-canvas visibility recorded as a follow-up.
9. Names are trimmed on input.
10. Filter ids are checked for uniqueness; crop larger than the source is documented as clamped by the compositor.
11. The property test now generates every edit kind and compares every intermediate state on redo; flatten tests for mirrored and rotated nested layouts added.
12. Semver note on `SceneScope` added.

Owner approval: Salih, 2026-11-02 (early-phase merge policy: merge once CI is green; large reviews later).
