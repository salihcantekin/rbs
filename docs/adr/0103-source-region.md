# ADR-0103: A source may store a capture region distinct from the item crop

- Status: Proposed
- Date: 2026-11-12
- Deciders: Salih (owner)
- Builds on: [ADR-0058](0058-crop-editing-of-sources.md), [ADR-0039](0039-scene-source-types.md), [ADR-0077](0077-source-health-retarget-and-settings-window.md)

## Context

Item crop (ADR-0058) is per placement: “in this scene, show this part of the picture.” A solo creator often wants the source itself to be only that part—for example a Media Player window cropped to the video pane—so every scene and every canvas sees the same region without studio-mode crop each time. Capture adapters still deliver the whole surface; the compositor already samples a UV rect. Settings changes restart the capture node (ADR-0039). Putting a region in settings keys would restart the device on every drag.

Filters run on the full frame before the compositor samples. Grading the whole window, including chrome the user never sees, is an accepted trade for this ADR. Region capture in adapters (ADR-0058 decision 8) stays deferred.

## Decision

1. **A source may store a region** in source pixels: left, top, right, bottom. Absent means the whole picture. The field lives on `Source`, not in settings. The binder’s settings compare must not see it. Editing the region must not replace the capture node. When the delivered picture shrinks or moves, the stored region is not re-derived (same as ADR-0058 item crop). Flatten/compositor uses the intersection of the region with the current picture; an empty intersection skips the layer (empty-crop rule).
2. **Item crop stays in full-picture source pixels.** Effective sampling is the intersection of the source region (or the whole picture) with the item crop. Fit, recrop, and `picture_size` for scale-keeping use the region’s size when a region is set (`w = picture_w − left − right`, same for height, after clamping to the picture), otherwise the capture picture size.
3. **The compositor still receives the full texture.** Filters still run on the full frame. Region and item crop merge into one UV rect in flatten (or the existing crop path); `Compositor::render` must not gain a per-layer “has region” branch. A colour change (saturation 0) grades cropped-away pixels; blur may bleed across the region edge. Both are accepted. This ADR must not add a capture-port region. Touching flatten or the UV path needs `hot-path-review` only if the merge is not a pure pre-frame edit of the existing crop fields.
4. **Source settings** (display, window, camera) gain a Region section: a preview and a drag rectangle. The grab exists only while that section is open, off the control thread, cancelled on close. It must not raise `THUMBNAIL_MAX`. It is a **new** capture-port method (not `thumbnail`), long side at most 960, one in flight, with its own rule 11 review. Privacy matches ADR-0077 thumbnails: UI-only, not logged, not stored, RBS’s own windows skipped.
5. **Add source** for those kinds opens this section before the item is placed. Confirm creates the source with the region and an item fitted to the region’s aspect.
6. **Control API.** `source.set_region { source, region: [left, top, right, bottom] | null }` and `source.get_region` (scope `control`). One undo step, `EditKind` for region (distinct from `EditKind::Crop` / `item.set_crop`). Refusals: `not_found`; `invalid_params` for a region that leaves less than one pixel after intersection with the known picture, or for a non-video source. No scene-format number is claimed here.

## Consequences

- One source can be “the video pane” everywhere it is placed. Per-scene item crop remains for further narrowing.
- Spec follow-ups when implemented: scene-model, scene-sources, user-data (next format), control-api (next minor), capture-port (region grab, not region capture), ui-design-system, glossary. Hot path stays the existing UV crop; no new per-frame branch.
- Negative: filters see the full frame. Bandwidth still captures the whole window until a later region-capture ADR.
- Visual UI (settings Region section) needs rule 16 approval before merge.

## Reference tools (CLAUDE.md rule 17)

- **OBS Studio** crops on the transform of a scene item, or with a crop/pad filter. There is no first-class source region separate from the item, so reusing a cropped window across scenes means copying transform or filter settings.
- **Where that falls short:** a solo user’s “this is my source” is an item edit, not a source property; filter order can interact oddly with crop/pad.
- **What RBS does by this decision:** region is source-owned and does not restart capture; item crop nests inside it; full-frame filters are an explicit accepted limit.

## Alternatives considered

- **Region as settings keys.** Would restart the device on every edit (ADR-0039). Rejected.
- **Only item crop.** Forces studio mode or per-scene edits for a permanent window crop. Rejected for the solo path.
- **Region capture in adapters first.** Saves bandwidth, needs three adapters and a port change. Deferred (ADR-0058 decision 8).

## Review

Owner approval: Salih, 2026-11-12 (implement in parallel with ADR-0104 and ADR-0105).

Reviewer: independent AI session
Date: 2026-11-12
Verdict: changes requested
Concerns:
1. **Picture resize vs a fixed source region.** Item crop behavior after ADR-0058 is explicit (box stays, picture rescales; oversize crop skips the layer). This ADR does not say what happens when the delivered picture shrinks or moves (window resize, camera mode, HiDPI) and the stored region no longer fits. -> State the rule in decision 1 or 2 (skip/clamp/clip to intersection, aligned with gpu-compositor empty-crop) before Accept.
2. **Coordinate space for nested crop.** Decision 2 says item crop is applied inside the region but both are in source pixels. The spec follow-ups must say whether `Layout::crop` stays full-picture edge insets intersected with the region, or region-relative insets, and how `Layout::recrop` and `Studio::picture_size` / effective size interact when a region is set. -> Add one sentence to decision 2 naming the space and the effective `(w, h)` for recrop/fit (decision 2 already points at region size; tie it to `picture_size()` vs region and to flatten).
3. **Hot-path contract.** Consequences claim no new per-frame branch; flatten/compositor must combine source region and item crop in one UV step (or equivalent) without a separate per-layer feature flag in `Compositor::render`. -> Cross-reference gpu-compositor and flatten in Consequences; P15 measurement when implementation touches flatten or compositor.
4. **Control API and undo vs item crop.** Decision 6 requires region get/set with one undo step (U20) but does not name commands, `EditKind`, or refusal rules (invalid region, zero area, unknown source). -> Mirror ADR-0058 decision 7 style so clients do not confuse `item.set_crop` with source region.
5. **Region-section preview grab.** ADR-0077 `thumbnail` is capped at `THUMBNAIL_MAX`; decision 4 needs its own capture-port additive method (not a larger `thumbnail` max). -> Spec follow-up is listed; the ADR should state that explicitly in decision 4 so port review does not overload `thumbnail`.
6. **Accepted limits (open).** Full-frame filters grading chrome and blur bleed are explicit trade-offs. No change required; record for implementers and rule 16 UI approval on the Region section.

Resolutions (author, 2026-11-12), written into decisions 1–6: (1) region not re-derived on resize; empty intersection skips the layer. (2) item crop stays full-picture pixels; effective UV is intersection; size for fit/recrop is the region size. (3) one UV merge; no compositor feature flag. (4) `source.set_region` / `get_region`, distinct EditKind, refusals named. (5) new port method for the 960px grab, not a larger thumbnail. (6) filter cost and rule 16 unchanged.
