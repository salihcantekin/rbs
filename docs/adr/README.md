# Architecture Decision Records

An ADR records one decision that is hard to reverse or affects more than one crate: the context, the decision, and its consequences. Specs in [../specs](../specs/) describe the current contract; ADRs explain why it looks that way.

## Process

1. Copy [template.md](template.md) to `NNNN-short-title.md` with the next free number.
2. Open a pull request with status `Proposed`.
3. Get an independent review ([ADR-0014](0014-review-policy.md), `architecture-review` skill) and record it in the ADR's `## Review` section.
4. The status becomes `Accepted` only when the review is recorded and the owner approves; a PR may be merged while the ADR is still `Proposed`. To change a decision, write a new ADR that supersedes the old one and update the old one's status to `Superseded by ADR-NNNN`. Do not rewrite accepted ADRs.

## Index

| ADR | Title | Status |
| --- | --- | --- |
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0002](0002-headless-engine-and-media-graph.md) | Headless engine built on a media graph | Accepted |
| [0003](0003-ports-and-adapters.md) | Every external technology behind a port with swappable adapters | Accepted |
| [0004](0004-single-master-clock.md) | Single master clock for all media | Accepted |
| [0005](0005-gpu-compositing-with-wgpu.md) | GPU compositing with wgpu and zero-copy frames | Accepted |
| [0006](0006-three-tier-plugin-model.md) | Three-tier plugin model | Accepted |
| [0007](0007-ffmpeg-as-optional-adapter.md) | FFmpeg only as an optional codec adapter | Accepted |
| [0008](0008-ui-as-api-client.md) | User interfaces are clients of the control API | Accepted |
| [0009](0009-performance-budgets-in-ci.md) | Performance budgets enforced in CI | Accepted |
| [0010](0010-project-license.md) | Project license | Accepted |
| [0011](0011-english-only-repository.md) | English for all repository content | Accepted |
| [0012](0012-ai-assisted-development.md) | Spec-first, AI-assisted development workflow | Accepted |
| [0013](0013-zero-cost-when-unused.md) | Zero cost when unused | Accepted |
| [0014](0014-review-policy.md) | Independent review for architecture, extra review for hot paths | Accepted |
| [0015](0015-ui-design-rules.md) | Binding UI design rules and a single design system | Accepted |
| [0016](0016-native-desktop-ui.md) | Native desktop UI, no web technology | Accepted |
| [0017](0017-flexible-panels-and-workspaces.md) | Flexible panels and saved workspaces | Accepted |
| [0018](0018-media-frame-representation.md) | Media frames as small values with leased memory | Accepted |
| [0019](0019-encode-mux-transport-ports.md) | Separate encode, mux and transport ports for outputs | Accepted |
| [0020](0020-node-contract-and-schedule-swap.md) | Node contract and lock-free schedule swap | Accepted |
| [0021](0021-scene-collection-model.md) | Scene collection as a validated model with scoped scenes and inverse-edit undo | Accepted |
| [0022](0022-native-ui-toolkit.md) | Slint as the desktop UI toolkit | Accepted |
| [0023](0023-node-parameter-channels.md) | Latest-value parameter channels for running nodes | Accepted |
| [0024](0024-node-restart-with-back-off.md) | Restart failed nodes with back-off, contained to the failed node | Accepted |
| [0025](0025-encoder-surface-export.md) | Hand encoders exported GPU surfaces as native frames | Accepted |
| [0026](0026-partitions-and-queues.md) | Partitions joined by queue endpoint nodes | Accepted |
| [0027](0027-engine-loop.md) | Engine loop: a worker thread per partition in a headless rbs-engine crate | Accepted |
| [0028](0028-cross-process-preview-surfaces.md) | Share preview surfaces with the UI process as native handles | Accepted |
| [0029](0029-final-ticks-on-drain.md) | Final ticks on drain | Accepted |
| [0030](0030-worker-thread-setup.md) | Worker thread setup: a thread policy port and a runtime attach hook | Accepted |
| [0031](0031-adapter-support-crates.md) | Adapter support crates | Accepted |
| [0032](0032-fast-deterministic-tests.md) | Fast, deterministic tests: no flaky or hanging test, fake time, tiers and per-test timeouts | Accepted |
| [0033](0033-audio-input-port.md) | Audio input port: pull planar `f32` from a lock-free sample ring, no resampling in adapters | Accepted |
| [0034](0034-preview-as-native-layer.md) | Show the engine preview as a native compositor layer, not through the toolkit's redraw | Accepted |
| [0035](0035-meters-as-native-layers.md) | Draw the moving parts of audio meters as native compositor layers | Accepted |
| [0036](0036-windows-preview-layer.md) | Cross-process preview on Windows: shared D3D12 textures, a named pipe and a composition swapchain layer | Accepted |
| [0037](0037-audio-graph.md) | Audio graph: block-cadence partitions on the shared executor, drift correction in input nodes, one graph format | Accepted |
| [0038](0038-io-graph-nodes.md) | Graph nodes for capture, encoding and outputs in an engine crate | Accepted |
| [0039](0039-scene-source-types.md) | Scene source types map to port nodes through a registry; the binder routes audio to one mixer | Proposed |
