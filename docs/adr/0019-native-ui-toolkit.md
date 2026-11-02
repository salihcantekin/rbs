# ADR-0019: Slint as the desktop UI toolkit

- Status: Proposed
- Date: 2026-11-02
- Deciders: Salih (owner)
- Builds on: ADR-0016 (native UI, toolkit chosen by prototype), ADR-0017 (flexible panels and workspaces), ADR-0010 (GPL-3.0-or-later)

## Context

ADR-0016 requires a native Rust UI toolkit and leaves the choice among Slint, iced and Qt (through `cxx-qt`) to a comparison prototype measured against the [toolkit requirements](../specs/ui-design-system.md#requirements-on-the-ui-toolkit). ADR-0017 adds a docking and workspace system to those requirements. The owner also asked whether the toolkit can draw rich custom graphics such as equalizers, histograms and meters.

Three prototypes with the same feature set were built in [`prototypes/ui-toolkit`](../../prototypes/ui-toolkit/README.md): a toolkit-neutral layout model (splits, tab stacks, floating windows, TOML workspaces) shared by all three, splitter dragging, close/reopen, move, float and dock back, workspace switching with save and load, an engine stand-in that renders the program output into `wgpu` textures, and a 64-band spectrum redrawn every frame with the toolkit's own vector drawing. The README holds the full tables and the measurement scripts. The findings that decide the choice:

| Requirement | Slint 1.18 | iced 0.14 | Qt 6 via cxx-qt 0.10 |
| --- | --- | --- | --- |
| 1. Preview without CPU copies | Yes. The engine can create the `wgpu` device and hand it to Slint (`WGPUConfiguration::Manual`); textures are shown directly | Yes, but iced owns the device and the engine must adopt it from inside a shader pipeline | No in the prototype: frames were copied through the CPU. Zero copy needs per-platform native interop with Qt's RHI, not attempted |
| `wgpu` version fit with `rbs-gpu` (wgpu 30, ADR-0005) | Matches (`unstable-wgpu-29`/`-30` features) | Does not match (iced uses wgpu 27); one device cannot be shared across `wgpu` majors | Not applicable |
| 2. Custom drawing at display rate | `Path` and primitives; GPU textures for heavy visuals | `canvas` and `shader` widgets | Qt Quick `Shape`, scene graph |
| 3. Accessibility | Screen reader tree with roles and names (AccessKit), checked on Linux | None exposed | Most mature; needs `Accessible` annotations on custom controls |
| 4. Bidirectional text | Correct shaping and order | Correct shaping and order | Correct; built-in layout mirroring |
| 5. Idle CPU | 0.0 % | 0.0 % | 0.0 % |
| 6. Docking | Built on our own model; works (layout flattened in Rust because `.slint` forbids recursive components) | Built on our own model; works | Built on our own model; works |
| 7. Licence under ADR-0010 | GPL-3.0-only option is compatible; release binaries become effectively GPL-3.0-only | MIT, compatible | LGPL-3.0, compatible with dynamic linking |
| Build and distribution | 256 s clean build, 22.6 MiB binary, 372 crates, Rust 1.92 | 142 s, 13.1 MiB, 204 crates, Rust 1.88 | 173 s plus a Qt install, 7 MiB binary plus about 28 MiB of Qt libraries, C++ toolchain, 87 shared libraries |
| API stability | 1.x with semver (except the `unstable-wgpu-*` features) | 0.x, breaking changes each release | Qt stable; `cxx-qt` 0.x |

Rendering-load numbers were measured on a software rasteriser (no GPU in the test machine) and are not a valid comparison; see the README.

## Decision

1. The RBS desktop UI uses **Slint**, under its GPL-3.0-only licence option, with the `femtovg-wgpu` renderer and the `unstable-wgpu-N` feature that matches the `wgpu` major version used by `rbs-gpu`.
2. The UI shows engine output (preview, program, projectors, heavy visualisations) as `wgpu` textures on the UI's device, imported through `rbs_gpu::interop` when the engine runs in another process (ADR-0008) and shared directly when it runs in the same process. No CPU copies on the default path (P4). The prototype verified only the same-process case.
3. Rich visualisations follow two tiers, independent of the toolkit:
   - Light, low-density graphics (audio meters, fader scales, equalizer curves, small charts) use the toolkit's vector primitives in design-system components.
   - Dense or per-pixel graphics (histograms, waveform and vectorscope, spectrum analysers with many bins, large meter bridges) are computed and drawn on the GPU by the engine or a UI-side `wgpu` pass and shown as textures, so the toolkit only composites them.
4. The panel, docking and workspace model lives in a toolkit-neutral module of the UI crate (as `dock-model` in the prototype), and Slint code only renders it. This keeps the toolkit replaceable (ADR-0008) and lets the model be tested without a window.
5. `slint` and `slint-build` are pinned to an exact version in the UI crate. A `wgpu` major upgrade in `rbs-gpu` waits until Slint offers the matching `unstable-wgpu-N` feature, or is done together with the Slint upgrade.
6. Acceptance gate: this ADR becomes Accepted only after the load scenarios in `prototypes/ui-toolkit/measure-runtime.sh` (program preview at 1080p60 plus a 64-band spectrum) have been run on the reference platform with a real GPU and meet these provisional limits: the window keeps the display refresh rate (60 Hz) with at most 1 % late frames over 60 s, and the UI process uses at most 10 % of one CPU core. The limits move into the [performance budgets](../specs/performance-budgets.md) when the UI crate is created. The independent review in this file is also required.

## Consequences

- One rendering stack (`wgpu`) from engine to screen; preview and visualisations stay on the GPU.
- RBS release binaries are effectively GPL-3.0-only while they include Slint under its GPL option, although RBS source stays GPL-3.0-or-later (ADR-0010). If that becomes a problem, Slint's commercial licence or a toolkit swap are the ways out.
- `rbs-gpu` and the UI crate share one `wgpu` major version, and Slint's release cadence gates `wgpu` upgrades. The `unstable-wgpu-*` API may change in Slint minor releases; the exact pin and a small integration test catch that.
- The workspace minimum Rust version rises to Slint's (1.92 for Slint 1.18) when the UI crate is added.
- We build the docking system, tab bar drag and drop, drop indicators and layout mirroring for right-to-left languages ourselves; none of the three toolkits provides docking for our model.
- Slint's per-window globals mean shared UI state is pushed to every window explicitly; the UI crate needs one place that does this.
- Accessibility on Windows and macOS is still to be verified with screen readers when the UI crate starts.
- Larger dependency tree and binary than iced; build-time caching in CI matters.

## Alternatives considered

- **iced:** smallest Rust-only build and a natural recursive layout API, MIT licence. Rejected for now: no screen reader support (requirement 3, U-rules on accessibility), the engine cannot hand it a device, its `wgpu` 27 does not match `rbs-gpu`'s `wgpu` 30, and its 0.x API changes every release. Worth re-evaluating if it ships accessibility and tracks current `wgpu`.
- **Qt via `cxx-qt`:** most mature accessibility, text, mirroring and widgets, LGPL. Rejected: zero-copy preview needs per-platform native texture interop with Qt's RHI on top of `rbs_gpu::interop`, the C++ toolchain and Qt installation complicate building and contributing, and around 87 shared libraries must be shipped. Remains the fallback if Slint fails the acceptance gate.
- **Slint with a CPU copy of the preview:** simpler integration without the unstable feature, but violates P4.

## Review

Independent review per ADR-0014: pending.
