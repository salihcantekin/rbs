# ADR-0122 effects: UI cost measurements (2026-11-15)

Gate per setting vs calm (ADR-0120 decision 13, ADR-0122 decision 9): idle 0 frames, render p95 rise <= 0.5 ms, max rise <= 2 ms; plus a gradient sweep and a whole-window redraw run.

## Mac (M4 Pro, macOS 26.7.1, 3024x1964 @2x, wgpu), release, commit fa1168a, median of 3 rounds, `RBS_MEASURE_UI=5`

Render time in ms (p50 / p95 / p99 / max); fresh settings for every run (`RBS_CONFIG_DIR` at an empty directory).

| effects | resize frames | resize | redraw frames | redraw |
|---|---|---|---|---|
| calm | 595 | 0.77 / 0.93 / 1.02 / 1.80 | 304 | 1.37 / 2.66 / 2.97 / 3.03 |
| glow | 542 | 0.82 / 0.97 / 1.06 / 1.91 | 303 | 1.42 / 2.90 / 3.17 / 3.83 |
| glass | 555 | 0.86 / 1.03 / 1.18 / 2.10 | 300 | 1.57 / 3.08 / 3.30 / 3.61 |
| tactile | 590 | 0.86 / 1.02 / 1.14 / 2.02 | 304 | 1.51 / 3.11 / 3.54 / 3.70 |

- Idle: 0 frames in all 12 runs.
- Resize vs calm: p95 +0.04 / +0.10 / +0.09 ms, max +0.11 / +0.30 / +0.22 ms (glow / glass / tactile).
- Whole-window redraw vs calm: p95 +0.24 / +0.42 / +0.45 ms, max +0.80 / +0.58 / +0.67 ms.
- Footprint, one sample per setting during resize: calm 342 MB, glow 370 MB, glass 347 MB, tactile 368 MB (single samples; the spread is about the noise).

Result: PASS for idle, resize and redraw.

## Not shown by this run

- **Gradient sweep**: the sweep phase drew only 1 to 3 frames per run and the hover phase 12. With fresh settings the synthetic pointer changed almost no hover state (the first-run page is showing), so the sweep did not exercise the gradient faces. It needs a run with a profile that is past the first-run page.
- **Shadow cache** (16 entries in Slint): the main window with an empty project and the three frame bar items has 6 distinct button sizes (7 with the vertical outputs), counted with the testing backend. In Glow a secondary uses two entries (shadow and inner glow) and the primary with the halo two, so that window stays within the cache. A full project (docks with more button sizes) was not counted; the redraw run above, which would show thrashing as a jump in p95, was also on the first-run page.
- Windows was not measured (the SC-PC froze during the ADR-0120 measurement on 2026-11-14; runs there need the owner's OK).
