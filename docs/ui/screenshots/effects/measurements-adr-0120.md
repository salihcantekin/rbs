# ADR-0120 effects: UI cost measurements (2026-11-14)

Gate per setting vs calm (ADR-0120 decision 13): idle 0 % extra CPU and 0 idle frames, render p95 rise <= 0.5 ms, max rise <= 2 ms, iGPU GPU memory rise <= 16 MB.

## Mac (M4 Pro, macOS 26.7.1, 3024x1964 @2x, 120 Hz), release, median of 5, resize phase (RBS_MEASURE_UI=10)

| effects | frames | render p50 | p95 | p99 | max |
|---|---|---|---|---|---|
| calm | 1197 | 0.82 | 1.00 | 1.14 | 1.68 |
| vivid | 1187 | 0.93 | 1.11 | 1.25 | 2.04 |
| tactile | 1135 | 0.89 | 1.08 | 1.23 | 2.26 |

Idle: 0 frames in all 15 runs. Idle CPU (no measure mode, 30 s): main 5.10 %, calm 5.01 %, vivid 5.23 %, tactile 4.84 % (same as main; the ~5 % is outside the UI). Footprint: calm 282 MB, vivid 292 MB, tactile 280 MB.
Result: PASS (p95 +0.11/+0.08 ms, max +0.36/+0.58 ms).

## Windows SC-PC (RTX 4090, wgpu on D3D12, 3440x1440 @1x), release, median of 3, RBS_MEASURE_UI=5

| effects | phase | frames | render p50/p95/p99/max |
|---|---|---|---|
| calm | resize | 600 | 1.08/1.33/1.59/2.02 |
| vivid | resize | 597 | 1.27/1.53/1.69/2.47 |
| tactile | resize | 600 | 1.22/1.47/1.71/2.26 |
| calm | hover | 19 | 0.87/1.03/1.12/1.12 |
| vivid | hover | 19 | 0.99/1.25/1.28/1.28 |
| tactile | hover | 19 | 1.05/1.24/1.35/1.35 |

Idle: 0 frames. Idle CPU (no measure mode, 16 s, of 32 cores): main 0.07-0.11 %, branch 0.04-0.09 %. Dedicated GPU memory 329.7-329.8 MB in every setting, main and branch alike.
Result: PASS (p95 +0.20/+0.14 ms, max +0.45/+0.24 ms).

Not measured: the AMD iGPU (the per-app GPU preference did not move rendering off the 4090; runs then paced at ~24 ms with 366.8 MB, render p50 calm 1.33 / vivid 1.50 / tactile 1.47 ms) and skia-software (`SLINT_BACKEND=winit-skia-software` was ignored; the app still used wgpu). The batch ended in an RTX driver error (nvlddmkm Event 14, no TDR) and a hard freeze; see the freeze note. Rerun only with a renderer choice that is verified to take effect and a batch that stops at the first hang.
