# ADR-0010: Project license

- Status: Accepted
- Date: 2026-11-02

## Context

The license affects which libraries can be bundled (x264 and GPL FFmpeg are GPL), whether closed-source plugins and commercial use are possible, and how attractive the project is to contributors and companies.

## Options

| Option | Bundled GPL encoders (x264, GPL FFmpeg) | Closed-source plugins | Notes |
| --- | --- | --- | --- |
| GPL-3.0-or-later | Yes | Only across process or WASM boundaries, subject to legal review | Same family as OBS |
| MPL-2.0 | No, only as user-installed optional adapters | Yes | File-level copyleft |
| Apache-2.0 OR MIT | No, only as user-installed optional adapters | Yes | Most permissive, common in Rust |

AAC and HEVC patent questions apply regardless of the license.

## Decision

RBS is licensed under **GPL-3.0-or-later**, chosen by the project owner.

- It keeps RBS in the same copyleft family as OBS Studio, so community expectations and the ability to bundle GPL encoders (x264, GPL builds of FFmpeg) carry over.
- GPL-3.0 rather than OBS's GPL-2.0-or-later: Apache-2.0 code cannot be combined with GPL-2.0-only. A GPL-2.0-or-later project may still use Apache-2.0 dependencies, but its distributed binaries are then GPL-3.0 in practice. Key planned dependencies are Apache-2.0 or `Apache-2.0 WITH LLVM-exception` (wasmtime), and Slint is offered under GPL-3.0-only, a royalty-free license or a commercial license (ADR-0016). Stating GPL-3.0-or-later says what our binaries will be anyway.
- If Slint is used under its GPL option, distributed app binaries are effectively GPL-3.0-only. The source stays GPL-3.0-or-later.
- Dependencies must be GPL-3.0 compatible. `cargo-deny` enforces an allow list in `deny.toml`.
- Plugins:
  - Native (C ABI) plugins load into the process and form a combined work, so they must be GPL-3.0 compatible. Closed-source vendor plugins cannot use the native tier.
  - Sandbox (WASM) and process-tier plugins talk through a documented interface. Whether a closed-source plugin there is a separate work depends on how the plugin SDK crates and the WIT files (`rbs:plugin`, `rbs:panel`) are licensed. If they are GPL, any guest built with them likely contains GPL code. Licensing the SDK and WIT files as `Apache-2.0 OR MIT` is the leading option; it is **not decided yet** and needs its own ADR, with legal advice, before plugin SDK 1.0.
  - Proprietary SDKs (for example NDI) cannot be linked into GPL code. The roadmap's NDI plugin must use a process-tier bridge or another approach checked by legal review.
- Contributions are accepted under the same license (inbound = outbound) with a Developer Certificate of Origin sign-off. There is no CLA, so relicensing later is hard; this is intended.

## Consequences

- A `LICENSE` file with the GPL-3.0 text and `license = "GPL-3.0-or-later"` in the workspace manifest.
- ADR-0007 still keeps FFmpeg dynamically linked and optional; GPL builds become an allowed distribution choice.
- Companies that want to embed the engine in a closed product cannot; this is accepted.
- Follow-ups: SPDX headers in source files and a DCO check in CI.
- AAC and HEVC patent questions remain separate from the license.

## Review

- 2026-11-02, independent AI review (fresh session given this ADR, ADR-0006/0007/0008/0016, the plugin SDK spec and the diff): changes requested. Fixed: Apache-2.0 and GPL-2.0 reasoning made precise, wasmtime and Slint licenses stated correctly, native-plugin and SDK/WIT licensing consequences spelled out, NDI conflict flagged, license allow list extended, roadmap updated, copyright notice added, official wrapped GPL text used.
- Owner approval: Salih, 2026-11-02 (project thread, chose GPL-3.0-or-later).
