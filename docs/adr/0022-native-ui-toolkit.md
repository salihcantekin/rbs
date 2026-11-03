# ADR-0022: Slint as the desktop UI toolkit

- Status: Proposed
- Date: 2026-11-02
- Deciders: Salih (owner)
- Builds on: ADR-0016 (native UI, toolkit chosen by prototype), ADR-0017 (flexible panels and workspaces), ADR-0010 (GPL-3.0-or-later), ADR-0008 (UI as a control API client)

## Context

ADR-0016 requires a native Rust UI toolkit and leaves the choice among Slint, iced and Qt (through `cxx-qt`) to a comparison prototype measured against the [toolkit requirements](../specs/ui-design-system.md#requirements-on-the-ui-toolkit). ADR-0017 adds a docking and workspace system to those requirements. The owner also asked whether the toolkit can draw rich custom graphics such as equalizers, histograms and meters.

Three prototypes with the same feature set were built in [`prototypes/ui-toolkit`](../../prototypes/ui-toolkit/README.md). All three share a toolkit-neutral layout model (splits, tab stacks, floating windows, TOML workspaces). Each implements splitter dragging, close and reopen, move, float and dock back, and workspace switching with save and load. Each also has an engine stand-in that renders the program output into `wgpu` textures, and a 64-band spectrum redrawn every frame with the toolkit's own vector drawing. The README holds the full tables and the measurement scripts.

The prototypes ran engine and UI in one process. ADR-0008 makes the desktop UI a client over local IPC, so the default topology is two processes. In that topology the engine exports program and preview surfaces as native handles (D3D shared handles, IOSurface, DMA-BUF), and the UI imports them into its own graphics device. That import is per platform for every toolkit. The prototype does not prove it.

| Requirement | Slint 1.18 | iced 0.14 | Qt 6 via cxx-qt 0.10 |
| --- | --- | --- | --- |
| 1. Preview without CPU copies | Shown in-process. External `wgpu` textures are displayed directly (`Image::try_from`), and the UI can also adopt an existing device | Shown in-process through a `shader` widget. iced creates its own device and cannot be given one | Not shown: the prototype copied frames through the CPU. Needs native texture import into Qt's RHI, written in C++ per backend |
| Reuse of `rbs_gpu::interop` (wgpu 30) for the UI-side import | Possible: same `wgpu` major (`unstable-wgpu-30`) | Not possible with wgpu 27: the import code must be written again against iced's `wgpu`, or iced must catch up | Not possible: separate C++ and RHI code |
| 2. Custom drawing at display rate | `Path` and primitives. Throughput is an open risk (see below) | `canvas` and `shader` widgets | Qt Quick `Shape` and the scene graph |
| 3. Accessibility | Screen reader tree with roles and names (AccessKit), seen on Linux AT-SPI only. Keyboard focus, Windows and macOS not tested | Nothing exposed to screen readers | Most mature. Custom controls need `Accessible` annotations |
| 4. Fluent i18n, RTL layout | Bidi text shaped correctly. No Fluent integration and no layout mirroring known: **open** | Bidi text correct. No mirroring known: **open** | Bidi text correct. Built-in `LayoutMirroring` |
| 5. Idle CPU | 0.0 % | 0.0 % | 0.0 % |
| 6. Docking | Built on our model; works. `.slint` forbids recursive components, so the layout is flattened in Rust | Built on our model; works | Built on our model; works |
| 7. Licence under ADR-0010 | Compatible through the GPL-3.0-only option | MIT, compatible | LGPL-3.0, compatible with dynamic linking |
| Build and distribution | 256 s clean build, 22.6 MiB binary, 372 crates, Rust 1.92 | 142 s, 13.1 MiB, 204 crates, Rust 1.88 | 173 s plus a Qt install, a 7 MiB binary plus about 28 MiB of Qt libraries, a C++ toolchain, 87 shared libraries |
| API stability | 1.x with semver, except the `unstable-wgpu-*` features | 0.x, breaking changes in every release | Qt is stable; `cxx-qt` is 0.x |

There was no GPU in the test machine, so the rendering-load numbers came from a software rasteriser. On it, Slint's `femtovg-wgpu` renderer reached 6 to 7 frames per second where Qt's scene graph reached about 62. This may be only a property of software rendering, or it may be a real cost, for example re-tessellating `Path` geometry every frame. It is an open risk until it is measured on a GPU.

## Decision

1. The RBS desktop UI uses **Slint**, under its GPL-3.0-only licence option, with its **Skia** renderer on the `wgpu` path (`renderer-skia`, `unstable-wgpu-30`). The `femtovg-wgpu` renderer was measured and rejected for now: it rebuilds render pipelines during steady redraw (see Review). Slint is chosen because it is the only candidate that has all three of the following:
   - working accessibility (iced has none),
   - direct display of `wgpu` textures on the same `wgpu` major as `rbs-gpu` today, so `rbs_gpu::interop` code can be reused,
   - a Rust-only build (Qt brings a C++ toolchain, its own RHI interop and more than 80 shared libraries).
2. Default topology is the one in ADR-0008: engine and UI in separate processes. The engine exports program, preview and projector surfaces as native handles. The UI imports them into its own `wgpu` device with the same `rbs_gpu::interop` code, and hands them to Slint as textures. There are no CPU copies on the default path (P4). Running both in one process with a shared device is an optional mode. The export side of this contract is not yet specified. It must be added to the [GPU compositor spec](../specs/gpu-compositor.md) before the UI crate shows engine output (spec first).
3. Rich visualisations follow two tiers, whichever toolkit is used:
   - Light, low-density graphics use the toolkit's vector primitives inside design-system components. Examples: audio meters, fader scales, equalizer curves, small charts.
   - Dense or per-pixel graphics are computed and drawn on the GPU by the engine or by a UI-side `wgpu` pass. The toolkit only composites the resulting textures. Examples: histograms, waveform and vectorscope, spectrum analysers with many bins, large meter bridges.
4. The panel, docking and workspace model lives in a toolkit-neutral crate, `rbs-ui`, and the Slint shell (`rbs-app`) only renders it. In the prototype this was `dock-model`. A separate crate lets Cargo enforce that it links no toolkit, and it is tested without a window. This limits the cost of replacing the toolkit.
5. The UI crate pins `slint` and `slint-build` to an exact version. While Slint supports the `wgpu` major used by `rbs-gpu`, the UI reuses `rbs_gpu::interop` for importing surfaces. `rbs-gpu` is never held back for the UI. If Slint lags behind a `wgpu` upgrade, the UI keeps importing surfaces with a fork of the import code built against Slint's `wgpu` major until Slint catches up. That is possible because the handles cross a process boundary, but it means duplicated platform code during the lag. This is the same drawback that counts against iced today, so the reuse advantage holds only while Slint keeps pace.
6. The UI crate sets its own `rust-version` (1.92 for Slint 1.18). Engine and port crates keep the workspace minimum.
7. **Acceptance gate.** This ADR becomes Accepted only when all of the following are true:
   - Frame pacing on a real GPU: met. On Apple M4 Pro the Slint prototype kept 60 Hz with 0 late frames in every scenario (1080p preview, 64-band spectrum, dense paths of 1,024 and 4,096 points) with both renderers. By owner decision (2026-11-02) Qt is not measured.
   - CPU: by owner decision (2026-11-02) the limit of at most 10 % of one CPU core while preview and meters animate is not a gate for this ADR. It becomes a budget of the UI crate, to be reached by redrawing less (only changed regions, the preview as its own layer or window, lower update rates for meters and text), and moves into the [performance budgets](../specs/performance-budgets.md) when the UI crate is created. The prototype with Skia used 20 to 27 % for 1080p preview and up to 40 % for a 4,096-point path.
   - Zero-copy display of an engine surface imported from another process has been shown on at least one platform.
   - Tabs, splitters and dock actions can be operated by keyboard alone. A smoke test with Narrator on Windows and VoiceOver on macOS reads the prototype's controls with correct roles and names.
   - The independent review in this file approves, and the owner approves (CLAUDE.md rule 11).

## Consequences

- One rendering stack (`wgpu`) from engine to screen. Preview and visualisations stay on the GPU.
- Licence: RBS source stays GPL-3.0-or-later (ADR-0010). Release binaries that include Slint under its GPL option are effectively GPL-3.0-only. Native in-process plugins distributed with or for those binaries must be GPL-3.0 compatible: code that is only GPL-2.0 is excluded. Sandboxed and out-of-process plugins (ADR-0006) are not linked with Slint and are unaffected. If this becomes a problem, the ways out are Slint's commercial licence or a toolkit swap.
- Slint's release cadence and its `unstable-wgpu-*` API, which may change in minor releases, affect only the UI crate. The exact version pin and an integration test catch breakage.
- Requirement 4 is open for every candidate except Qt. We build Fluent string binding and right-to-left layout mirroring ourselves. Mirroring means mirrored layouts, icons and docking directions in the component library, estimated at a few weeks of component work. It is easier if it is designed into components from the start.
- We also build the docking system ourselves: tab drag and drop, drop indicators and keyboard docking. None of the candidates provides docking for our model.
- The Skia renderer adds a prebuilt Skia library (downloaded by `skia-bindings` at build time, BSD-3-Clause), about 5 MiB of binary size and about 20 MiB of memory compared with femtovg. If femtovg fixes its pipeline caching, switching back is a feature flag.
- Slint globals are per window, so shared UI state is pushed to every window from one place in the UI crate.
- The dependency tree and binary are larger than with iced, so build caching in CI matters.
- Reversal cost: the component library and panels written in `.slint` would be rewritten. The panel, docking and workspace model, the control API client, the visualisation textures and the interop code are toolkit-neutral and stay. Reversing a year after UI work starts is estimated at one to two months of UI work, most of it in components.

## Alternatives considered

- **iced:** the smallest Rust-only build, a natural recursive layout API, and an MIT licence. Rejected for now because:
  - it has no screen reader support (requirement 3);
  - its 0.x API changes in every release;
  - its `wgpu` 27 cannot share `rbs_gpu::interop`.

  Worth re-evaluating if it ships accessibility and tracks current `wgpu`.
- **Qt via `cxx-qt`:** the most mature accessibility, text, mirroring and widgets, under the LGPL. Rejected because:
  - zero copy needs native texture import into Qt's RHI in C++ for each platform, beside `rbs_gpu::interop`;
  - a C++ toolchain and a Qt installation complicate building and contributing;
  - around 87 shared libraries must be shipped.

  It remains the fallback if Slint fails the acceptance gate, and it is measured in the same gate run.
- **Slint with a CPU copy of the preview:** a simpler integration without the unstable feature, but it violates P4.

## Review

- 2026-11-02, independent AI session (fresh context; given only this ADR, the prototypes and their README, the principles, the related ADRs and specs): changes requested. Concerns, and the author's responses in this revision:
  1. The process boundary was treated as an edge case, and the export side is not specified. Response: decision 2 makes two processes the default topology, requires the export contract in the GPU compositor spec before UI work shows engine output, and the gate requires a cross-process zero-copy demonstration.
  2. The `wgpu` coupling held back an engine crate and was unfair to iced. Response: decision 5 never holds back `rbs-gpu`; the comparison table is re-scored around interop reuse instead of device sharing; iced's remaining blockers are accessibility and API churn.
  3. The gate did not match the script (software rendering, 720p, timer ticks). Response: the gate requires extending the script (GPU, 1080p, presented-frame timestamps) and running Qt too.
  4. Slint's 6 to 7 fps on the rasteriser was dismissed too quickly. Response: listed as an open risk; a dense-path stress case is added to the gate.
  5. Accessibility decided the choice but was not gated. Response: keyboard-only operation and a Narrator and VoiceOver smoke test are added to the gate.
  6. Requirement 4 was not shown for Slint. Response: marked open, with the cost of building mirroring stated.
  7. The MSRV bump would hit engine crates. Response: decision 6 sets `rust-version` on the UI crate only.
  8. The licence cost for native plugins was missing. Response: added to Consequences.
  9. The reversal cost was missing. Response: added to Consequences.
  10. The spec cited a Proposed ADR as binding. Response: requirement 8 is marked provisional.
- 2026-11-02, re-review by the same independent session: changes requested. Concerns 1 and 3 to 10 resolved, 2 mostly resolved. New concerns, and the author's responses:
  - A. Decision 5 claimed interop reuse even while Slint lags a `wgpu` major. Response: decision 5 now says the import code is forked during a lag and that the reuse advantage holds only while Slint keeps pace.
  - B. The first review's resolutions were recorded as the reviewer's verdict, and owner approval was missing from the gate. Response: they are now recorded as author responses, this re-review is a separate entry, and owner approval is part of the gate.
  - C. The README framed iced's `wgpu` problem as device sharing. Response: the README wording now matches the interop-reuse framing.
- Owner: Salih approved the choice of Slint on 2026-11-02 (project thread: "slint kullanıyoruz") and offered a MacBook for the GPU measurements.
- Gate run, 2026-11-02, MacBook Pro M4 Pro (Metal), Slint only (Qt not installed): 60 Hz with 0 late frames in every scenario, including a 4,096-point path; idle CPU 0.0 %. Process CPU while animating was 33 to 45 % of one core, above the provisional 10 % limit; the cause is not profiled yet. Details in the prototype README. Frame pacing passes; the CPU limit does not.
- Profile and renderer comparison, 2026-11-02, same Mac: femtovg's wgpu renderer rebuilds render pipelines during steady redraw (most of its CPU). The Skia renderer, which also imports wgpu textures, removes that and lowers 1080p-preview CPU from about 35 % to 20-27 %, with 0 late frames and about 20 MiB more memory; dense paths cost the same in both. Both stay above the provisional 10 % limit because the whole window is redrawn at 60 Hz. Details in the prototype README.
- Owner decision, 2026-11-02: use the Skia renderer; the 10 % CPU limit becomes a UI crate budget instead of a gate item; Qt is not measured.
- Status: the review is still open. The ADR stays Proposed until the acceptance gate in decision 7 is met, including a final independent approval and the owner's approval.
