# ADR-0190: Prepare group resources on their owning executor

- Status: Superseded by ADR-0193
- Date: 2026-11-25
- Deciders: Salih (owner)
- Supersedes in part: ADR-0154 decision 4, for pending executor work
- Supersedes in part: ADR-0013 rule 5, for unprovable group-resource retirement after failure
- Builds on: [ADR-0185](0185-scroll-a-group-as-one-picture.md),
  [ADR-0187](0187-version-group-scroll-settings.md)

## Context

Group Scroll edits need resources prepared before the edit is published, but
the texture pool belongs to the render executor. The current schedule swap
does not acknowledge that preparation before publication. A worker can also
be absent while its executor is parked on the control thread. The
[compositor spec](../specs/gpu-compositor.md#planned-scrolling-groups-adr-0185-adr-0188)
requires bounded requests, cancellation and studio-wide accounting; it does
not specify how to service them in either case.

The engine must retain exclusive executor ownership, not block the UI or
render callback, and not add group-specific per-frame work while the feature
is unused. A pending retirement must progress even with no running worker.

## Decision

1. **Dispatch.** Add a default no-op, non-tick service hook to `Runtime` and
   an `Executor` entry point to invoke it with that executor's shared and
   local state. The GPU runtime owns a typed group-work mailbox; neither
   `rbs-engine` nor `rbs-graph` names GPU types or accepts arbitrary work
   closures. A control-side operation reserves its budget before submitting
   an identified request. Each executor accepts at most one outstanding
   preparation; a busy or closed mailbox returns a typed error without
   publishing the edit. Cancellation has an independent signal so a full
   preparation slot cannot block it. The existing render worker services
   signalled work between ticks on the executor that owns the pool, without
   adding a tick. A parked executor services it on the control thread without
   a tick, attaching and detaching as ADR-0030 requires. No second thread may
   use the executor or pool. Requests survive an executor handover.
2. **No inactive work.** Dispatch is enabled only while there is executor work
   to service. It must not introduce a per-frame check for an inactive group
   feature, create an idle worker or poll a healthy idle studio. The worker's
   existing mode signal must carry the service request, preserving simultaneous
   drain, stop and wake events; normal cadence ticks must not query a separate
   group mailbox. The typed GPU hook processes at most one preparation or one
   bounded retirement step per service call, outside `Node::process`, without
   waiting for GPU completion.
3. **Publish or cancel.** Prepared resources remain held by their originating
   executor until committed or cancelled. A prepared reply alone does not
   publish the edit: the control thread rechecks the complete prospective
   state and permissions before commit. Cancellation is identified by the
   same operation id and is acknowledged only after the executor has
   relinquished the plan. A late reply cannot publish a cancelled or stale
   edit. Each accepted operation has a control-side deadline, bounded by the
   engine's configured `swap_timeout`: if the executor does not acknowledge
   in time, return a typed timeout and mark it abandoned, not cancelled
   successfully. Keep its ownership and possible charge quarantined; reject
   conflicting edits and never reuse its id or resources. If it later returns,
   discard its result and retire any allocation. Queue saturation, shutdown,
   device loss and failed preparation also return typed failures. A hung node
   can still block engine shutdown under the existing engine contract; this
   decision does not claim to make a stuck worker joinable.
4. **Retirement.** A cancellation or failure releases the portion of a
   reservation for which no allocation exists. Every allocated intermediate,
   including a prepared, retained-free or in-flight allocation, remains
   charged until GPU completion and removal from the originating pool prove
   that allocation was destroyed or reassigned to non-group use. An
   acknowledgment of cancellation, a timeout or a device-loss report alone is
   not that proof. Retirement ownership and its charge survive failed edits,
   worker handover and studio replacement. On device loss, keep the old
   device, pool ownership and charges quarantined unless destruction or
   reassignment is actually proved; a new device or studio does not reset
   those charges. The studio host owns the lifetime-wide ledger and retains
   the entire old studio, including its engine, on the same bound control
   thread until retirement completes. It never takes the pool from a running
   worker or rebinds `ControlWake`. Before final shutdown, a new nonblocking
   engine quiesce phase drains schedules in existing pipeline order, while
   rejecting new graph work and keeping each executor and pool available for
   non-tick retirement. Only after every required retirement is proved may
   final shutdown drop the old engine. A hung worker cannot block the host's
   control thread during quiesce: on `swap_timeout`, quiesce reports the
   timeout but does not join that worker; it continues once the worker returns.
   A timed-out drain remains pending until its completion is observed; it is
   not cancelled or restarted merely because the deadline passed.
   The existing terminal shutdown and draining-apply timeout behaviour is
   unchanged. The charge stays quarantined. While an old studio is
   quarantined, a successor refuses
   new group preparations, so another abandoned studio cannot accumulate.
   If device loss prevents proof, retain the old owner and charges without
   periodic retries; process termination is the final boundary. This
   failure-only retention is an explicit exception to ADR-0013 rule 5, not a
   normal Installed or Standby cost, and must be visible in diagnostics.
5. **Progress.** While executor work or retirement is pending,
   `Engine::service` and its caller must arrange bounded progress and wakeups,
   including for a parked executor and wgpu callbacks requiring a nonblocking
   device poll. An engine-owned, runtime-independent service state is
   registered with each executor. A typed producer marks its work pending
   before waking the engine; the hook reports completion or that retirement
   needs another poll. The engine includes pending state and deadlines in its
   wait calculation until completion, without inspecting GPU types. Publishing
   a reply or completion updates the state before waking the control thread;
   the wake and final before-park recheck obey ADR-0154's lost-wake rules.
   `Engine::service` requests a non-tick hook on the owning
   active executor; for a parked executor it attaches, invokes the hook and
   detaches on the control thread. Each poll is nonblocking. A pending
   operation or retirement may cap `service().wait` by `control_interval` or
   an earlier operation deadline; this is the only extension to ADR-0154
   decision 4. No wait or poll remains when that work is gone. Device-loss
   handling must terminate requests without claiming successful retirement
   without proof.

## Consequences

- Define the exact mailbox, ownership-transfer, wake and terminal-state
  contracts in the engine, media-graph and compositor specs before
  implementation; preserve the engine-to-graph dependency direction.
- Deterministic tests must cover concurrent reservations, a full mailbox,
  active and parked executors, wake/park races, shutdown and device loss,
  cancellation racing preparation, stale edits, and retirement after GPU and
  pool completion. Test that idle and no-group paths do no extra group work.
- Worker-loop changes require hot-path measurements, including render lateness
  during preparation, independent review and human approval. The group Scroll
  fail-closed gate remains until the complete resource, snapshot, API and UI
  contracts are integrated.
- Specify nonblocking quiesce, owner retention, failure diagnostics and
  their stop behaviour; verify that an unused feature starts no thread.

## Alternatives considered

- **A worker for an empty partition.** Rejected: it conflicts with ADR-0027
  decision 2 and adds a thread while nothing is scheduled.
- **Control thread mutates an active executor's pool.** Rejected: it violates
  exclusive ownership and can race rendering.
- **Publish before `warm` and report failure later.** Rejected: a user edit
  could commit without its required resources.
- **Keep polling all running executors.** Rejected: idle studios would pay
  for an unused feature and ADR-0154's idle-wake improvement would be lost.

## Review

Independent AI review, 2026-11-25: changes requested. The reviewer asked for
typed dispatch to active and parked executors, bounded timeout behaviour for
an unresponsive worker, and a non-tick retirement and device-loss ownership
path. On re-review, the timeout was resolved; the reviewer requested an
engine-visible progress signal and a quarantine owner beyond executor lifetime.
On the next review the proposed reaper conflicted with engine thread affinity
and shutdown order; permanent failure retention also needed an explicit
ADR-0013 exception. Decision 4 now proposes same-thread retention and a
separate quiesce phase.

Independent AI follow-up review, 2026-11-25: approve. Before implementation,
the engine contract must distinguish quiesce from terminal shutdown, keep
retirement service and pools available, reject new graph edits, and preserve
pipeline-ordered final ticks and swap-timeout behaviour. Owner approval of
this ADR: Salih, 2026-11-25.
The owner chose a typed timeout with fail-closed resource quarantine instead of
an indefinite wait on 2026-11-25.
