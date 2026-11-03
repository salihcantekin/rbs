# ADR-0024: Restart failed nodes with back-off, driven by the controller

- Status: Proposed
- Date: 2026-11-03
- Deciders: Salih (owner)

## Context

Principle P7 says a failing node is isolated, restarted with back-off and reported. ADR-0020 implemented the isolation: the executor bypasses a node after `failure_limit` failed ticks, a panic or a failed `warm`, and a failed `prepare` leaves the node idle. It left two gaps (ADR-0020, Decision 7):

- A bypassed node stays in the schedule, holding its resources, until the scene changes. Nothing restarts it.
- A failed `prepare` is retried on every `apply`. A `prepare` that blocks (a device that times out) makes every edit slow.

Forces:

- The executor must stay real-time: no allocation, no locking, no waiting (P6). Restart work (`release`, `prepare`) belongs on the control thread.
- Restarts must not depend on the user editing the scene. A capture card that drops out at 3 a.m. must come back on its own.
- Some errors will not go away by retrying, such as a permission the user has to grant. The capture spec says these are never retried automatically.
- The description's lifecycle state is what the scene wants and is never rewritten by the engine (ADR-0020, Decision 5).
- Tests must not sleep (CLAUDE.md): time comes from an injectable `rbs_core::time::Clock`.

## Decision

1. **The controller owns restart policy.** It keeps one failure record per node: the number of failures in a row, when the node may be prepared again, and whether it has been restarted since. The executor does not change except for reporting.
2. **What counts as a failure:**
   - `prepare` returns an error or panics. `NoInstance` and `DescriptorMismatch` are configuration problems, not failures, and are not recorded.
   - The executor bypassed the node, for any reason it reports in `NodeHealth`: errors in a row, a panic, a failed `warm`, or a permanent error.
   - A bypass is recorded once. The record then waits until the node is restarted, so the same bypassed counters reported by later `collect`s do not count again.
3. **Back-off.** `RestartPolicy { initial, max, max_attempts, stable_after }`.
   - The wait after failure n is `initial * 2^(n-1)`, capped at `max`.
   - Defaults: 0.5 s, doubling up to 30 s, never giving up, forgiven after 10 s.
   - When `max_attempts` is reached, the node is not restarted automatically.
   - A node that has run for `stable_after` since its restart without being bypassed is forgiven: its record is removed, and the next failure starts again at `initial`.
   - Set with `Controller::set_restart_policy`; time comes from `Controller::set_clock` (default `MonotonicClock`).
4. **Permanent errors.** `NodeError::Permanent(&'static str)` is a new variant, constructible on the real-time thread.
   - From `prepare`: no automatic restart.
   - From `process`: the executor bypasses the node at once, without waiting for `failure_limit`, and sets `NodeHealth::permanent`.
   - In both cases the node waits for the user: `Controller::retry(id)` clears the record. Switching the node to Standby and back to Active in the description also clears it, so "turn it off and on again" is a retry without extra API.
5. **Keeping failed nodes out of the plan.** `apply` treats a node as Standby while its record waits: before its retry time, or for the user.
   - It also treats a node as Standby when the node is still scheduled but was bypassed and not yet restarted, even if its retry time has already passed. That takes the node out of the schedule; it is cooled on the executor and released on the controller. It is restarted from a clean state, through `prepare` and `warm`, never by resetting a bypassed instance in place.
   - These nodes appear in `ApplyReport::failed` as `JoinError::BackingOff { retry_at }` (`None`: waiting for the user). Their dependants drop out of the plan as for any Standby node.
   - The description itself is not changed (ADR-0020, Decision 5).
6. **Learning about bypasses without an edit.** The executor bumps a shared `AtomicU64` each time it bypasses a node; that is one relaxed atomic add, only on the failure path. Each retired schedule carries the counter value from its swap, so the controller knows which bypasses its health report already covers.
7. **Driving restarts.** `Controller::restart_due()` is true when one of these holds:
   - an unseen bypass happened;
   - a scheduled node failed and must be taken out;
   - a waiting node's retry time has come.

   The engine's control loop already wakes on a short timer to `collect` (ADR-0020, Consequences). On that timer it checks `restart_due()` and re-applies the current description; `next_retry()` tells it when to wake next. Taking a bypassed node out and restarting it takes up to three apply-and-collect rounds: report the bypass, remove the node, then restart it after the wait.
8. **Events.** `ApplyReport::events` and `Collected::events` carry `RuntimeEvent`s for the engine's event stream:
   - `Failed { node, attempt, cause, retry_at }`, where `cause` is `FaultCause::Join(JoinError)` or `FaultCause::Bypassed(NodeHealth)`;
   - `Restarted { node, attempt }`;
   - `Recovered { node }`.

   They are plain values on the control thread; the event stream itself (control API) is specified with the control API.
9. **Inspection.** `Controller::fault(id) -> Option<FaultStatus>` and `faulted()` show attempts, retry time and whether a node is waiting, for the UI's source status.
10. **Semver.** New public types are `#[non_exhaustive]`. The new fields on `ApplyReport`, `Collected` and `NodeHealth` and the new variants of `NodeError` and `JoinError` are additions to `#[non_exhaustive]` types.

## Consequences

- P7 is implemented for the render executor: failing nodes are released and come back on their own, and a blocking `prepare` runs at most once per back-off period instead of on every edit.
- The engine's control loop gets one more check per wake-up, `restart_due()`, which is cheap. The engine (`rbs-app`) must call it and re-apply. Until that loop exists, nodes are restarted only on edits.
- The executor gains one atomic add per bypass and one atomic load per swap. Measured with `cargo bench -p rbs-graph --bench executor` on the reference VM, against main: ticks of 6, 20 and 68 nodes and `apply` plus swap are unchanged within noise (68 nodes: 1.62 µs both).
- Restart takes the node out for at least one schedule swap. Its video output then has no producer, and consumers see an empty input; the compositor skips that layer, as it does for any producer that is not scheduled.
- The restart wait is measured in master-clock time. Tests drive it with `TestClock`; nine tests in `crates/rbs-graph/tests/restart.rs` cover:
  - back-off and doubling;
  - restart without an edit;
  - release before restart;
  - forgiveness;
  - permanent errors from `prepare` and from `process`;
  - giving up;
  - toggling the node off and on as a retry;
  - a failed `warm`.
- Per-node policies (a capture source with a longer wait than a filter) are not supported yet. They can be added as a policy lookup by node type without changing the events.
- Time budgets (a node that is too slow, not failing) are still not enforced; when they are, an over-budget bypass reuses this path.

## Alternatives considered

- **Restart inside the executor** (call `cool`, `warm` again in place). That would avoid the extra rounds, but it cannot call `release` and `prepare` without blocking the real-time thread, and it would keep the instance's possibly broken `prepare` state.
- **Rewrite failed nodes to Standby in the description.** Simple, but it loses what the scene wants, which ADR-0020 rejected for the same reason.
- **A restart supervisor outside `Controller`** (in `rbs-app`) using `Collected::health`. Possible, but every composition root would re-implement it. It would also need the controller to accept "keep these nodes out" anyway, and it could not stop a blocking `prepare` from being retried inside `apply`.
- **Send bypass notices over a channel from the executor.** That means one message per bypass and a queue that can fill. A counter is enough, because the health report already carries the details.
- **Jitter in the back-off.** Useful when many clients hit one server; here the nodes are local, and determinism helps tests and support. Can be added to the policy later.

## Review

Pending: independent review per ADR-0014.
