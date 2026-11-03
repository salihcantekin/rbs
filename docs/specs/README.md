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
| [scene-model.md](scene-model.md) | Draft | `rbs-scene` (model, edits, undo, flattening, graph binding; document format planned) |
| [gpu-compositor.md](gpu-compositor.md) | Draft | `rbs-gpu` |
| [plugin-sdk.md](plugin-sdk.md) | Draft | `rbs-plugin-sdk`, `rbs-plugin-host` (planned) |
| [performance-budgets.md](performance-budgets.md) | Draft | `benches/`, CI |
| [control-api.md](control-api.md) | Draft | `rbs-api` (planned) |
| [testing-strategy.md](testing-strategy.md) | Draft | CI, conformance suites |
| [ui-design-system.md](ui-design-system.md) | Draft | `rbs-ui-tokens`, `rbs-app` (planned) |

New specs start from [template.md](template.md).
