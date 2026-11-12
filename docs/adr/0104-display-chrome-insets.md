# ADR-0104: Display chrome insets hide the taskbar or Dock via the source region

- Status: Proposed
- Date: 2026-11-12
- Deciders: Salih (owner)
- Builds on: [ADR-0103](0103-source-region.md), [ADR-0102](0102-output-size-distinct-from-canvas.md), [ADR-0077](0077-source-health-retarget-and-settings-window.md)

## Context

People who share a full display often crop the taskbar or Dock by hand. Fixed pixel guesses fail: the bar can sit on any edge, HiDPI changes thickness, and auto-hide reports a full work area. ADR-0103 gives a source region. Display capture needs a chrome setting that fills that region from the OS work area, in the same physical pixels as the capture framebuffer (ADR-0102).

Covering a 1920×1080 canvas with a shorter work area either scales up (cover) or letterboxes (fit). Stretch distorts and lost. A single-edge control cannot describe a macOS menu bar and Dock at once.

## Decision

1. **Chrome is off for a new display source.** Only `org.rbs.source.display` may store chrome. Window and camera must not. Chrome on/off, the four insets, and fill live on `Source` next to the ADR-0103 region (not settings). The binder’s settings compare must not see them. Editing them must not replace the capture node.
2. **When chrome is on,** the source stores four insets in capture pixels (top, right, bottom, left) and a fill: `cover` (default) or `fit`. The inset rectangle is written into the source region from ADR-0103. Fill must not be baked into that rectangle. Cover and fit are layout placements of the item box on a canvas (`Layout::fit` / fill-style place); they are not a second compositor scale pass beyond sampling that region’s UV into the item box.
3. **Turning chrome on** copies insets from the adapter work area (`work_rect`, same pixel space as `SourceTarget.size`). The user may edit any edge. “Reset to system” copies again when `work_rect` is present; when absent it is a no-op and the UI disables it. Stored values are what the document keeps; opening the app must not re-query and move the picture. Auto-hide that reports zero stays zero until the user types a thickness. Drift after a display layout change is fixed by Reset or by editing edges (no automatic health retarget in this ADR).
4. **Apply** writes chrome and the region. It must not move the item box and must not restart capture. **Fit** is a separate undoable action that places the region with `cover` or `fit` on that canvas. The settings preview shows the inset and the side strips `cover` would drop. Changing method or thickness updates the sampled region inside the current box; the box moves only on Fit.
5. **`work_rect` is a capture-port addition** (optional, pixels, per display; minor additive field on the target). That port change needs its own rule 11 review. Adapters that cannot report a work area return `None` (PipeWire portal, unknown Linux); chrome can still be on with manual insets. macOS menu bar and Dock are two non-zero edges of the same four fields. Windows `rcWork` vs `rcMonitor` and macOS `visibleFrame` (points converted with the display scale) must agree with the capture size’s pixel space.
6. **Tests (ADR-0032).** Inset-to-region math; missing `work_rect`; cover/fit placement without restarting capture; binder settings compare ignores chrome fields.

## Consequences

- Hiding chrome is one toggle plus optional edge edits, not a hand crop each session.
- Spec follow-ups: capture-port (`work_rect`), scene-sources / scene-model (chrome fields, region from insets), user-data, control-api, ui-design-system, glossary. Depends on ADR-0103 being accepted and implemented.
- Negative: cover drops side strips; fit leaves empty bands the user must fill with other sources. Auto-hide cannot be detected as a bar thickness.
- Visual UI needs rule 16 approval before merge.

## Reference tools (CLAUDE.md rule 17)

- **OBS Studio** has no built-in “hide taskbar” on display capture; users crop or use a third-party filter. Work-area APIs are unused.
- **Where that falls short:** every creator redoes the same crop; HiDPI and edge changes break fixed crops.
- **What RBS does by this decision:** OS work area fills the region; cover is the default fill; fill stays out of the stored region so multi-canvas layouts stay free.

## Alternatives considered

- **Stretch the work area to the canvas.** Distorts. Rejected.
- **A single edge and thickness.** Cannot express menu bar plus Dock. Rejected.
- **Letterbox with a built-in colour bar source.** Fit already leaves empty canvas; other sources fill it. No special bar type.

## Review

Owner approval: Salih, 2026-11-12 (implement in parallel with ADR-0103 and ADR-0105).

Reviewer: independent AI session
Date: 2026-11-12
Verdict: approve
Concerns:
1. Chrome and inset fields must live on `Source` with the ADR-0103 region and must stay out of binder settings compare (ADR-0039 restart rule); the ADR says Apply must not restart capture but does not state the binder rule as explicitly as ADR-0103 decision 1 -> open: scene-model / scene-sources follow-up must mirror region placement; owner to accept or add one decision bullet before merge of the scene-format change.
2. `work_rect` is optional per display; decision 5 names Windows and macOS only. Linux (PipeWire portal) and adapters without work area need a stated behaviour (`None`, manual insets only, “Reset to system” disabled or no-op) -> open: capture-port spec and conformance must document it before the port change is accepted.
3. Cover versus fit is split from the stored inset rectangle (decision 2), but the compositor contract for how `cover` samples inside the item box while the capture texture stays full-size is not named (UV rect only versus an extra scale pass) -> open: gpu-compositor / scene-model spec must define it; if cover adds scaling beyond existing UV crop, P15 hot-path review applies (not claimed in consequences).
4. Stored insets are intentionally not refreshed on app open (decision 3); display layout or resolution changes can leave wrong edges with no tie-in to ADR-0077 health or retarget -> open: product choice (user edits or reset) is acceptable; UI copy or a light health hint is a follow-up, not a blocker.
5. Full-frame filters still run on hidden chrome (same trade as ADR-0103) -> resolved: accepted and consistent.
6. Test plan is thinner than ADR-0102 (no listed unit tests for inset-to-region math, `work_rect` versus `SourceTarget.size`, or cover/fit placement) -> open: add deterministic tests in consequences or specs per ADR-0032 when implemented.
7. Port semver and the separate rule-11 review for `work_rect` are acknowledged in decision 5 but not in consequences -> open: capture-port ADR/spec change records semver as minor additive field on `CaptureTarget` / `SourceTarget`.

Resolutions (author, 2026-11-12), written into decisions 1–6: (1) chrome fields on `Source`, out of settings compare. (2) no `work_rect` → manual insets; Reset no-op. (3) cover/fit are layout Fit/fill of the region UV into the item box, not a new compositor path. (4) no auto-refresh; Reset or edit. (5) filter trade unchanged. (6) named tests. (7) minor additive `work_rect` + rule 11.
