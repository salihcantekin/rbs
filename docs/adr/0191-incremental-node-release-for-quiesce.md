# ADR-0191: Bounded control-thread service and quiesce

- Status: Superseded by ADR-0193
- Date: 2026-11-25
- Deciders: Salih (owner)
- Supersedes in part: ADR-0020 decisions 1, 4 and 7, for parked quiesce swaps and release only
- Supersedes in part: ADR-0013 rule 5, for retained quiesce ownership after failure
- Supersedes in part: ADR-0030 decision 3 and ADR-0190 decisions 1, 3 and 5, for parked non-tick service and quiesce only
- Builds on: [ADR-0190](0190-prepare-group-resources-on-executors.md)

## Context

ADR-0190 requires nonblocking, pipeline-ordered quiesce on the existing
studio control thread, with the old engine and GPU pool retained until
retirement is proved. Today `Controller::collect` calls `Node::release` there;
[ADR-0020](0020-node-contract-and-schedule-swap.md) permits that call to block.
Avoiding a worker join therefore does not alone keep quiesce responsive.
Moving the node to a new thread would change control-thread affinity, and
neither a timeout nor a failed release proves its resources are gone.
The parked executor's legacy `attach` and `detach` may also block, and
ordinary service can wait for swaps, join workers, invoke other legacy hooks,
or drop objects with blocking destructors. Incremental release alone cannot
make that service path nonblocking.

## Decision

1. **Incremental contract.** For quiesce, a node opts into a separate
   control-thread release step returning complete, runnable continuation,
   pending external progress, unsupported, or typed failure. Each call must
   do bounded nonblocking work and return; pending work registers a wake before
   returning. The controller retains exclusive ownership of the same node,
   release state and required context across steps. A step must preserve that
   ownership on error or panic; completion must provide the same reusable
   postcondition as `release`. GPU/pool retirement proof remains separate.
2. **Compatibility.** The new object-safe node method has a default
   `Unsupported` result that performs no release, so existing Rust node
   implementations still compile. Quiesce must never invoke legacy `release`
   as a fallback, even for an apparently trivial node; it reports unsupported
   and retains ownership instead. Ordinary collect, apply, draining apply and
   terminal shutdown keep their existing blocking release behavior. No plugin
   ABI change or automatic plugin capability is implied.
3. **Bounded parked attachment.** Parked non-tick service and parked
   quiesce must not call legacy `Runtime::attach` or `detach`, nor assume
   their empty defaults imply a capability. A runtime explicitly opts into
   separate, paired bounded nonblocking parked attach and detach hooks;
   the default is `Unsupported`, without side effects. Attach, each permitted
   executor action, and detach must return and preserve ADR-0030's same-thread
   save/restore and exclusive executor ownership. The runtime must make
   failure and panic paths restore the previous control-thread context and
   preserve ownership without waiting. If restoration cannot be proved, the
   control thread is contaminated: report terminal failure, retain its owners
   and do not run context-dependent work as though restoration succeeded.
   Never start an idle worker to avoid these requirements. An unsupported
   parked preparation is refused before acceptance; already accepted work or
   quiesce that cannot proceed is failed and quarantined with its owner and
   possible charge, not acknowledged as retired. Ordinary worker startup,
   swaps and shutdown retain ADR-0030's legacy hooks.
4. **Order and progress.** Quiesce takes returned schedules without the
   blocking collector, retains their nodes, slots and dependencies, then
   advances release in bounded steps on the **same bound control thread**.
   Final ticks, cooling and completed release of each producer partition
   precede downstream drains as in ADR-0190. No new idle worker, control
   thread, per-tick work or healthy-idle polling is introduced; pending
   retirement may poll as ADR-0190 permits. Pending external work wakes
   control after publishing its state; generation checks prevent lost or stale
   wakes. A timed-out worker drain remains pending as ADR-0190 requires.
5. **Explicit service boundary.** A quiescing old studio uses a dedicated
   bounded path, not ordinary `Engine::service`, `Studio::service`,
   `Controller::collect`, `apply`, `settle`, `Worker::drain`, `Worker::stop`
   or terminal shutdown. It issues a drain once and observes its published
   completion without a wait or join, including after timeout; a worker
   retains its executor while running. Per host round the quiesce path performs
   at most one fixed-size state-machine action, including bounded collection,
   graph-order traversal and reclamation; no action hides a graph-sized walk,
   a blocking callback, a live-thread join or an unverified destructor.
   An already in-flight swap is observed without reissuing it; a parked swap
   runs only when every required runtime and node hook has an explicit bounded
   capability. Unsupported work fails closed with ownership retained.
   Worker final ticks and cooling retain their executor affinity and pipeline
   ordering; their completion is observed without blocking control. Host
   handling while an old studio quiesces must gate operations that could
   re-enter these legacy paths; this decision does not claim that every
   unrelated host command is nonblocking.
6. **Failure and retention.** An unsupported node, runtime hook or
   reclamation operation, failed or panicking step,
   or release deadline produces a distinct typed quiesce outcome, not a
   completed release, cancellation or GPU proof. A deadline at most
   `swap_timeout` after release starts must not cause another call to legacy
   release, a join, a drop of the incomplete node, or downstream advancement.
   After timeout, retain the node and its whole owning studio, engine, pool,
   dependencies and unproved charges on the original control thread. Suspend
   automatic runnable continuations; a later genuine readiness wake may drive
   one recovery window of at most `swap_timeout`, without repeating completed
   steps. If it still has not completed, suspend again until a new readiness
   wake. An operation that never wakes stays retained without periodic retries.
   A failed, panicked or unsupported release is terminally quarantined, not
   automatically retried.
   Diagnostics must identify the node and reason.
7. **Bound retained cost.** While an old studio has uncompleted node release,
   or has uncompleted quiesce or unproved retirement after a failure, a
   successor must refuse new group preparations and the host must refuse
   further studio replacements; this prevents accumulating multiple retained
   studios. Retention for failures is a narrow
   exception to ADR-0013 rule 5, not a normal Installed or Standby cost.
   Process termination is the final boundary if safe release cannot be proved.
   Device-loss quarantine still clears its own pending service and does not
   poll; independent release wakes do not prove GPU retirement.
8. **Limit of the guarantee.** A hook, driver call, destructor or other
   operation that violates the bounded nonblocking contract and never returns
   cannot be timed out or preempted by its own control thread. A deadline is
   observed only between returning actions, not a hard wall-time guarantee.
   The engine must not claim otherwise; the ownership guarantee for a
   pending step assumes each invoked step returns. Such a violation needs
   separate isolation or process-level recovery, not an unsafe forced drop.

## Consequences

- Update the planned [media graph](../specs/media-graph.md),
  [engine](../specs/engine.md), [compositor](../specs/gpu-compositor.md) and
  [performance](../specs/performance-budgets.md) contracts before
  implementation. This ADR is accepted; those additions are not implemented.
- Each built-in or plugin bridge needed by quiesce must explicitly implement
  incremental release; until then quiesce fails closed instead of blocking.
  Returned schedules, slot destruction and node destructors must not hide a
  blocking operation in a supposedly bounded step. Existing GPU allocation,
  polling and pool destruction have no established bounded-call guarantee:
  the GPU runtime cannot advertise bounded parked service until each required
  operation is proved or replaced. Group Scroll remains fail-closed.
- Deterministic tests must cover all outcomes, partially released nodes,
  release and wake races, deadlines and late progress, hanging workers,
  unsupported legacy nodes and runtime hooks, attach/detach restoration,
  no legacy call or premature join, bounded per-round work, pipeline order,
  retained ownership and charges, and unchanged ordinary shutdown and draining
  apply. A genuinely blocking step cannot be tested as a recoverable
  control-thread timeout.

### Current runtime hook audit

The in-tree production runtimes are `()` in `rbs-graph/src/runtime/mod.rs`
and `GpuRuntime` in `rbs-gpu/src/node.rs`; both inherit the current no-op
attach and detach. Test runtimes `Other`, `Spy` and `Ticks` do too. The test
runtime `Affine` overrides both to check same-thread binding. None yet opts
into the planned hooks. External runtime implementations may do system calls
and cannot be inferred safe from the default. GPU `end_tick` calls importer
retirement, pool trimming and device polling on the worker; parked GPU service
has not established a bound for driver calls, callback execution, pool scans or
resource destruction. The current whole-plan preparation also needs splitting.
No supported runtime or GPU service capability is claimed by this audit.

## Alternatives considered

- **Call legacy release from quiesce.** Rejected: it can block the control
  thread indefinitely, so the timeout cannot run.
- **Move release to a helper thread.** Rejected: it moves control-thread-bound
  node state and changes the existing ownership and affinity contract.
- **Call legacy attach and detach for parked work.** Rejected: ADR-0030
  permits system calls and offers no boundedness or failure-atomic guarantee.
- **Drop or detach on timeout.** Rejected: it loses ownership without proving
  release and may free resources still in use.

## Review

- Independent review, 2026-11-25: changes requested. Parked executor
  `attach` and `detach` were not bounded, so incremental node release alone
  could not establish nonblocking quiesce. The revision requires explicit
  bounded parked hooks, a dedicated bounded service path and fail-closed
  retention for unsupported participants; it does not claim that a timeout
  can interrupt a stuck same-thread call.
- Independent re-review, 2026-11-25: approved, with no blocking findings.
- Owner approval: Salih, 2026-11-25, including the stated fail-closed GPU
  limitation.
