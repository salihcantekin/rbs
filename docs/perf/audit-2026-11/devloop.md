# RBS audit: developer loop and shipped binary

Auditor: devloop. Date: 2026-11-17. Repo: /home/user/rbs (rustc 1.97.0, cargo 1.97.0). No repo changes.

Scope note: partway through, the coordinator moved cloud build/test speed (environment, linker, debuginfo, caching, CI tiers) to another thread. The timings build (`cargo check --workspace --timings`, CARGO_BUILD_JOBS=3, CARGO_INCREMENTAL=0) was running at that point. It was stopped after about 540 units, while the Slint UI crates were compiling, and its target dir was deleted, so there are no timing numbers. The dev-loop findings below are a short list from static analysis. The deeper findings are about the shipped binary and runtime.

Verified facts used below:
- The Linux default linker is already rust-lld: a test binary's `.comment` section shows `Linker: LLD 22.1.6`.
- The repo has 237 integration-test files (`crates/*/tests/*.rs`); rbs-app has 35 of them.
- rbs-app has 20 example files (11 of them in rbs-app) and 49 bench files.
- The Linux graph holds 624 packages, 36 proc-macros and 95 build scripts.
- No tokio. rustls uses ring, not aws-lc-rs.

---

## Part A: runtime and shipped binary

### D-01 Generated Slint UI crates are built at opt-level 3 in release; `"s"` would shrink the binary
- Location: `Cargo.toml:30-34` (`[profile.release] lto = "thin"`, `codegen-units = 1`, `debug = "line-tables-only"`). There is no `[profile.release.package.rbs-app-ui-*]` override.
- Problem: about 62-70 MB of generated Rust (rbs-app-ui-base/main/settings/dialogs) is compiled with opt-level 3. That code is property bindings and tree building, which runs at UI-event rate, not frame rate. At opt-level 3 it is inlined and unrolled heavily. It is likely the largest Rust-side contributor to `rbs` text size after Skia.
- Proposal: add `opt-level = "s"` for the profile.release packages `rbs-app-ui-base`, `rbs-app-ui-main`, `rbs-app-ui-settings` and `rbs-app-ui-dialogs`. Optionally add `codegen-units = 16` for the same packages to cut release build time and peak memory. Check with `cargo bloat --release -p rbs-app --crates` before and after.
- Benefit: estimate. 20-35 % less code from those crates, perhaps 5-15 MB off a release binary. "s" usually cuts 20-35 % compared with O3 on generated straight-line code. There should be no measurable runtime cost, because the UI code is not on a hot path (docs/architecture/hot-paths.md). As a side effect, the release build is faster and peaks lower in RAM.
- Effort: S. Risk: Low. Profile-only change, reversible. One thing to verify: whether thin LTO keeps the per-package opt-level for those modules. Measure the size directly.
- Kind: quick-win

### D-02 Windows ships and initialises two wgpu backends (Vulkan and DX12)
- Location: `crates/rbs-gpu/Cargo.toml:17` (and `rbs-filters:18`, `rbs-nodes:34`): `features = ["std","parking_lot","dx12","metal","vulkan","wgsl"]`. `crates/rbs-gpu/src/context.rs:124-126` uses `Backends::PRIMARY` for the instance and for enumeration. `backend_rank` (context.rs:397) prefers Dx12 on Windows only as a tie-breaker.
- Problem: in wgpu 30 the `vulkan` feature is active on Windows (`wgpu-core-deps-windows-linux-android`), so the Windows exe contains the Vulkan HAL, ash and naga SPIR-V output next to DX12. That costs binary size. Every launch also creates a Vulkan instance and enumerates its adapters. That loads `vulkan-1.dll`, the ICDs and every implicit layer (overlays, capture tools) before DX12 is picked. Implicit layers cost time and are a known source of crashes. The extra Vulkan device is never used: interop (D3D11/MF shared handles) needs DX12.
- Proposal: (a) at runtime on Windows, create the instance with `Backends::DX12` only. Fall back to `Backends::VULKAN` only when DX12 gives `NoAdapter`. (b) Optionally make `vulkan` a Linux-only (non-Windows) feature through `[target.'cfg(not(windows))'.dependencies] wgpu = {features=[..,"vulkan"]}`, if Vulkan as a fallback on Windows is not wanted. Either way, record the decision in the GPU spec.
- Benefit: estimate. (a) Launch is 20-300 ms faster on Windows. Vulkan loader plus layer start-up is typically tens of ms and much more with heavy implicit layers. (b) The binary is about 1-3 MB smaller (wgpu-hal vulkan, ash, naga spv-out).
- Effort: S for (a), S for (b). Risk: Low/Med. A machine with a broken DX12 driver but working Vulkan loses Vulkan with (b), and keeps it with (a).
- Kind: quick-win

### D-03 Release on Linux carries line tables inside the shipped binary
- Location: `Cargo.toml:33` `debug = "line-tables-only"`. No `split-debuginfo` and no `strip`.
- Problem: rbs-crash (`crates/rbs-crash/src/lib.rs:267`, `Backtrace::force_capture`) symbolises in-process, so the line tables are currently needed at run time. On Linux they sit inside the ELF: a bigger download and a bigger mapped file. Windows already puts them in a PDB and macOS in a dSYM or object files, so this is Linux only.
- Proposal: short term, for Linux packages, keep the release profile and objcopy `.debug_*` into a side file plus `--add-gnu-debuglink` at packaging time. Write crash reports with raw addresses and module base as well as the symbolised text, so stripped builds can be symbolised offline. Longer term, `strip = "debuginfo"` with uploaded symbols.
- Benefit: estimate. Line tables are usually 10-25 % of a Rust release ELF, so about 5-20 MB on a binary that is likely around 60-100 MB.
- Effort: M. Risk: Med. Crash reports from stripped builds lose file:line unless offline symbolisation exists.
- Kind: structural

### D-04 No persistent wgpu pipeline cache (Vulkan first-use stalls)
- Location: no `PipelineCache` or `create_pipeline_cache` anywhere in `crates/*/src`. There are 7 `create_shader_module` and 7 pipeline-creation call sites.
- Problem: on Vulkan (Linux, and Windows fallback) every launch recompiles pipelines in the driver. The first program frame, the first filter use and the first transition each pay the driver's compile time. On Mesa that is tens to hundreds of ms per pipeline.
- Proposal: when the adapter has `Features::PIPELINE_CACHE`, create one cache per adapter. Load it from the user cache directory, keyed by `wgpu::util::pipeline_cache_key(adapter_info)`, and save it on clean exit. Zero cost when the feature is absent (P13).
- Benefit: estimate. Shorter first-frame and first-use hitches on Vulkan, typically 50-500 ms in total at startup. Needs measuring with `RBS` start-to-first-frame.
- Effort: M. Risk: Low. wgpu validates the cache key and drops a stale cache.
- Kind: structural

### D-05 Windows: Media Foundation and other DLLs are bound at process load
- Location: `crates/rbs-app/Cargo.toml` (Windows deps `rbs-capture-mf`, `rbs-media-mf`, `rbs-codec-mf`, `rbs-vcam-mf`, `rbs-audio-io-wasapi`). The `windows` 0.62 crate imports through windows-link (raw-dylib), so these DLLs sit in the exe's import table.
- Problem: `mfplat.dll`, `mfreadwrite.dll`, `mf.dll`, `ksuser`/`mfsensorgroup`-style DLLs and their dependencies are mapped and initialised on every launch, even when no MF source is used. That conflicts with the zero-cost rule (P13) and adds startup time and private memory. HW codecs are already dlopened (`crates/rbs-codec-{nvenc,amf,qsv,vaapi}/src/loader.rs`), which is good.
- Proposal: measure the import list first (`dumpbin /dependents rbs.exe`). Then delay-load the MF DLLs with `-C link-arg=/DELAYLOAD:mfplat.dll` (plus `delayimp.lib`) on the Windows target in `rbs-app/build.rs`. Check that raw-dylib imports honour /DELAYLOAD; if they do not, load MF through `LoadLibraryW` the way the codec loaders do.
- Benefit: estimate. Startup 10-40 ms faster and 2-8 MB less private working set when no MF source is active.
- Effort: M. Risk: Med. Delay-load failure turns into an SEH exception at first call unless a hook is installed.
- Kind: structural

### D-06 Linux: PipeWire and libdbus are hard load-time dependencies
- Location: `crates/rbs-capture-pipewire/Cargo.toml:22-24,30-31` and `crates/rbs-audio-io-pipewire/Cargo.toml:19,29`. `pipewire` 0.10 links libpipewire-0.3 and libspa through system-deps; `dbus` links libdbus-1.
- Problem: these become DT_NEEDED of `rbs`. The app cannot start on a system without the PipeWire client library, and every launch loads libpipewire, its modules' dependencies and libdbus, even when no capture or audio device is opened. The engine already dlopens HW codecs and FFmpeg (`rbs-codec-ffmpeg/src/linux.rs`).
- Proposal: measure with `ldd target/release/rbs` and `LD_DEBUG=statistics`. Medium term, either generate the FFI with dlopen (bindgen `--dynamic-loading` for the few PipeWire entry points used) or link with `-Wl,--as-needed` plus lazy binding. Or put the adapters behind an ADR'd optional runtime load, the same pattern as the codecs.
- Benefit: estimate. A few ms of startup and a few MB of mapped libraries. The main gain is robustness: the app starts without PipeWire.
- Effort: L. Risk: Med. FFI surface changes in adapter crates; needs the add-backend procedure.
- Kind: architectural

### D-07 Skia carries GL, Vulkan, PDF and JPEG-encode backends on every platform
- Location: Slint pulls them in, so they are not set in RBS Cargo.toml files. `cargo tree -e features -i skia-bindings` on Linux shows `binary-cache, embed-icudtl, ganesh, gl, jpeg, jpeg-decode, jpeg-encode, pdf, vulkan`. On Windows it shows `d3d` plus `gl`. The source is `i-slint-renderer-skia-1.18.1/Cargo.toml:133-135,174-205` (skia-safe with default features, so `pdf` and `embed-icudtl` are on).
- Problem: the prebuilt Skia static library is the biggest single size driver of `rbs` (estimate: 15-30 MB before gc-sections). The linker drops unreferenced sections, but Slint references GL and Vulkan (or D3D) context code, so both GPU backends of Skia ship. RBS uses one of them per platform.
- Proposal: (a) Measure with `cargo bloat --release --crates` and a symbol-size dump of `skia` objects. (b) Ask Slint upstream for `default-features = false` on skia-safe and for opt-out of GL when a wgpu or Vulkan surface is used. (c) Longer term (ADR-0022 decision 1 is Skia): compare binary size and RAM against Slint's femtovg-wgpu renderer, which shares RBS's own wgpu device, so there is one GPU API and one driver stack per process.
- Benefit: estimate. (b) 1-5 MB. (c) 10-25 MB of binary and one fewer GPU driver stack in RAM (tens of MB RSS on some drivers). Only measurement can confirm.
- Effort: (b) S (upstream), (c) L. Risk: (c) High: renderer change with visual and effect differences (ADR-0120 shadows need Skia).
- Kind: architectural

### D-08 Duplicate crates in the shipped binary (small; mostly winit 0.30)
- Location: `cargo tree -d -p rbs-app -e normal`. Runtime-relevant duplicates:
  - Linux: `rustix` 0.38 and 1.1 (plus `linux-raw-sys` 0.4 and 0.12), `tiny-skia` 0.11 and 0.12 (`sctk-adwaita` Wayland CSD), `thiserror` 1 and 2, `smol_str` 0.2 and 0.3, `getrandom` 0.2 and 0.3 (ring).
  - Windows: `windows-sys` 0.52, 0.59, 0.60 and 0.61, `keyboard-types` 0.7 and 0.8.
  - macOS: `objc2` 0.5 and 0.6, `objc2-foundation` and `objc2-app-kit` 0.2 and 0.3, `block2`, `core-foundation` 0.9 and 0.10, `bitflags` 1 and 2.
  - The rest (`syn` 2 and 3, `toml` 0.8, 0.9 and 1.1, `hashbrown` 0.14, `base64` 0.23, `miniz_oxide` 0.9) are host-only: build scripts and proc-macros.
- Problem: almost everything comes from winit 0.30 (old rustix, objc2 0.5, sctk-adwaita), which Slint 1.18 pins. The macOS objc2 0.5 and 0.6 split is the largest of these.
- Proposal: no local action. When Slint moves to winit 0.31, upgrade together (the Slint pin is exact, ADR-0022 decision 5). On Linux, consider whether Wayland client-side decorations (`wayland-csd-adwaita`, which brings tiny-skia 0.11) are needed, since RBS draws its own title bar (ADR-0071). Slint exposes no switch today, so this is upstream.
- Benefit: estimate. 0.5-2 MB on Linux, 1-3 MB on macOS.
- Effort: S once upstream moves. Risk: Low.
- Kind: structural

### D-09 No `target-cpu` baseline; x86-64-v2 is safe for the supported OS set
- Location: no `.cargo/config.toml` `[target.*] rustflags` and no `target-feature` use in crates (no `is_x86_feature_detected`/`std::arch` anywhere).
- Problem: x86_64 builds target the 2003 baseline (SSE2), so autovectorised loops cannot use SSE4.1 or SSSE3. That covers the CPU-side loops: audio DSP (`rbs-audio`, `rbs-audio-filters`, `rbs-denoise`), the YUV download path (ADR-0038) and the mux checksums.
- Proposal: set `-C target-cpu=x86-64-v2` for release on x86_64 Windows and Linux. Windows 11 24H2 already requires SSE4.2 and POPCNT. For AVX2 gains, add runtime dispatch only in measured hot kernels; never make v3 global. Requires the measure-performance and hot-path-review steps, and an ADR or spec line on the minimum CPU.
- Benefit: estimate. 0-10 % on CPU-bound DSP and convert loops, zero elsewhere.
- Effort: S. Risk: Low/Med. Drops pre-2009 CPUs, which must be a written product decision.
- Kind: quick-win

### D-10 Allocator: system default everywhere; not measured
- Location: no `#[global_allocator]` in any crate.
- Problem: hot paths are allocation-free by rule (P6), but the UI thread (Slint models, Skia paths, string formatting of meters and labels) and control-API JSON allocate a lot. On Windows the process heap is notably slower under multi-thread contention than mimalloc.
- Proposal: measurement only. Build `rbs` with mimalloc (feature-gated, composition root only) and compare CPU and RSS under the measure-performance scenario on Windows and Linux. Adopt only if CPU drops without RSS growth (lean goal, CLAUDE.md rule 17).
- Benefit: unknown. Typically 0-5 % UI-thread CPU, with an RSS risk of +2-10 MB.
- Effort: S to try. Risk: Low (composition root only).
- Kind: quick-win (experiment)

### D-11 Release profile: `codegen-units = 1` applies to every crate
- Location: `Cargo.toml:30-34`.
- Problem: cgu=1 plus thin LTO is good for the engine crates. For the 62 MB of generated UI it mostly makes release builds slow and memory-heavy (single-threaded LLVM on huge modules) for no runtime gain.
- Proposal: keep cgu=1 for the default package set. Add `codegen-units = 16` per package for the `rbs-app-ui-*` crates (pairs with D-01). Optionally add a `[profile.profiling] inherits = "release"` with `codegen-units = 16` and `lto = false` for measure-performance iteration.
- Benefit: estimate. Release build several minutes faster and peak RSS halved for the UI crates; runtime unchanged.
- Effort: S. Risk: Low.
- Kind: quick-win

---

## Part B: developer loop (short list; deep dive moved to another thread)

### D-12 Test-only features switch the whole workspace to a second build
- Location: `crates/rbs-core/Cargo.toml:18` `conformance = []`, reached through `rbs-codec`, `rbs-mux`, `rbs-transport`, `rbs-audio-io` and `rbs-api` `conformance` features. About 35 adapter crates enable these in `[dev-dependencies]`. Also `crates/rbs-app/Cargo.toml` dev-deps `rbs-app = { features = ["fault-injection"] }`, `rbs-app-ui = { features = ["probes"] }` and `rbs-codec-mf = { features = ["testing"] }`.
- Problem: with resolver 2, `cargo test` and nextest unify dev-dep features, so `rbs-core` is built with `conformance`. `cargo build`, `cargo run` and `xtask ui-dev` build it without. rbs-core is the root of every engine crate, so alternating `cargo run -p rbs-app` and `cargo test` rebuilds the whole in-house graph, including the rbs-app lib, and keeps two copies on disk. `-p` subsets (`xtask affected`) produce further variants.
- Proposal: move conformance kits into their own crates, as was done for `rbs-*-test` (for example `rbs-codec-conformance`), or make them always compiled (`#[doc(hidden)]`, dead code stripped). Gate `fault-injection` behind a runtime test hook rather than a lib feature.
- Benefit: estimate. Removes a full rebuild of about 85 workspace crates when switching run↔test (minutes), and several GB of duplicate artifacts.
- Effort: M. Risk: Low.
- Kind: structural

### D-13 One `rbs-ui` edit regenerates all Slint UI crates
- Location: `crates/rbs-app-ui*/build.rs` use `rbs-ui` (21.5k lines: layout, workspace, store and more) as a `[build-dependencies]` only for `rbs_ui::tokens_slint`. `slint-build-1.18.1/lib.rs:605` always `File::create`s the generated file.
- Problem: any rbs-ui source edit recompiles the build scripts, which reruns Slint codegen, which rewrites the generated `.rs` with a new mtime. All UI crates then rebuild even when the output is byte-identical: the 99-381 s path.
- Proposal: split `tokens`/`tokens_slint` into a small `rbs-ui-tokens` crate used as the build-dependency. Also, in the build scripts, generate to a temp path and copy only when the content changed (`compile_with_output_path` + compare).
- Benefit: estimate. 100-380 s saved on every rbs-ui edit and every no-op build-script rerun.
- Effort: S/M. Risk: Low.
- Kind: quick-win

### D-14 rbs-app links about 48 near-identical huge test and example binaries
- Location: `crates/rbs-app/tests/` has 35 files. `crates/rbs-app/examples/` has 11 files, which `cargo test` and nextest build and link even though they never run. The bin target `src/main.rs` holds 33.6k lines of bin-only modules and 161 unit tests, so it links a second app-sized test harness.
- Problem: each binary is 230-560 MB and mostly link time, and all of them re-link after any lib edit.
- Proposal: merge the integration tests into 1-3 binaries (`tests/studio/main.rs` with `mod` files; nextest keeps process isolation). Gate the examples with `required-features = ["dev-examples"]` or move them out. Move the bin-only modules into the lib so the bin test harness is trivial (`[[bin]] test = false`).
- Benefit: estimate. About 40 fewer 400 MB links: about 15 GB less disk and roughly 30-90 s less per incremental test run on 4 cores. Same pattern for rbs-gpu (22 test files) and rbs-record (18).
- Effort: M. Risk: Low/Med. Plain `cargo test` would share a process for tests that set globals; 18 files touch `static`, `OnceLock` or `set_var`.
- Kind: structural

### D-15 Nextest GPU groups throttle CPU-only tests (worst on Windows)
- Location: `.config/nextest.toml`. The overrides put the whole `package(rbs-app)` (532 tests, mostly CPU-only), `rbs-record` and the others into `gpu-windows` (max 2) or `gpu-linux` (max 4).
- Problem: on the Windows dev machine every rbs-app test runs 2 at a time, whether or not it opens a device. On machines with more than 4 cores, the Linux cap of 4 also idles cores.
- Proposal: narrow the filters to the binaries or tests that open a device (for example `binary(effects_render) | test(/gpu/)`, or a naming convention), and leave the rest ungrouped.
- Benefit: estimate. A large share of the Windows test wall time for rbs-app and rbs-record (they were "the whole Windows run").
- Effort: S/M. Risk: Low/Med. A missed GPU test can share the device again (the documented WARP slowdown).
- Kind: quick-win

### D-16 Smaller dev-loop items
- Nextest never runs doctests. CI and `affected test` use nextest, so the roughly 6 doctests in 12 crates never run. Either set `doctest = false` and turn them into unit tests, or add `cargo test --doc`. Edition 2024 already merges doctests.
- `criterion` 0.7 (rbs-gpu, rbs-filters) and 0.8 (everyone else) are both compiled. Align them on 0.8.
- `cargo build --workspace` builds `rbs-app-ui-probes` (codegen of test-only windows) because it is a workspace member. Consider `default-members` that leave out probes, single and xtask.
- No `[workspace.dependencies]`: about 90 crates repeat versions and features by hand (slint feature lists ×8, wgpu ×4). One table removes feature-mismatch risk. For example, rbs-app's slint list adds `unstable-winit-030`, so a `-p rbs-app-ui-*` build compiles Slint with a different feature set.
- `xtask affected test` always runs an extra cold-capable `cargo check -p rbs-app --features ui-live-preview` in `target/live-preview` when any UI crate is selected, and the selection is All for root changes. That second target dir is a full second dependency build (GB).
- Windows: point `linker` to `rust-lld` for `x86_64-pc-windows-msvc` in `.cargo/config.toml`. link.exe is slow on 400 MB debug binaries. Linux already uses rust-lld by default (verified).
- Dev test speed: `[profile.dev.package.naga]`, `wgpu-core` and `png` at opt-level 2 would speed up 961 GPU tests that each validate and compile WGSL in debug naga (estimate; measure with nextest junit).

---

## Already good
- Linux links with rust-lld by default (rustc 1.97). Dev debug info is trimmed (`line-tables-only`, deps `debug=false`, UI crates `debug=false`). `i-slint-compiler` and `rbs-audio` are optimised in dev.
- wgpu: no GLES or WebGPU, and Metal/DX12 are target-gated by wgpu itself. Only one wgpu version (30.0.1) is in the lock, and Slint's skia renderer reuses it (`unstable-wgpu-30`).
- No tokio. rustls uses ring (lighter than aws-lc-rs).
- HW codecs and FFmpeg are dlopened at runtime (`loader.rs`, `rbs-codec-ffmpeg/src/linux.rs`).
- Secondary windows (Settings, Source settings, Sync measure, Attributes, Float) are created lazily (`main.rs:1958, 3091, 4361, 4447, 4501`). The GPU context is created on the studio thread, not the UI thread (`main.rs:5899`). The system font scan is lazy (`look.rs:43-70`).
- Test-only code is cfg-gated: `fault-injection` (10 cfg sites), `rbs_core::conformance`, `look/drawn.rs` (`#[cfg(test)]`). Embedded runtime assets are tiny: an 87 KB RNN weights file and a 75 KB macOS icon.
- `panic = "unwind"` (the default) is required by rbs-crash `catch_unwind` (ADR-0055), so it is correctly kept.
- Every first-party build.rs prints `rerun-if-changed`. The token generator preserves mtime. Slint-build emits per-file rerun lines.
- Release already uses thin LTO and cgu=1 for the engine crates.
- The ADR-0138 split and `xtask affected` are in place. Edition 2024 merges doctests.

## Open questions / measurements needed
1. Release size breakdown. Run `cargo bloat --release -p rbs-app --bin rbs --crates -n 40` on Linux and Windows to rank Skia, the UI crates, wgpu, naga, accesskit/zbus and ring. This decides D-01, D-07 and D-08. Not run: a release build with cgu=1 and LTO takes an estimated 20-40 min and 8+ GB here.
2. D-01: does thin LTO keep per-package `opt-level = "s"`? Measure binary size and UI-thread CPU before and after.
3. D-02: Windows launch time with `Backends::PRIMARY` versus `DX12` only (instance creation to first frame), on a machine with implicit Vulkan layers installed (Steam, OBS).
4. D-05 and D-06: `dumpbin /dependents rbs.exe` and `ldd rbs` to list load-time DLLs and .so files. Check whether raw-dylib imports honour `/DELAYLOAD`.
5. Linux accessibility (`accesskit_unix` + zbus): does it open a D-Bus connection or thread at startup even without an AT client? That would be a P13 cost.
6. Skia GL versus Vulkan at runtime on Linux: which context Slint uses with `unstable-wgpu-30`, and whether both drivers get loaded.
7. The timings build was stopped before the UI crates finished; there are no critical-path numbers from this audit.
