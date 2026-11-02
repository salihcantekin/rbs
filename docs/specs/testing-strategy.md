# Spec: Testing strategy

- Status: Draft
- Related ADRs: [0003](../adr/0003-ports-and-adapters.md), [0009](../adr/0009-performance-budgets-in-ci.md), [0012](../adr/0012-ai-assisted-development.md)

## Layers

| Layer | What | Where it runs |
| --- | --- | --- |
| Unit tests | Pure logic in every crate | Hosted CI, all platforms |
| Property tests | Clock conversion, capability matching, mux output, protocol framing (`proptest`) | Hosted CI |
| Conformance suites | One per port; every adapter runs it | Hosted CI (software adapters), self-hosted (hardware) |
| Fuzzing | Parsers and protocol handlers (`cargo fuzz`) | Nightly |
| Miri / sanitizers | Crates with `unsafe` | Hosted CI where supported |
| Golden image tests | Compositor and filters render reference scenes; compared with tolerance | Hosted CI with software rendering (WARP / lavapipe) |
| Scenario benchmarks | End-to-end headless scenes with budgets | Self-hosted GPU runners, nightly |
| Soak tests | 4-hour stream + record per tier-1 platform | Before each release |

## Rules

- Tests must be deterministic: no sleeping for timing, use a controllable test clock from `rbs-core`.
- No test depends on network access except explicitly marked integration tests against local servers started by the test.
- Hardware-dependent tests skip with a clear message when hardware is absent.
