# Spec: Testing strategy

- Status: Draft
- Related ADRs: [0003](../adr/0003-ports-and-adapters.md), [0009](../adr/0009-performance-budgets-in-ci.md), [0012](../adr/0012-ai-assisted-development.md), [0032](../adr/0032-fast-deterministic-tests.md)

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

Binding ([ADR-0032](../adr/0032-fast-deterministic-tests.md); procedure: `test-reliability` skill).

1. **No flaky, no hanging test.** A test that fails once without a code change is fixed (root cause, in the test or the code) before any other work merges. No retries, no `#[ignore]` or skip to get green, no "re-run CI".
2. **Fake time.** Time-dependent logic reads an injected clock (`rbs_core::time::Clock`; `TestClock` in tests) or takes `now` as an argument. Tests advance fake time and never sleep for timing.
3. **Wait for conditions, not durations.** Another thread is awaited by polling the condition (`eventually`, a channel, a join). A tick is awaited by its index, not by a counter bumped after it. Thread order that matters is set by the test.
4. **Timeouts that must fire** hold the other side until the timeout was observed (a gate opened on an observable step such as `WorkerStats::stops`), never for a fixed time.
5. **Real-time bounds are hang guards**: at least 10x the expected time and never below 250 ms (prefer 1 s or more when a passing run does not wait for it), with a comment. Performance budgets are checked by benchmarks, not by wall-clock asserts in pass/fail tests.
6. **Tiers.** Fast tier: everything run by default; each test well under 1 s, a crate's fast tier in seconds; a binary over 1 s says why in a comment. Slow tier: deterministic tests that need seconds of real time, `#[ignore = "slow: <why>"]`, run with `cargo nextest run --run-ignored only`. Hardware: skip unless `RBS_REQUIRE_<THING>=1`. External tools: `#[ignore = "needs <tool>"]` in `*_interop` binaries.
7. **CI** runs both tiers on every platform with `cargo nextest run --profile ci` (`.config/nextest.toml`): no retries, a test at 15 s is reported, at 60 s it is killed; job timeouts; the slowest tests are listed in the job summary (reported, not gated).
8. **A flake fix is proven**: root cause in the PR, and the test run repeatedly under CPU load before and after.
9. No test depends on network access except explicitly marked integration tests against local servers started by the test.
10. Hardware-dependent tests skip with a clear message when hardware is absent.

## Running tests

| Goal | Command |
| --- | --- |
| The crates a change touches | `cargo test -p <crate>` (or `cargo nextest run -p <crate>`) |
| Whole fast tier | `cargo nextest run --workspace` (or `cargo test --workspace`) |
| Slow tier | `cargo nextest run --workspace --run-ignored only -E 'not binary(ffmpeg_interop)'` |
| Stress one test binary | run it 50 times pinned to two cores (`taskset -c 0,1`) with busy loops on the others |
