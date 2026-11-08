---
name: test-reliability
description: Rules and procedure for RBS tests (ADR-0032). Use when writing or changing any test, when a test failed once, hung or got slow, or before claiming a flaky test is fixed.
---

# Test reliability

Policy: `docs/adr/0032-fast-deterministic-tests.md`. Rules: `docs/specs/testing-strategy.md#rules`.

## A test failed once, hung, or a CI run was red without a code change

This blocks all other work. Do not re-run CI to get green, do not retry, do not `#[ignore]` it.

1. **Get the evidence.** The failing assertion, platform, run id and log lines (`get_job_logs` for CI). Note which other tests ran at the same time.
2. **Find the root cause.** Ask of every wait and assertion in the test: *what if this thread is descheduled for 500 ms right here?* Typical causes found in this repository:
   - A counter read right after an engine call that the worker bumps *after* the work the call waited for (`WorkerStats::ticks` after `apply`): the next clock advance is then skipped as late. Wait for the tick by its index instead (`Ticks` runtime in `crates/rbs-engine/tests/support`).
   - Two threads on the same cadence "in either order": order them (a cadence offset) and drive the clock in steps.
   - A gate or stall that ends after a real duration while the test expects an engine timeout: open it on an observable engine step (`WorkerStats::stops`, a tick index, a release log) with `open_when`.
   - A wall-clock bound with a small margin (`elapsed() < budget + 50 ms`): make it a hang guard (rule 4) or measure in a benchmark.
   - OS state read too early (thread counts in `/proc` after `join`): poll for the condition with a bound only a failing run reaches.
3. **Reproduce if you can**, before the fix: build the test binary and stress it:

   ```sh
   cargo test -p <crate> --test <binary> --no-run
   # 6 busy loops, the test pinned to two cores, 50 runs
   for i in 1 2 3 4 5 6; do yes > /dev/null & done
   for i in $(seq 50); do taskset -c 0,1 target/debug/deps/<binary>-<hash> -q || echo FAIL; done
   kill %1 %2 %3 %4 %5 %6
   ```

   A race that does not reproduce locally is still fixed from the analysis; say so in the PR.
4. **Fix the cause**, in the test or in the code if the test found a real race. Then run the same stress loop after the fix and put both results in the PR.

## Writing or changing a test

Check each item; a reviewer checks them again.

- [ ] Time-dependent logic gets time from an injected `Clock` (`TestClock`) or a `now` argument; the test advances it. No `thread::sleep` in a test except as the behaviour under test (a fake adapter that blocks), with a comment.
- [ ] Waits are for conditions (`eventually`, channel, join, tick index), never for durations.
- [ ] Every real-time bound is a hang guard: at least 10x the expected time, at least 250 ms (prefer 1 s or more when a passing run does not wait for it), with a comment saying so. No wall-clock performance asserts; use a benchmark (`measure-performance`).
- [ ] An engine built for a test that applies graphs on real workers gets `EngineConfig::default().swap_timeout(LIMIT)` (the hang guard), never the 1 s production default: a swap on a loaded CI runner (WARP, 4 vCPUs) can take longer than that without anything being wrong. A test that expects `SwapTimeout` sets its own short value.
- [ ] An expected timeout cannot be missed because the other side finished early: the other side is held until the timeout was observed.
- [ ] The outcome does not depend on thread order unless the test sets the order.
- [ ] Fast: the test takes well under 1 s in a debug build. Expensive fixtures are built cheaply (`slice::repeat` instead of per-byte iterators; small frames unless size is the point). A test that must take seconds of real time is `#[ignore = "slow: <why>"]` and named `slow_*` (the slow tier selects by name).
- [ ] Hardware tests skip unless `RBS_REQUIRE_<THING>=1`; external tools are `#[ignore = "needs <tool>"]` in a `*_interop` binary.
- [ ] No test depends on other tests in the same process (nextest runs each test in its own process; `cargo test` runs them as threads of one process: both must pass).

## While iterating

Run what you touched: `cargo test -p <crate>` (or `cargo nextest run -p <crate>`). Run the workspace once before pushing. Check the slowest-tests table in the CI job summary for tests you added.
