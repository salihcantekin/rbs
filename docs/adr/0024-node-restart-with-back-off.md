# ADR-0024: Restart failed nodes with back-off, contained to the failed node

- Status: Accepted
- Date: 2026-11-03
- Deciders: Salih (owner)

## Context

Principle P7 says a failure is contained to the node that failed: its last good frame or silence is used, it is restarted with back-off, the user is notified, and other outputs keep running. ADR-0020 implemented part of this. The executor bypasses a node after `failure_limit` failed ticks, a panic or a failed `warm`, and a failed `prepare` leaves the node idle. It left these gaps (ADR-0020, Decision 7):

- A bypassed node stays in the schedule, holding its resources, until the scene changes. Nothing restarts it.
- A failed `prepare` is retried on every `apply`, so a `prepare` that blocks (a device that times out) makes every edit slow.
- A failed `prepare` replans the graph as if the node were Standby. Its dependants are skipped and released too, against P7: a failing filter takes its source down, and a failing output takes its encoder and canvas chain down.

Forces:

- The executor must stay real-time: no allocation, no locking, no waiting (P6). Restart work (`release`, `prepare`) belongs on the control thread.
- Restarts must not depend on the user editing the scene. A capture card that drops out at 3 a.m. must come back on its own.
- Some errors will not go away by retrying, such as a permission the user has to grant. The capture spec says these are never retried automatically.
- The description's lifecycle state is what the scene wants and is never rewritten by the engine (ADR-0020, Decision 5).
- Tests must not sleep (CLAUDE.md): time comes from an injectable `rbs_core::time::Clock`.

## Decision

1. **The controller owns restart policy.** It keeps one failure record per node: the number of failures in a row, when the node may be prepared again, and whether it has been restarted since. The executor changes only to report.
2. **Failed nodes stay in the plan without an instance.** This supersedes ADR-0020's "the plan is recomputed without it" for failures.
   - A node whose `prepare` failed, or that waits for its restart, keeps its place in the schedule with no instance.
   - The executor already skips such entries. Its video outputs keep the last frame of the instance that left, so P7's "last good frame" holds, and its event outputs stay empty.
   - Its dependants, and the nodes upstream of a failed output, keep running and receive no events: a failure stays contained to its node.
   - A node with no instance or a different descriptor is a configuration problem, not a failure. For those the plan is still recomputed as if the node were Standby, as in ADR-0020.
3. **What counts as a failure:**
   - `prepare` returns an error or panics.
   - The executor bypassed the node for any reason it reports in `NodeHealth`: errors in a row, a panic, a failed `warm`, or a permanent error. `NodeHealth::last_error` carries a static summary of the last error, for the UI.
4. **Instance epochs.** Each successful `prepare` gives the node's instance a new epoch, and the schedule records it per entry.
   - A bypass is recorded at most once per epoch.
   - Health of an older epoch is ignored. That covers an instance replaced since, retried by the user, or switched off.
   - So no `collect` can resurrect a failure the user already cleared.
5. **Back-off.** `RestartPolicy { initial, max, max_attempts, stable_after }`.
   - The wait after failure n is `initial * 2^(n-1)`, saturating at `max`.
   - Defaults: 0.5 s, doubling up to 30 s, never giving up, forgiven after 10 s.
   - When `max_attempts` is reached, the node is not restarted automatically.
   - Forgiveness is decided only in `collect`, from the retired schedule's health. When the current instance was not bypassed at the swap and `stable_after` has passed since its restart, the record is removed (`Recovered`). A node that is bypassed when the control loop is late is therefore never forgiven, and a flapping node escalates its back-off.
6. **Permanent errors.** `NodeError::Permanent(&'static str)`, constructible on the real-time thread, with `NodeError::is_permanent()`.
   - From `prepare`, `warm` or `process` (the executor bypasses it at once and sets `NodeHealth::permanent`), the node is not restarted automatically.
   - `Controller::retry(id)` clears the record. Switching the node to Standby, or removing it, also clears it (a `Cleared` event), so "turn it off and on again" retries it.
   - Port errors that carry a recoverable flag (capture, output) map to `Permanent` when not recoverable. The node keeps the full error for its own status.
7. **Taking out and restarting.**
   - A scheduled instance that failed, or that a retry superseded, is held without an instance in the next `apply`. It is cooled on the executor and released on the controller.
   - The `apply` after its wait prepares and warms it again. A restart always goes through `release` and `prepare`, so `prepare` after `release` must work as on a fresh instance; this is now part of the node contract in the spec.
   - A waiting node whose wait is over, but that the plan skips for another reason (no sink, starved), is parked and not due until an edit schedules it. This keeps the loop from re-applying forever.
8. **Learning about bypasses without an edit.** The executor bumps a shared `AtomicU64` each time it bypasses a node; that is one relaxed atomic add, only on the failure path. Each retired schedule carries the counter value from its swap, so the controller knows which bypasses its health report covers.
9. **Driving restarts.** `Controller::restart_due()` is true when one of these holds:
   - an unseen bypass happened;
   - a scheduled instance must leave;
   - a held node may be prepared;
   - a retry time or a forgiveness time has passed.

   `Controller::next_due() -> Option<Duration>` says how long the loop may sleep, measured on the controller's own clock. On each wake-up the engine's control loop calls `collect`, then, if `restart_due()`, calls `apply` with the current description. A restart takes up to four rounds: report the bypass, take the node out, prepare it after the wait, and for a failed `warm` one more round to fetch its health.
10. **Events.** `ApplyReport::events` and `Collected::events` carry `RuntimeEvent`s for the event stream:
    - `Failed { node, attempt, cause, retry_at }`, where `retry_at: None` means waiting for the user;
    - `Restarted { node, attempt }`;
    - `Recovered { node }`;
    - `Cleared { node }`.

    `ApplyReport::failed` is a snapshot: it lists every node held without an instance in that schedule (`JoinError::BackingOff { retry_at }` or the `prepare` error), not only new failures. `Controller::fault(id)` and `faulted()` give the current state for the UI.
11. **Semver.** New public types are `#[non_exhaustive]`. The new fields on `ApplyReport`, `Collected` and `NodeHealth` and the new variants of `NodeError` and `JoinError` are additions to `#[non_exhaustive]` types.

## Consequences

- P7 holds for the render executor:
  - failing nodes are isolated without taking neighbours down;
  - they come back on their own;
  - a blocking `prepare` runs at most once per back-off period instead of on every edit.
- `Executor::scheduled()` and `ApplyReport::plan` include held nodes; `Controller::is_running(id)` says whether an instance runs.
- The engine (`rbs-app`) must run the control loop of Decision 9. Until it exists, nodes are restarted only on edits.
- Executor cost: one relaxed atomic add per bypass, a few field writes on the error path, and one relaxed load per swap. The success path of `process` is unchanged. Measured with `cargo bench -p rbs-graph --bench executor` on the reference VM against main, nothing changed beyond noise:
  - tick, 6 nodes: 118 ns;
  - tick, 20 nodes: 474 ns (p = 0.07);
  - tick, 68 nodes: 1.61 µs;
  - `apply` and swap, 32 sources: 56 µs.
- Tests in `crates/rbs-graph/tests/restart.rs` (18) cover:
  - back-off, doubling and saturation;
  - restart without an edit;
  - release before restart;
  - forgiveness, and no forgiveness when the loop is late;
  - permanent errors from `prepare`, `warm` and `process`;
  - giving up, after failed `prepare`s and after bypasses;
  - toggling a node off and on, including a node that failed while scheduled;
  - retry while a schedule is in flight and while the failed instance is scheduled;
  - a node parked by the plan;
  - containment in a source, filter and output chain;
  - removing a failed node.

  `runtime.rs` now checks that a failed `prepare` keeps the dependants scheduled.
- Per-node policies (a capture source with a longer wait than a filter) are not supported yet. They can be added as a lookup by node type without changing the events.
- Time budgets (a node that is too slow, not failing) are still not enforced; when they are, an over-budget bypass reuses this path.

## Alternatives considered

- **Replan without the failed node** (ADR-0020's behaviour for `prepare`, and this ADR's first draft for restarts). It is simpler, but dependants and upstream chains are released and re-prepared on every back-off cycle, which contradicts P7.
- **Restart inside the executor** (`cool`, then `warm` again in place). That would avoid extra rounds, but it cannot call `release` and `prepare` without blocking the real-time thread, and it would keep the instance's possibly broken `prepare` state.
- **Rewrite failed nodes to Standby in the description.** It loses what the scene wants, which ADR-0020 already rejected.
- **A restart supervisor outside `Controller`** (in `rbs-app`) using `Collected::health`. Every composition root would re-implement it, and it would still need the controller to hold nodes without instances and not retry a blocking `prepare` inside `apply`.
- **Send bypass notices over a channel from the executor.** That means one message per bypass and a queue that can fill. A counter is enough, because the health report carries the details.
- **Jitter in the back-off.** Useful when many clients hit one server; here the nodes are local, and determinism helps tests and support. Can be added to the policy later.

## Review

Independent AI session per ADR-0014, 2026-11-03. It got only the proposal, specs and code. Verdict: changes requested. It reproduced four bookkeeping bugs with throwaway tests and found a P7 conflict. All concerns were addressed in the same change:

1. Blocking: switching a node off and on did not retry a node that failed while scheduled; its leaving health created a new record. Fixed with instance epochs (Decision 4), and records are now cleared whenever the scene does not want the node. Test `switching_off_and_on_retries_a_node_that_failed_while_scheduled`.
2. Blocking: stale health undid `retry`. Same fix. Test `retry_while_the_failed_instance_is_scheduled_or_in_flight_is_not_undone`.
3. Blocking: `restart_due` stayed true forever for a node the plan skipped. Fixed by parking (Decision 7). Test `a_node_the_plan_skips_after_its_wait_does_not_keep_the_loop_busy`.
4. Blocking: forgiveness ignored bypasses when the loop was late. Forgiveness now happens only in `collect`, from health (Decision 5). Test `a_flapping_node_escalates_even_when_the_loop_is_late`.
5. Blocking: taking a node out cascaded to its neighbours, against P7. Failed nodes are now held in the plan without an instance (Decision 2). Test `a_failure_stays_contained_to_its_node_in_a_chain`.
6. Blocking: ADR and spec were out of sync. Both rewritten.
7. Forgiveness was noticed only on edits. `restart_due` and `next_due` now include forgiveness times.
8. `next_retry` returned a time on a private clock. Replaced by `next_due() -> Option<Duration>`; the loop order is documented.
9. The round count was wrong. Corrected to up to four.
10. `Permanent` lost information and `warm` ignored it. Added `NodeHealth::last_error`, `NodeError::is_permanent`/`summary`, and `Permanent` from `warm`. Test `a_permanent_warm_error_is_not_restarted`.
11. UI gaps: added the `Cleared` event and documented `failed` as a snapshot.
12. Records of removed nodes lingered. They are now cleared on the apply that removes the node. Test `removing_a_failed_node_drops_its_record`.
13. Hot-path process. The checklist is in the PR; owner sign-off is below.
14. Same instance reused: "prepare after release works as fresh" was added to the node contract.
15. Missing tests: added, as listed above, plus `delay_saturates_at_the_maximum` and `the_policy_gives_up_on_repeated_bypasses_too`.
16. The WIP commit is squashed on merge; the hollow-entry alternative is now the decision.

Owner approval: Salih, 2026-11-03 (early-phase merge policy: merge once CI is green; large reviews later). This also stands in for the P15 human sign-off on the executor change until the owner's later review.
