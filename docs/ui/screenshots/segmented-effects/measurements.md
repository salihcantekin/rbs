# SegmentedControl effects: UI cost measurement (2026-11-16, partial)

Gate (ADR-0120 decision 13): calm within noise of `main`; each setting's p95 rise at most 0.5 ms and max rise at most 2 ms against calm; idle 0 frames.

Mac M4 Pro, macOS 26.7.1 (25G241), built-in 3024x1964 at 2x, wgpu, release builds, `RBS_MEASURE_UI=5`, profile past the first-run page (`experience = "pro"`). main = `56d43387` (merge base), seg = `8fd78b37`. Render time in ms (p50 / p95 / p99 / max), median of the valid rounds.

| build | effects | rounds | hover | sweep | redraw | resize |
|---|---|---|---|---|---|---|
| main | calm | 2 | 1.69 / 2.90 / 3.11 / 3.11 | 2.64 / 3.34 / 3.45 / 3.45 | 1.83 / 2.88 / 3.06 / 3.20 | 0.71 / 0.91 / 1.02 / 1.50 |
| main | glow | 2 | 2.10 / 3.62 / 3.88 / 3.88 | 2.40 / 3.55 / 3.72 / 3.72 | 2.00 / 3.08 / 3.25 / 4.04 | 0.77 / 0.96 / 1.06 / 1.79 |
| seg | glow | 1 | 1.60 / 3.21 / 3.24 / 3.24 | 1.48 / 2.96 / 3.17 / 3.17 | 1.20 / 2.62 / 2.88 / 3.00 | 0.78 / 0.93 / 1.05 / 1.69 |
| main | glass | 2 | 2.31 / 3.52 / 4.18 / 4.18 | 2.87 / 3.54 / 3.78 / 3.78 | 2.10 / 3.13 / 3.37 / 3.64 | 0.81 / 1.02 / 1.15 / 1.73 |
| main | tactile | 2 | 2.69 / 3.37 / 3.84 / 3.84 | 3.01 / 3.72 / 4.00 / 4.00 | 2.66 / 3.35 / 3.54 / 3.68 | 0.83 / 1.04 / 1.13 / 1.77 |
| seg | tactile | 1 | 2.79 / 3.25 / 3.27 / 3.27 | 1.63 / 3.45 / 3.60 / 3.60 | 1.31 / 2.78 / 2.95 / 3.27 | 0.81 / 0.96 / 1.05 / 1.74 |

- Idle: 0 frames in every valid run.
- Where seg has a valid round (Glow, Tactile), its p95 and max are at or below main's in every scenario.
- **Pending (not measured yet):** seg Calm and seg Glass, to be measured on a later device slot. Three runs stopped drawing partway (the window lost the front) and are invalid; the run was stopped early at the owner's request.
- The main window of the measure mode shows few SegmentedControls (Properties only, with a source). Calm adds no drawn item to a segment, only the `if` slots, as in Button (ADR-0120 decision 10).
