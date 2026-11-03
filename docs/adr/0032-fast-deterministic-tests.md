# ADR-0032: Fast, deterministic tests: no flaky or hanging test, fake time, tiers and per-test timeouts

- Status: Accepted
- Date: 2026-11-03
- Deciders: Salih Cantekin (owner)

## Context

The owner set a standing rule on 2026-11-03: the suite must have no flaky and no hanging test, and once one appears, work stops until it is fixed. Unit tests must finish in seconds; a change should run the tests it needs instead of a suite that takes minutes. Tests of time-dependent logic must use a fake time provider, not real waiting.

An audit of `main` on that day found the rule broken in ways a reviewer cannot see from one green run:

- Three `rbs-engine` tests failed now and then on hosted CI (`tick_indices_never_repeat_across_worker_restarts` on Windows, `shutdown_passes_every_queued_packet_not_just_one_ticks_worth` on Windows, `shutdown_after_a_timed_out_swap_leaves_no_worker` on macOS). Each outcome depended on how fast threads were scheduled: a test read a counter the worker bumps after its tick and advanced the clock twice before the worker saw the first advance; two cadence threads ticked "in either order"; a node held a swap for 200 ms of real time against a 20 ms timeout.
- An RTMP test failed on macOS (fixed by #48) and a PipeWire conformance check read the process thread count before the OS had removed an exited thread (fixed in #62).
- Real-time assertions with tight margins: capture conformance required `probe` within 50 ms and `latest_frame` within 20 ms of wall-clock time on any runner (one ScreenCaptureKit failure); transport tests allowed 200 ms of scheduling slack where 390 ms had already been seen.
- Slow tests: the capture conformance negative suite took 4 s (a per-byte iterator filling 4K test frames in a debug build), the engine suite 2 s (real-time holds of 2 s), one SRT test 4 s (it waits out a 4 s protocol latency).

The repository already said "use the `TestClock`, never sleep for timing" ([testing strategy](../specs/testing-strategy.md)), but nothing in CI caught a hang early, retried tests could hide flakes, and there was no rule for real-time bounds or slow tests.

## Decision

1. **Zero flaky, zero hanging tests.** A test that fails once without a code change is a bug of the highest priority: the thread or person that sees it stops feature work, finds the root cause and fixes it (in the test, or in the code if the test found a real race) before merging anything else. Tests are never retried to get green (`retries = 0`), never marked ignored or skipped to get green, and a failure is never waved through as "flaky, re-run".

2. **Time is injected.** Logic that depends on time (cadences, deadlines, back-off, timeouts in media or protocol time, rate estimation) reads it from an injected clock: `rbs_core::time::Clock` with `TestClock` in tests, or an explicit `now` argument. Tests move fake time; they never sleep to let time pass. A test waits for another thread by polling for the condition it needs (`eventually`, a channel, a join), never for a duration.

3. **Outcomes must not depend on scheduling.** A test that needs an engine or adapter timeout to fire holds the other side until the timeout has been observed (a gate opened on an observable step, such as `WorkerStats::stops`), never "for a while". A test that waits for a tick waits for the tick by its index, not for a counter bumped after the tick. Threads whose order matters are ordered by the test (for example cadence offsets), not left to race.

4. **Real-time bounds are hang guards only.** Where real time cannot be faked (a wait for the OS, a real socket, a real daemon, a contract like "`finish(timeout)` returns within its budget"), a test may bound real time only as a hang guard: at least 10 times the expected time and never below 250 ms; prefer 1 s or more when a passing run does not wait for it. A passing run never waits for the bound. For a "returns within its budget" contract the expected time is the budget itself, so the guard is the budget plus at least 1 s of slack: it catches a call that ignores its budget, not one that overruns it slightly (that precision needs fake time or a benchmark). Engine or adapter timeouts that a test expects to fire are set to at least 250 ms too, because the same timeout also bounds the setup steps that must not fire. Performance budgets (probe within 50 ms, latency per frame) are measured with benchmarks ([ADR-0009](0009-performance-budgets-in-ci.md)), not asserted on wall-clock time in pass/fail tests.

5. **Tiers.**
   - *Fast tier* (default `cargo test` / `cargo nextest run`): every unit, property, conformance and in-process integration test. Each test should take well under 1 s; a test binary over 1 s needs a reason in a comment. The whole fast tier of a crate runs in seconds, so a change runs `cargo test -p <crate>` for the crates it touches.
   - *Slow tier*: a deterministic test that must take seconds of real time (a protocol latency, a real daemon round trip) is marked `#[ignore = "slow: <why>"]`, named `slow_*`, and runs with `cargo nextest run --run-ignored only -E 'test(/^slow_/)'` (the name selects it: other ignored tests are child-process helpers or need a tool). CI runs both tiers.
   - *Hardware and tool tiers*: tests that need hardware skip at run time unless `RBS_REQUIRE_<THING>=1` is set (as today); tests that need an external tool are `#[ignore = "needs <tool>"]` in `*_interop` binaries.

6. **CI enforces it.** CI runs tests with `cargo-nextest` and the `ci` profile in `.config/nextest.toml`: no retries, a test still running at 15 s is reported and one at 60 s is killed (a hang fails in a minute, not at the job timeout), every job has a timeout, and the slowest tests of every run are printed in the job summary. Timings are reported, not gated: a pass/fail gate on wall-clock time would itself be flaky.

7. **Stress before claiming a fix.** A fix for a flaky test shows the root cause in the PR and runs the test repeatedly under CPU load (for example 50 runs pinned to two cores with busy loops on the others) before and after.

The procedure is the `test-reliability` skill; the rules are in the [testing strategy](../specs/testing-strategy.md) and CLAUDE.md.

## Consequences

- Fixed in the same change: the three engine flakes (index-based waits through a `Ticks` test runtime, a half-frame encoder offset so render always ticks first, gates opened on `WorkerStats::stops`), capture conformance hang guards instead of 50 ms / 20 ms wall-clock budgets, transport `finish`/`drop` bounds with 1 s of slack, a fast test-frame fill, and the 4 s SRT test moved to the slow tier. The engine suite went from 2.1 s to 0.1 s, the capture conformance negative suite from 4.05 s to 0.27 s.
- `rbs-engine` gains `WorkerStats::stops()`, a counter bumped on the control thread before a worker is joined (not on the worker's hot path); tests use it to react to engine steps instead of timing them.
- The 50 ms probe budget and a tighter `latest_frame` bound are no longer pass/fail checks; until a probe benchmark exists they are open measurement items ([performance budgets](../specs/performance-budgets.md)). The conformance `latest_frame` guard (500 ms) catches a call that blocks until a frame is released, not one that waits a single frame period.
- Every future test is reviewed against these rules; a new real-time wait in a test needs a comment saying why it is a hang guard.
- Hardware conformance runs and soak tests stay outside hosted CI, as before.
- Tests that open the GPU or a hardware encoder run one at a time under nextest (test group `gpu`): with a process per test, dozens of processes opening the device at once made the macOS runners' virtual Metal device report "Parent device is lost".
- Developers who use plain `cargo test` keep working; nextest adds per-test timeouts and process isolation (each test in its own process: tests that count process threads no longer see other tests' threads, while tests that share a device across processes are serialized, see above).

## Alternatives considered

- **Retry failed tests in CI** (nextest `retries`): hides real races, which in an engine with lock-free cross-thread code are exactly the bugs tests must find. Rejected.
- **Make every engine timeout fake** (an injectable wait provider for swap and drain timeouts): the most deterministic, but changes the engine's control API and the hot paths' neighbours for a problem that observable steps (`stops`, tick indices) already solve. Kept as an option if a future test cannot be made deterministic otherwise.
- **Wall-clock budgets in tests with larger margins**: still depend on the runner; the margin that is "enough" keeps growing (50 ms, then 200 ms, then 390 ms seen). Budgets belong in benchmarks.
- **Separate slow tests by a cargo feature**: needs a feature in every crate and is easy to forget in CI; `#[ignore = "slow: ..."]` is visible in the test list and selectable with one flag.

## Review

Independent review per ADR-0014.

- Reviewer: independent AI session (fresh context, given the diff, specs and code), 2026-11-03. Verdict: changes requested, then resolved. It re-ran the engine suite 60 times on two cores and 40 times on one core under CPU load (0 failures) and checked `stops` (control thread only, off the hot path), the `TickLog` release/acquire pairing, the offset arithmetic and that no test can wait past its 10 s guard.
- Concerns and resolution:
  1. The slow-tier step ran every ignored test (child-process helpers, a manual golden dump that failed on Linux). Resolved: slow tests are also named `slow_*` and the step selects `test(/^slow_/)`.
  2. Process-per-test made the macOS virtual Metal device fail ("Parent device is lost"). Resolved: nextest test group `gpu` with one thread for GPU and hardware-encoder packages; Consequences corrected.
  3. Expected-timeout tests used 20 ms and 100 ms swap timeouts that also bound setup applies, and transport "within budget" bounds were not 10x. Resolved: 250 ms (`EXPECTED_TIMEOUT`), setup applies assert no errors, rule 4 states the budget-contract case.
  4. The spec claimed the 50 ms probe budget is benchmarked; no benchmark exists, and 500 ms lets a one-frame wait in `latest_frame` pass. Resolved in text: both are recorded as open measurement items in the performance budgets spec; a probe benchmark and a non-timing check of `latest_frame` while all frames are held are follow-ups.
  5. Status Accepted while the review was pending. Resolved: this record.
  6. Stale "either order" comments fixed. The Rig no longer exercises the encoder ticking before render; the engine does not depend on that order, and the queue tests in `rbs-graph` cover reading a replaced frame.
- Owner approval: Salih, 2026-11-03 (he set the rule; early-phase policy lets threads mark ADRs Accepted).
