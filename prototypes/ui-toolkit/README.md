# UI toolkit comparison prototypes

Throwaway prototypes behind [ADR-0022](../../docs/adr/0022-native-ui-toolkit.md). They compare Slint, iced and Qt (through `cxx-qt`) against the [toolkit requirements](../../docs/specs/ui-design-system.md#requirements-on-the-ui-toolkit). This directory is excluded from the product workspace and from CI; nothing here is product code.

## Layout

| Path | What |
| --- | --- |
| `model/` | `dock-model`: toolkit-neutral panel and workspace model (layout tree of splits and tab stacks, floating windows, TOML workspace files, splitter geometry). Shared unchanged by all three prototypes, so only toolkit glue differs. |
| `shared/engine.rs`, `shared/engine.wgsl` | Stand-in for the engine compositor: renders an animated program frame into a pool of two `wgpu` textures. Included with `include!` so each prototype compiles it against its toolkit's `wgpu` major version. |
| `slint-proto/` | Slint 1.18 (`renderer-femtovg-wgpu`, `unstable-wgpu-30`). |
| `iced-proto/` | iced 0.14 (wgpu 27 renderer, `shader` and `canvas` widgets, multi-window `daemon`). |
| `qt-proto/` | Qt 6.4 Qt Quick through `cxx-qt` 0.10. |
| `measure-build.sh`, `measure-runtime.sh`, `a11y-check.sh`, `a11y-dump.py` | Measurement scripts used for the tables below. |
| `measure-gate.sh` | ADR-0022 gate run for a machine with a GPU (macOS or Linux desktop): 1080p preview, spectrum and dense-path cases, measured from presented frames (`RBS_PROTO_MEASURE`). |

## Same feature set in every prototype

1. Layout from the workspace model: nested splits with tab stacks, tabs switchable.
2. Splitters resizable by dragging.
3. Close a panel from its tab; reopen it from the Panels toggles in the window frame.
4. Move a panel to the next tab stack.
5. Float a panel into its own OS window and dock it back.
6. Switch between built-in workspaces (Edit, Minimal); save and load the current workspace as TOML.
7. Program panel shows the engine's `wgpu` texture, animated at display rate.
8. Mixer panel draws a 64-band spectrum (bars plus a polyline) with the toolkit's own vector drawing, updated every frame. This answers the owner's question about rich custom graphics (equalizers, histograms, meters).
9. Live indicator in the window frame, always visible.
10. Animation timers run only while an animated panel is visible; hidden panels are not instantiated (ADR-0013).

Environment variables: `RBS_PROTO_SIZE=1920x1080` (engine output size), `RBS_PROTO_BANDS=<n>` (spectrum points), `RBS_PROTO_MEASURE=<seconds>` (measure presented frames after 3 s warm-up, print one `MEASURE` line, exit), `RBS_PROTO_WORKSPACE=minimal`, `RBS_PROTO_CLOSE=<panel ids, comma separated>`, `RBS_PROTO_DIR=<workspace directory>`, `RBS_PROTO_FPS=1` (print tick rate). On a machine without a GPU Slint needs `SLINT_WGPU_CPU=1`.

## Build

```sh
cd prototypes/ui-toolkit
cargo build --release            # Qt needs Qt 6 development packages (qmake6) and a C++ compiler
cargo test -p dock-model
```

## Results (2026-11-02)

Machine: cloud container, 4 vCPU, 15 GiB RAM, Ubuntu 24.04, **no GPU**. Rendering went through Mesa llvmpipe/lavapipe on Xvfb. Rust 1.97. Numbers that involve rendering load are therefore CPU rasterisation costs, not GPU costs; see "What these numbers do not show".

### Feature checklist

| | Slint | iced | Qt (cxx-qt) |
| --- | --- | --- | --- |
| Dock layout, splitter drag, close/reopen, move, float/dock, workspace switch, TOML save/load | Works | Works | Works |
| How the layout tree is rendered | Flattened to absolute rects in Rust: `.slint` forbids recursive components | Recursive `row`/`column` with `FillPortion` (natural fit) | Flattened rects in a `Repeater` (QML could recurse) |
| Multiple OS windows | Works (one component instance per window; globals are per window, so shared state must be pushed to each) | Works (`daemon`, view per window id) | Works (`Instantiator` of `Window`) |
| Program preview from engine texture | **Zero copy, same process only.** Engine creates the `wgpu` device and hands it to Slint (`WGPUConfiguration::Manual`); texture shown with `Image::try_from(wgpu::Texture)` | **Zero copy (same process)**, but iced creates the device and the engine must adopt it inside `shader::Pipeline::new`; there is no way to give iced an existing device. Colour was washed out because the RGBA texture was sampled into an sRGB surface without conversion (fixable) | **CPU copy.** Qt renders with its own graphics stack; frames go GPU -> buffer map -> `QImage` -> Qt texture. Zero copy would need per-platform native texture interop with Qt's RHI (not attempted) |
| `wgpu` major version | 29 or 30 (`unstable-wgpu-*` features, may change in minor releases). `rbs-gpu` is on wgpu 30, so the UI can reuse `rbs_gpu::interop` to import engine surfaces | 27 (fixed by iced). `rbs-gpu` is on 30, so `rbs_gpu::interop` cannot be reused; the UI-side import would be written again against wgpu 27 | Not applicable; import goes through Qt's RHI in C++ |
| Custom drawing for spectrum/meters | `Path` (SVG commands) plus `Rectangle` per band; works | `canvas` with cached geometry; works | Qt Quick `Shape` with `PathSvg`; works |
| Screen reader tree (AT-SPI, `a11y-check.sh`) | 41 nodes: buttons with accessible names, page tabs, sliders, labels (AccessKit) | **Nothing exposed** (no accessibility in iced 0.14) | 19 nodes: buttons, tabs, sliders. Plain `Text` + `MouseArea` controls need explicit `Accessible` properties; the prototype did not add them |
| Mixed Arabic/Hebrew/Latin text | Shaped and ordered correctly | Shaped and ordered correctly | Shaped and ordered correctly |
| Layout mirroring for right-to-left UI | Not built in as far as known; not tested | Not built in as far as known; not tested | `LayoutMirroring` built in; not tested |
| Idle CPU with no animated panel visible | 0.0 % | 0.0 % | 0.0 % |

### Build and size

From `./measure-build.sh` (each prototype in its own empty target directory, 4 parallel jobs, release, stripped):

| | Slint | iced | Qt (cxx-qt) |
| --- | --- | --- | --- |
| Clean release build | 256 s | 142 s | 173 s (plus a Qt installation) |
| Rebuild after editing the UI file | 7.5 s | 7.2 s | 7.3 s |
| Binary size | 22.6 MiB | 13.1 MiB | 7.0 MiB + 28 MiB of Qt 6 libraries + QML modules |
| Crates in dependency tree | 372 | 204 | 108 |
| Linked shared libraries | 13 (fontconfig, freetype, ...) | 5 | 87 (Qt, ICU, GLib, D-Bus, Kerberos, curl, ...) |
| Minimum Rust version of the toolkit | 1.92 (workspace is on 1.87) | 1.88 | 1.85 |
| Glue code for the identical feature set | 255 Rust + 186 `.slint` lines | 435 Rust lines | 426 Rust + 155 QML lines |

### Runtime on a software rasteriser

From `./measure-runtime.sh 10` (after 4 s warm-up, 10 s sample; CPU as % of one core):

| Prototype | Scenario | CPU % | RSS MiB | Threads | Ticks/s |
| --- | --- | --- | --- | --- | --- |
| Slint | idle | 0.0 | 189 | 24 | - |
| Slint | spectrum only | 116.7 | 201 | 24 | 7 |
| Slint | program preview only | 119.6 | 195 | 24 | 6 |
| iced | idle | 0.0 | 143 | 27 | - |
| iced | spectrum only | 327.5 | 170 | 27 | 57 |
| iced | program preview only | 294.9 | 155 | 27 | 130 |
| Qt | idle | 0.0 | 164 | 14 | - |
| Qt | spectrum only | 68.8 | 165 | 14 | 62 |
| Qt | program preview only | 156.6 | 231 | 37 | 62 |

### What these numbers do not show

- **Load numbers are not GPU numbers, and the script is not yet the acceptance gate.** It counts timer ticks, renders 1280x720 and forces CPU adapters; ADR-0022 requires extending it (GPU, 1080p, presented-frame timestamps) before the gate run. With llvmpipe every GPU draw runs on the CPU. Slint's `femtovg-wgpu` renderer managed only 6-7 frames per second here and Qt's OpenGL scene graph about 62; this says how each renderer behaves on a CPU rasteriser, not on a GPU, but Slint's result stays an open risk until measured on a GPU. iced's frame subscription is not vsync limited on Xvfb (130 ticks/s), so its CPU is not comparable per frame either. **The load scenarios must be re-run on real GPU hardware (reference platform) before ADR-0022 is accepted.**
- RSS is dominated by Mesa's software drivers; compare differences, not absolutes.
- Accessibility was checked on Linux only (AT-SPI). Windows (UIA) and macOS (NSAccessibility) are not tested.
- Engine and UI ran in one process. The default two-process topology (ADR-0008) needs native surface export and import, which was not prototyped.
- Docking was implemented with buttons for move and float; drag-and-drop docking with drop indicators was not built in any prototype.

## Gate run on a GPU (2026-11-02)

`./measure-gate.sh 20` on a MacBook Pro (Mac16,8), Apple M4 Pro (16-core GPU, Metal), 24 GB, macOS 26.7.1, built-in ProMotion display (refresh rate during the run not verified), on AC power. Qt was not installed, so it was skipped; iced has no presented-frame hook and was measured idle only. CPU is the whole process (all threads) as % of one core, from `getrusage` over the measured window; it includes the engine stand-in, which renders a 1080p frame every tick in the same process.

| Prototype | Scenario | fps | Late frames | p50 / p99 / max interval ms | CPU % | Peak RSS MiB |
| --- | --- | --- | --- | --- | --- | --- |
| Slint | idle, no animated panel | - | - | - | 0.0 | 108 |
| Slint | 1080p preview + 64-band spectrum | 61.5 | 0 | 16.7 / 17.3 / 18.2 | 35.3 | 110 |
| Slint | 1080p preview only | 61.4 | 0 | 16.7 / 17.3 / 21.0 | 35.0 | 110 |
| Slint | dense path, 1024 points | 60.5 | 0 | 16.7 / 17.4 / 18.3 | 33.4 | 111 |
| Slint | dense path, 4096 points | 64.5 | 0 | 16.6 / 17.4 / 17.8 | 44.7 | 115 |
| iced | idle, no animated panel | - | - | - | 0.1 | 101 |

Reading: Slint keeps a steady 60 Hz with no late frames, and the 6 to 7 fps seen on the software rasteriser was a property of software rendering. Dense paths cost little extra up to 1,024 points. CPU use while animating (33 to 45 % of one core) is above the provisional 10 % limit. Where it goes (full-window redraw at Retina resolution, CPU-side path tessellation, or the in-process engine stand-in) is not yet known and needs a profile.

### Profile and renderer comparison (2026-11-02, same Mac)

A `sample` profile of the femtovg build (1080p preview, 10 s at 1 ms, steady state) showed about 29 % of the main thread busy, and about 82 % of the drawing time inside `wgpu_core::Global::device_create_render_pipeline` (naga validation and MSL translation, Metal `newLibraryWithSource`) called from femtovg's `CommandToPipelineAndBindGroupMapper::update_renderpass`. femtovg 0.27's wgpu renderer keeps a pipeline cache but drops every entry not used in the current flush (`pipeline_cache.retain(.. accessed ..)` after each command buffer), so pipelines appear to be rebuilt during steady redraw. Turning the engine stand-in off saved only about 2 percentage points.

`measure-gate.sh` now also builds the Slint prototype with the Skia renderer (`--features skia`, `RBS_PROTO_RENDERER=skia`), which also supports wgpu texture import:

| Scenario | femtovg-wgpu CPU % | Skia CPU % | Late frames (both) |
| --- | --- | --- | --- |
| idle | 0.1 | 0.1 | - |
| 1080p preview + 64-band spectrum | 35.6 | 26.9 | 0 |
| 1080p preview only | 34.9 | 22.9 | 0 |
| 1080p preview only, engine idle | 33.7 | 19.9 | 0 |
| dense path, 1024 points | 33.6 | 31.4 | 0 |
| dense path, 4096 points | 41.1 | 40.3 | 0 |
| Peak RSS MiB | 109-115 | 130-137 | |

With Skia the pipeline rebuilding is gone; about 13 % of the main thread is busy, spread over walking the item tree, text layout and drawing, the Skia flush and the timer tick, with no single hot spot. The remaining cost is redrawing the whole window at 60 Hz. Getting under the provisional 10 % would need redrawing less: only the animated regions, the preview as its own layer or window, or lower update rates for meters and text. None of these was measured.
