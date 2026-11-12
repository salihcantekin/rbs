# ADR-0100: Configurable glide duration and easing

- Status: Accepted
- Date: 2026-11-11
- Deciders: Salih (owner)
- Builds on: ADR-0021 (scene collection), ADR-0061 (studio mode / transition), ADR-0023 (parameter channels; slide/fade evaluated on the node)

## Context

When the program scene changes (`scene.switch` or `canvas.transition`), a source that is in both the old and the new scene moves from its old box to its new one, and a source that appears only in the new scene fades in. Today that morph is hard-coded: 400 ms; slides use smoothstep ease-in/out (`Slide::at` in `rbs-gpu`) while fades stay linear (`Fade::at`). The control API and gpu-compositor specs stated a fixed 400 ms.

Operators who share one source across several scenes (camera in a wide shot and a close-up) want to set how fast that move feels, and whether the slide is linear or eased, without inventing a separate "link" step. Collection-wide defaults cover the common case; a per-source override is only useful when the same picture source is placed more than once.

Constraints: scene document format and control API are contracts (ADR-0014); the compositor evaluates slides and fades on the render tick (hot path, P15) so any change there must stay allocation-free and cheap; unused overrides must cost nothing beyond what is already stored (P13); `rbs-gpu` must not depend on `rbs-scene` (P1).

## Decision

1. **Collection glide defaults.** `SceneCollection` gains `glide: CollectionGlide { duration_ms: u32, easing: GlideEasing }` with `GlideEasing` = `Linear` | `EaseInOut` (`#[non_exhaustive]`). Bounds: `duration_ms` in `0..=MAX_GLIDE_MS` (`MAX_GLIDE_MS = 5000`). Default `400` and `EaseInOut`. `0` jumps (no animation). One undoable edit: `set_glide`. These defaults apply to every program-scene change on every canvas of the collection. **Duration** applies to both slides and fade-ins. **Easing** applies only to slides; fade-ins stay linear (today's `Fade::at`), so the default collection easing does not change how new sources appear.

2. **Optional per-source override.** `Source` gains `glide: Option<SourceGlide>` where `SourceGlide { duration_ms, easing }` uses the same bounds and easing enum. `None` means use the collection defaults. The override applies only to the **slide** of that source when it is in both the outgoing and incoming program scenes. A **fade-in** always uses the collection **duration** (linear). Edits: `set_source_glide` (sets or clears with `None`), one undo step. The Properties UI shows the override controls only for a picture source with more than one place (`places > 1`); the model may still hold an override if places later drop to one (it stays saved until cleared).

3. **Scene document format 12.** Root table `glide = { duration_ms?, easing? }` (defaults omitted when writing). On a source, `glide = { duration_ms, easing }` only when overridden. Formats ≤11 load as the defaults. The app writes format 12. Unknown or out-of-range values refuse the document. Older builds refuse format 12 without touching the file (P9).

4. **Compositor contract and layering.** `rbs-gpu` owns `Easing` (`Linear` | `EaseInOut`) on `Slide` (and optionally on `Fade` for future use; fade-ins from a scene change always pass `Linear`). Evaluation on the tick: linear maps `u` in `0..=1` straight; `EaseInOut` keeps today's smoothstep `u²(3−2u)`. No allocation. `rbs-scene` owns `GlideEasing` with the same variants for the document; `rbs-sources::glide_layers` maps `GlideEasing` → `Easing`. `rbs-gpu` must not depend on `rbs-scene`. `glide_layers` takes the collection glide and a lookup of source overrides; for each layer that slides it uses the source override or the collection; for each fade-in it uses the collection duration and `Easing::Linear`.

5. **In-flight `max_duration`.** At `note_scene_change`, for every canvas in `Glide::from`, consider each source that will receive a **slide** on that canvas (in both old and new layer lists with a changed transform). `max_duration` is the maximum of the collection `duration_ms` and those sources' override `duration_ms` values (collection alone when nothing slides). `paint_layers` clears the glide when elapsed time since `start` is ≥ `max_duration`. One value covers every canvas in that event.

6. **Control API.** Protocol 1.35: `collection.set_glide { duration_ms, easing }` and `source.set_glide { source, glide: null | { duration_ms, easing } }` (`control`, undoable). `canvas.transition` / `scene.switch` keep their live semantics; only the morph timing changes. `scene.list` / `scene.changed` carry the collection `glide` and an optional per-source `glide` when overridden.

7. **UI.** Transition panel / rail: Duration (`NumberField`, ms) and Easing (`SegmentedControl`: Linear / Ease in-out) edit the collection defaults (for Cut, duration is how long shared sources slide and new sources fade in). Properties → Scenes: when `places > 1` and the source has a picture, a "Custom glide" toggle and, when on, the same Duration and Easing controls; off clears the override. Visual change: propose, implement, screenshots in dark / light / high-contrast, owner approval before merge (CLAUDE.md rule 16). Tokens only.

8. **Tests and hot path.** `Slide` mid-points for both easings; `Fade` stays linear; `glide_layers` with mixed overrides and fade on collection duration only; format 11→defaults and format 12 round-trip; undo ops; out-of-range refused. Hot-path checklist, a quick before/after on slide evaluation, and a human maintainer's approval (P15) before merge of the compositor change.

## Consequences

- Operators can tune how shared sources morph without a new panel type.
- Scene format and protocol go forward; older builds refuse format 12; older clients ignore unknown methods until they upgrade.
- `Slide` gains `easing`; fade-ins from scene changes stay linear. Constructors and tests that build `Slide` must set easing (default `EaseInOut` for today's slide look).
- The Transition panel grows Duration and Easing; Properties gains a conditional block (zero UI cost when the source is single-place or audio-only).
- An override can remain in the document when `places` later drops to one; it has no UI then and does not affect fade-ins.

## Alternatives considered

- **App setting only (machine-local).** Glide feel belongs with the show (scene collection), not the computer; a touring show would lose the timing.
- **Per-canvas defaults.** One collection morph is enough for the first version; canvases share sources and the same program switches.
- **Always show Properties glide.** Clutters single-scene sources; the useful case is multi-place picture sources.
- **Per-source fade override.** A source that was not on air has no "from" box of its own; collection duration is enough for fade-in.
- **Ease fade-ins with the collection easing.** Would change today's linear fade; rejected so defaults preserve shipped fade behaviour.
- **More easing curves.** Linear and ease-in-out cover the owner's ask; further curves can extend the enums later (`#[non_exhaustive]`).

## Review

Independent review per ADR-0014.

- Reviewer: independent AI session (fresh context; given only this ADR, principles, related ADRs, specs and code)
- Date: 2026-11-11
- Verdict: approve after changes
- Concerns:
  1. Fade would become ease-in-out by default while shipped fades are linear → resolved: easing applies to slides only; fade-ins stay linear with collection duration (decisions 1, 2, 4).
  2. `GlideEasing` crate / P1 unstated → resolved: `Easing` in `rbs-gpu`, `GlideEasing` in `rbs-scene`, map in `rbs-sources` (decision 4).
  3. `max_duration` underspecified → resolved: max of collection and every source that actually slides on any canvas in that event (decision 5).
  4. `SceneCollection.glide` missing from scene-model table → resolved in specs (`set_glide`, field on the collection).
  5. gpu-compositor struct snippet out of sync → resolved in specs (`Slide.easing`, `NodeLayer.slide`).
  6. P15 human approval → accepted as implementation gate (decision 8); this review is not that approval.
  7. Long `max_duration` keeps morph path warm → non-blocking follow-up (per-canvas end times).
  8. Orphan override when places ≤ 1 → accepted (Consequences).
  9. Format 12 reversibility → accepted; refuse unknown format stays (decision 3).
- Owner approval: Salih, 2026-11-11 (implement the plan)
- 2026-11-12: Owner feedback — Glide controls left the Transition rail and `rbs.transition` panel (too crowded, not responsive). Collection defaults stay in Settings › General; per-source override stays in Properties. Decision 7's "Transition panel / rail" UI is superseded by that placement.
