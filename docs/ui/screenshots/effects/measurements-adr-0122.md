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


## Addendum: gradient hover cost with a filled profile (Mac, 2026-11-15)

Mac M4 Pro (16-core GPU, macOS 26.7.1, 3024x1964 at 2x, wgpu), release build of main `ad0a0402`, `RBS_MEASURE_UI=5`, 4 settings x 3 rounds (12 runs, none hung). Settings: `experience = "pro"`, `first_run_done = true` (past the first-run page, every panel shown). Median over 3 rounds; render time in ms (p50 / p95 / p99 / max).

| effects | hover (frames) | hover | sweep (frames) | sweep | redraw | resize |
|---|---|---|---|---|---|---|
| calm | 22 | 2.14 / 2.95 / 3.04 / 3.04 | 40 | 2.55 / 3.05 / 3.25 / 3.25 | 1.17 / 2.61 / 2.80 / 3.05 | 0.70 / 0.85 / 1.00 / 1.51 |
| glow | 23 | 2.48 / 3.14 / 3.41 / 3.41 | 42 | 2.69 / 3.44 / 3.64 / 3.64 | 1.30 / 2.76 / 2.99 / 3.18 | 0.78 / 0.93 / 1.08 / 1.73 |
| glass | 21 | 2.29 / 3.35 / 3.72 / 3.72 | 40 | 2.92 / 3.68 / 4.15 / 4.15 | 1.36 / 2.90 / 3.03 / 3.29 | 0.79 / 0.95 / 1.09 / 1.80 |
| tactile | 22 | 2.35 / 3.35 / 3.54 / 3.54 | 40 | 2.89 / 3.62 / 3.98 / 3.98 | 1.35 / 2.90 / 3.06 / 3.23 | 0.81 / 1.00 / 1.07 / 1.79 |

- Idle: 0 frames in all 12 runs.
- Deltas vs calm (p95 / max): hover glow +0.19 / +0.37, glass +0.40 / +0.68, tactile +0.40 / +0.50; sweep glow +0.39 / +0.39, glass +0.63 / +0.90, tactile +0.57 / +0.73; redraw glow +0.15 / +0.13, glass +0.29 / +0.24, tactile +0.29 / +0.18.
- The sweep draws only on a hover change (about 40 frames per 5 s, 16 hover changes per pass), so it measures real hover frames, not a continuous stream. The earlier run drew 1 to 3 frames because it stood on the first-run page.
- Footprint, one sample per setting (hover/resize phase): calm 342 MB, glow 336 MB, glass 352 MB, tactile 369 MB; within noise.
- Shadow cache thrash would show as a jump in the redraw p95; it did not (+0.29 ms at most).

Result: the p95 gate (+0.5 ms) is exceeded only in the sweep, by glass (+0.63 ms) and tactile (+0.57 ms). The max gate (+2 ms) holds everywhere (at most +0.90 ms). The worst absolute frame is 4.15 ms against a 16.7 ms budget, and these frames occur only when a hover state changes. Owner decision 2026-11-15: accepted. Windows is still not measured (SC-PC freeze of 2026-11-14).
