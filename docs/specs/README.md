# Specifications

Specs are the current contracts of RBS. Code must match them; a PR that changes a contract updates the spec in the same PR. Each spec has a status:

- **Draft**: under design, may change freely.
- **Stable**: implemented; changes follow semver of the crate that implements it.

| Spec | Status | Implemented in |
| --- | --- | --- |
| [backend-ports.md](backend-ports.md) | Draft | `rbs-core` (capabilities, registry), port crates |
| [capture-port.md](capture-port.md) | Draft | `rbs-capture`, `rbs-capture-*` adapters |
| [audio-io-port.md](audio-io-port.md) | Draft | `rbs-audio-io`, `rbs-audio-io-*` adapters |
| [output-ports.md](output-ports.md) | Draft | `rbs-codec`, `rbs-mux`, `rbs-transport` |
| [media-graph.md](media-graph.md) | Draft | `rbs-graph` (graph description, plan, node contract, executor and controller, parameter channels, restart with back-off, partitions and queues), `rbs-core` (`lifecycle`) |
| [audio-graph.md](audio-graph.md) | Draft | `rbs-audio` (input node, resampler, mixer, meters, encoder node), `rbs-graph` (audio values, audio queue) |
| [engine.md](engine.md) | Draft | `rbs-engine` (worker threads per partition, pacing, control loop, shutdown, non-blocking handle) |
| [media-types.md](media-types.md) | Draft | `rbs-core` (`media`, `timing`) |
| [scene-model.md](scene-model.md) | Draft | `rbs-scene` (model, edits, undo, flattening, graph binding, validating loader) |
| [user-data.md](user-data.md) | Draft | `rbs-user-data` (versioned documents, libraries, output profiles, scene collection document; ADR-0051) |
| [gpu-compositor.md](gpu-compositor.md) | Draft | `rbs-gpu` |
| [video-filters.md](video-filters.md) | Draft | `rbs-filters` (filter type trait, filter node, catalogue, conformance), `rbs-sources`, `rbs-record` (ADR-0062) |
| [audio-filters.md](audio-filters.md) | Draft | `rbs-audio-filters` (audio filter type trait, DSP, filter node, Gain, Noise gate, conformance), `rbs-params` (ADR-0082) |
| [virtual-camera.md](virtual-camera.md) | Draft | `rbs-vcam` (port, conformance), `rbs-vcam-ring` (frame ring), `rbs-vcam-*` adapters, `rbs-nodes` (virtual camera node), `rbs-record` (ADR-0066) |
| [scene-sources.md](scene-sources.md) | Draft | `rbs-sources` (built-in source types, adapter registry for sources, instances for bound sources) |
| [io-nodes.md](io-nodes.md) | Draft | `rbs-nodes` (capture source, video export, video encoder and output nodes), `rbs-gpu` (`upload`, `download`) |
| [recording.md](recording.md) | Draft | `rbs-record` (scene to encoders and a muxed output on the engine), `rbs-cli` (`record`, `devices`) |
| [plugin-sdk.md](plugin-sdk.md) | Draft | `rbs-plugin-sdk`, `rbs-plugin-host` (planned) |
| [performance-budgets.md](performance-budgets.md) | Draft | `benches/`, CI |
| [control-api.md](control-api.md) | Draft | `rbs-api`, `rbs-api-ws`, `rbs-api-ipc` (planned; ADR-0044) |
| [mcp-server.md](mcp-server.md) | Draft | `rbs-mcp` (planned; ADR-0052) |
| [crash-resilience.md](crash-resilience.md) | Draft | `rbs-crash` (panic boundary, crash reports, unclean-exit detection), `rbs-app` (`entry.rs`, `clippy.toml`, studio host recovery; ADR-0055) |
| [testing-strategy.md](testing-strategy.md) | Draft | CI, conformance suites |
| [ui-design-system.md](ui-design-system.md) | Draft | `rbs-ui-tokens`, `rbs-app` (planned) |
| [themes.md](themes.md) | Draft | `rbs-ui` (`tokens`), `rbs-user-data`, `rbs-app` (user themes, Settings › Appearance; ADR-0057) |
| [shortcuts.md](shortcuts.md) | Draft | `rbs-ui` (`shortcut`), `rbs-user-data`, `rbs-app` (shortcuts, global shortcuts, Settings › Shortcuts; ADR-0060) |

New specs start from [template.md](template.md).
