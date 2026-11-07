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
| [0039](0039-scene-source-types.md) | Scene source types map to port nodes through a registry; the binder routes audio to a mixer per canvas | Accepted |
| [0040](0040-recording-composition.md) | A recording of a canvas is assembled by an engine crate; `rbs-cli record` is the first composition root that runs it | Accepted |
| [0041](0041-studio-recording-on-a-running-engine.md) | A studio keeps a canvas running for preview and meters; recordings start and stop on the running engine | Accepted |
| [0042](0042-loopback-presentation-latency.md) | Loopback audio is delayed by the render endpoint's measured output latency | Rejected |
| [0043](0043-end-of-stream-on-final-ticks.md) | Final ticks are marked, and the video encoder flushes on them | Accepted |
| [0044](0044-control-api-protocol-and-security.md) | Control API: one typed request layer, JSON-RPC over swappable transports, scoped token authentication | Accepted |
| [0045](0045-recording-tail-on-stop.md) | A stopped recording keeps its tail: the export ends first, the audio encoder flushes when its input ends | Accepted |
| [0046](0046-network-access-over-tls.md) | Network access to the control API: TLS on the remote control port, a generated key the client pins | Accepted |
| [0047](0047-several-outputs-per-canvas.md) | Several outputs per canvas: record and stream at once, encoders shared when their settings match | Accepted |
| [0048](0048-selection-overlay-on-the-preview-layer.md) | Draw the preview's selection outline and handles as native layers of the preview | Accepted |
| [0049](0049-main-window-areas-panel-states-and-sizes.md) | Main window areas, panel states (collapse, auto-hide, maximize) and size limits (workspace format 2) | Accepted |
| [0050](0050-command-registry-menus-and-toolbar.md) | One command registry for menus, context menus, the toolbar and the palette | Accepted |
| [0051](0051-user-data-profiles-and-scene-collections.md) | User data as versioned documents; output profiles and scene collections as libraries of named files | Accepted |
| [0052](0052-ai-agent-control-through-mcp.md) | AI agents control RBS through an MCP server that is a plain control API client | Accepted |
| [0053](0053-item-style-and-video-filter-chains.md) | Item shape, border and shadow drawn by the compositor; video filters as a per-source chain of GPU nodes | Accepted |
| [0054](0054-preview-never-disappears.md) | The preview picture never disappears behind popups: split it around covers | Accepted |
| [0055](0055-crash-resilience.md) | Crash resilience: a fault in the app must not end a recording or a stream | Proposed |
| [0056](0056-keep-running-after-the-main-window-closes.md) | Keep running after the main window closes: Close and Quit are separate; Dock on macOS, tray icon on Windows | Accepted |
| [0057](0057-user-themes-and-appearance-settings.md) | User themes as checked token overrides in a theme library; Settings › Appearance picks and edits them | Accepted |
| [0058](0058-crop-editing-of-sources.md) | Crop editing of sources: keep the scale, read the picture size from the capture node | Accepted |
| [0059](0059-scenes-lock-and-source-audio-in-the-studio.md) | Scenes, item lock and source volume in the studio | Accepted |
| [0060](0060-shortcuts-in-app-and-global.md) | Shortcuts bind registry commands to key chords, in the app or globally; Settings › Shortcuts edits them | Accepted |
| [0061](0061-studio-mode-in-the-desktop-app.md) | Studio mode in the desktop app: preview scene picture, cut transition, control API 1.9 | Accepted |
| [0062](0062-built-in-video-filters-and-properties-tabs.md) | Built-in video filters: one filter node behind a filter type trait, settings through parameter channels, and the Properties tabs | Accepted |
| [0063](0063-hold-the-program-while-its-scene-is-edited-in-preview.md) | Hold the program while its scene is edited in preview (studio mode) | Accepted |
| [0064](0064-transition-rail-with-the-pictures.md) | The Transition rail sits with the pictures: between or below Preview and Program, only in studio mode | Accepted |
| [0065](0065-global-sources-and-safe-source-removal.md) | Global sources as a global scene per canvas, and safe source removal | Accepted |
| [0066](0066-virtual-camera.md) | Virtual camera on Windows: the program as a camera for meeting apps, off by default | Accepted |
