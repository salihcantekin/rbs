# Spec: Performance budgets

- Status: Draft
- Owner: whole project; enforced in CI
- Related ADRs: [0009](../adr/0009-performance-budgets-in-ci.md)

## Purpose

Turn "use memory, CPU and GPU efficiently" into numbers that CI can check.

## Hard rules (always enforced)

| Rule | How it is checked |
| --- | --- |
| No heap allocation on the audio thread or inside `Node::process` | Allocation guard in debug/test builds fails the test |
| No CPU readback of video frames on the default path when zero-copy is available | Metric `frames_cpu_copied` must be 0 in scenario tests |
| No blocking I/O or contended locks on real-time threads | Review checklist; lock contention metrics |
| Encoded packets fan out to many outputs without copying payloads | Micro-benchmark asserts zero payload copies |

## Idle cost

Principle P13 and [ADR-0013](../adr/0013-zero-cost-when-unused.md). All checks use the same full release binary, run twice on the same machine: once with a registry allowlist that registers only the adapters the reference scene uses and no plugins (baseline), once with every built-in adapter registered and 20 test plugins installed but not used (loaded). Rows marked *scene* run the reference scene; rows marked *idle* run the engine with no scene active.

| Check | Pass condition | Runs on |
| --- | --- | --- |
| Thread count (*scene* and *idle*) | Loaded equals baseline | Hosted CI |
| Wake-ups (*idle*): over 60 s, count thread wake-ups and syscalls | Loaded at most baseline + 5 of each over the 60 s | Hosted CI (Linux, `perf`/`strace` based) |
| Resident memory (*idle*) | Loaded minus baseline at most 64 KiB per registered or installed item | Hosted CI |
| Plugin binaries (*scene*) | No native library loaded and no WASM module compiled in the loaded run | Hosted CI |
| Engine CPU time over 5 min (*scene*) | Loaded at most 1 % above baseline | Self-hosted runner |
| GPU time per frame (*scene*) | Loaded equals baseline within 1 % | Self-hosted GPU runner |
| Activate then deactivate each feature | Threads, resident memory (within 64 KiB) and GPU allocations return to the pre-activation level | Hosted CI where possible |

"Hot loops contain no checks for inactive features" cannot be measured by a benchmark; it is a review item in the `hot-path-review` skill.

## Scenario budgets (initial targets, to be calibrated in Phase 0)

Measured on reference machines that will be listed here. Each target is compared with OBS on the same machine and scene.

| Scenario | Metric | Target |
| --- | --- | --- |
| 1080p60 display capture, one camera, two images, text, stream + record, hardware encoder | Engine CPU time | at or below OBS |
| Same | GPU time per frame | at or below OBS |
| Same | Resident memory after 10 min | at or below OBS |
| Same | Frames dropped by RBS (render or encode lag) over 4 h | 0 |
| Audio path, 8 sources, 4 buses | Audio thread load per 10 ms block | < 10 % of block time |
| Glass-to-encoder latency, display capture | Latency | < 2 frame intervals |
| Glass-to-delivery latency, display capture (OS capture to `latest_frame`) | Latency | <= 1 frame interval |
| Cold start to first preview frame | Time | < 1.5 s |
| Capture adapter `probe` (fastest of three calls) | Time | <= 50 ms (open: needs a probe benchmark; conformance only guards against a hang, ADR-0032) |
| Capture `latest_frame` while every frame is held | Time | does not wait for a frame release (open: needs a non-timing check or a benchmark; conformance guards at 500 ms, ADR-0032) |

## Desktop UI

The desktop UI (`rbs-app`) owns this budget (ADR-0022, owner decision 2026-11-02). Measured with the gate script in `prototypes/ui-toolkit` (later a UI scenario test) on the reference Mac, release build, presented-frame timestamps.

| Scenario | Metric | Target |
| --- | --- | --- |
| No visible panel animates, not live | Process CPU, wake-ups | 0.0 % and no redraw timer |
| 1080p preview and mixer meters visible and animating | Process CPU of the UI | at most 10 % of one core |
| Same | Late frames (interval over 1.5 refresh periods) | 0 over 60 s (open on the preview layer path: presentation is the window server's and needs Core Animation instrumentation, ADR-0034) |
| Workspace switch | Time to the first frame of the new layout | under 100 ms (U21) |

Measured 2026-11-03 on the reference Mac (`remote_preview` with `RBS_DEMO_SECONDS`, ADR-0034): 1080p preview and 16 meters at 30 Hz, 9.2 % (one run; 720p: 8.3-8.8 % over two), 60 fps, 1 to 4 preview frames skipped by the engine per 20 s; 720p preview alone 2.0 %. Before ADR-0034, with Slint drawing the preview at 720p: 15.3 % with meters and 12.2 % without. With the meters' moving parts as native layers too ([ADR-0035](../adr/0035-meters-as-native-layers.md)): 2.4-2.7 % at 720p (four runs) and 2.5 % at 1080p (one run), against 8.4-9.2 % with Slint drawing them in the same sitting, 60 fps; with 128 meters 3.0-3.1 % against 13.5-13.7 %. The budget is met with a wide margin. WindowServer's share is not separated yet (its baseline on the measuring desktop is about 49 %).

Measured 2026-11-03 on the reference Windows PC (RTX 4090, physical console, `remote_preview`, 720p preview, [ADR-0036](../adr/0036-windows-preview-layer.md)): preview as a composition swapchain layer 1.3 %, with 16 meters 7.6 % (the meters are still drawn by Slint on Windows); Slint drawing the preview 5.0 %, with meters 9.5-10.2 %. 60 fps, 0 late frames. Native meter layers on Windows are the next gain.

How it is reached: the preview and the meters' moving parts as native layers that the toolkit never redraws ([ADR-0034](../adr/0034-preview-as-native-layer.md), [ADR-0035](../adr/0035-meters-as-native-layers.md); Slint's GPU path cannot redraw only dirty regions), meters at 30 Hz and the live duration and stats at 1 Hz (rates are declared per panel in the [design system](ui-design-system.md#panel-model)), and no timer at all when nothing animates.

## Regression policy

- Micro-benchmarks: CI fails on > 5 % regression with statistical confidence (criterion comparison against `main`).
- Scenario benchmarks: nightly on self-hosted GPU runners; > 3 % regression opens an issue automatically and blocks release.
- A PR that intentionally trades performance must say so and link the measurement.
