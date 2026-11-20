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

Recording protection (ADR-0090, [spec](recording-protection.md)) costs nothing while no recording or stream runs: no thread, timer or polling. During a take it adds one probe thread and one guard step a second on the control thread (integer arithmetic over at most four outputs and the volumes they write to), and one free-space and memory reading every 5 s on the probe thread. It reads existing counters only, so it adds nothing to any hot path.

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
| Audio input node (read, steer, resample, map channels), one source | Time per 10 ms block, reference VM (`cargo bench -p rbs-audio --bench nodes`) | at most 50 µs (measured 27 µs at 48 kHz mono, 35 µs at 44.1 kHz stereo) |
| Audio output node (FIFO, steer, resample, map channels, volume, ring push), one output | Time per 10 ms block, reference VM (`cargo bench -p rbs-audio --bench nodes`) | at most 50 µs (measured 32 µs to 48 kHz and 30 µs to 44.1 kHz stereo on the cloud development container) |
| Mixer, 16 stereo inputs with meters | Time per 10 ms block, same bench | at most 30 µs (measured 13 µs; with 4 monitored and the monitor read 14.6 µs against 11.5 µs unmonitored on the cloud development container) |
| Static scene (stills and text only), 1080p60 recording, render on change (R-03) | Compositor draws and export conversions per second after the first | 0 (`CompositorStats::drawn`, `DownloadStats::converted`); the recording still has 60 frames per second (`ExportNodeStats::repeated`) |
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

### Responsiveness

Owner decision 2026-11-20 (stability plan WP-I), from the measurements below. Checked with the measure mode (`RBS_MEASURE_UI`, release build; `cargo xtask device-check app --release` runs it for about a minute on a Mac or Windows machine).

| Scenario | Metric | Target |
| --- | --- | --- |
| Any measure phase | UI render time (`render_ms`) | p99 at most 8 ms |
| Hover, resize and sweep phases | Input to the end of the next frame, the app's share (`input_ms`) | p95 at most 16 ms (one 60 Hz refresh), max at most 33 ms |
| Every measure phase | Long tasks: UI entries of 50 ms or more (`long_tasks`, `rbs_app::ui_thread::LONG_TASK`) | 0 |
| Start-up (`phase=warmup`) | Long tasks | at most 1, and at most 100 ms |
| Quit, nothing live | Click to every window hidden and the shutdown on its thread (`close_ms`) | at most 100 ms |
| Browser source in Interact | Pointer move to the page's frame shown | at most 50 ms (target; measured in stability plan WP-J) |
| Add a source | Click to its first frame in the preview | at most 500 ms (target; measurement open) |

`input_ms` starts when the measure mode hands a pointer move or a size change to the window and ends when the next frame is drawn; the OS input queue before it and the compositor's presentation after it are not in it. A UI entry that waits on the disk counts against the long-task budget: one user data save (`cargo bench -p rbs-user-data --bench write`) is a known wait on the UI thread (docs/architecture/threads.md), so it must stay well under 50 ms on a local disk; the stall watch logs any entry of 100 ms or more.

Measured 2026-11-20 on the reference Mac (M4 Pro, macOS 26.7.1, 3024x1964 at 2x, wgpu), release build of `ed1c754`, the owner's scene collection (a copy), two runs of `device-check app --release --seconds 60`: render p99 at most 5.0 ms in every phase; `input_ms` p95 9.1-14.0 ms, max 17.8 ms; no long task in any phase, one at start-up (58-60 ms); `close_ms` 7.5-16.7 ms, process exit 89-102 ms after the last measure line. One user data save (collection of 9 or 64 KiB plus `current.toml`, each flushed to the disk) in `~/Library/Application Support`: p50 16.0 ms, p95 19.1-20.1 ms, max 23.2 ms; size makes no difference (the cost is `F_FULLFSYNC`). The cloud container (ext4) measures about 1 ms p50 for the same save.

Measured 2026-11-20 on the reference Windows PC (RTX 4090, Ryzen 9 7950X, Windows 11 Pro 26300, wgpu on Direct3D 12), same build and runs, over Remote Desktop (3600x2260 at 2x, no preview layer in a remote session): render p99 at most 2.0 ms; no long task in any phase or at start-up; `close_ms` 4.2-5.0 ms, process exit 544-570 ms after the last measure line. The remote session presents at about 32 fps (frame interval p50 31 ms), so `input_ms` is bound by it: hover and sweep p95 16.7-17.1 ms, resize p95 30.6 ms, max 40.2 ms. These do not test the input budget; that needs a run on the physical console. One user data save in `%APPDATA%\RBS`: p50 7.5-8.3 ms, p95 8.6-11.6 ms, max 12.4 ms.

Measured 2026-11-03 on the reference Mac (`remote_preview` with `RBS_DEMO_SECONDS`, ADR-0034): 1080p preview and 16 meters at 30 Hz, 9.2 % (one run; 720p: 8.3-8.8 % over two), 60 fps, 1 to 4 preview frames skipped by the engine per 20 s; 720p preview alone 2.0 %. Before ADR-0034, with Slint drawing the preview at 720p: 15.3 % with meters and 12.2 % without. With the meters' moving parts as native layers too ([ADR-0035](../adr/0035-meters-as-native-layers.md)): 2.4-2.7 % at 720p (four runs) and 2.5 % at 1080p (one run), against 8.4-9.2 % with Slint drawing them in the same sitting, 60 fps; with 128 meters 3.0-3.1 % against 13.5-13.7 %. The budget is met with a wide margin. WindowServer's share is not separated yet (its baseline on the measuring desktop is about 49 %).

Measured 2026-11-03 on the reference Windows PC (RTX 4090, physical console, `remote_preview`, 720p preview, [ADR-0036](../adr/0036-windows-preview-layer.md)): preview as a composition swapchain layer 1.3 %, with 16 meters 7.6 % (the meters are still drawn by Slint on Windows); Slint drawing the preview 5.0 %, with meters 9.5-10.2 %. 60 fps, 0 late frames. Native meter layers on Windows are the next gain.

Theme effects (ADR-0120 decision 13) are measured with the app's own measure mode: `RBS_MEASURE_UI=<seconds>` (release build) warms up 3 s, then runs an idle, a toolbar hover sweep and a window resize sweep phase of `<seconds>` each and prints frames, render time and frame interval (p50, p95, p99, max) per phase, plus input to frame, long tasks and `close_ms` ("Responsiveness" above); `RBS_EFFECTS=<calm|glow|glass|tactile>` picks the setting (ADR-0122; `vivid` still means Glow). Calm must be within noise (the spread of five baseline runs) of `main`; a lively setting is offered only if, on the iGPU and on the `skia-software` run, idle stays at 0.0 % CPU with no redraw timer, p95 rises by at most 0.5 ms and max by at most 2 ms, and GPU memory rises by at most 16 MB.

How it is reached: the preview and the meters' moving parts as native layers that the toolkit never redraws ([ADR-0034](../adr/0034-preview-as-native-layer.md), [ADR-0035](../adr/0035-meters-as-native-layers.md); Slint's GPU path cannot redraw only dirty regions), meters at 30 Hz and the live duration and stats at 1 Hz (rates are declared per panel in the [design system](ui-design-system.md#panel-model)), and no timer at all when nothing animates.

## Regression policy

- Micro-benchmarks: CI fails on > 5 % regression with statistical confidence (criterion comparison against `main`).
- Scenario benchmarks: nightly on self-hosted GPU runners; > 3 % regression opens an issue automatically and blocks release.
- A PR that intentionally trades performance must say so and link the measurement.
