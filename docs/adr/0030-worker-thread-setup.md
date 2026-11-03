# ADR-0030: Worker thread setup: a thread policy port and a runtime attach hook

- Status: Proposed
- Date: 2026-11-03
- Deciders: Salih (owner)
- Amends: [ADR-0027](0027-engine-loop.md) Decision 2 (`Local` must not be thread-affine, no attach hook) and Decision 10 (thread priorities and high-resolution timers out of scope).

## Context

ADR-0027 runs each partition on its own worker thread at the platform's default class and leaves two gaps open (spec `docs/specs/engine.md`, open questions):

- **Scheduling class and timers.** Render and encoders must finish each frame inside its period while the desktop, the UI and capture threads compete for CPUs. Each OS has its own mechanism: MMCSS on Windows, `THREAD_TIME_CONSTRAINT_POLICY` on macOS, `SCHED_FIFO`/`SCHED_RR` on Linux (through rtkit when the process lacks the privilege). Timer precision differs too: Windows rounds `park_timeout` to its 15.6 ms default tick unless the timer resolution is raised, and Linux adds 50 us of timer slack per wake-up by default. All of these need `unsafe` OS calls, which CLAUDE.md keeps out of `rbs-engine` (rule 7) and P1 keeps out of core crates.
- **Thread-bound runtime resources.** A worker stops when its partition runs nothing and starts again on a new OS thread (ADR-0013), carrying `X::Local` with it. A runtime that needs a per-thread context (a COM apartment, a current CUDA or GL context) has no point to bind it on the new thread and unbind it on the old one.

Forces: zero cost when unused (P13: no policy means no call); the worker loop is a hot path (P6, P15) and must not change; one failing OS call must not stop a partition; tests must run without privileges on CI.

## Decision

1. **Port in `rbs-core`** (`rbs_core::thread`): `ThreadPolicy: Send + Sync` with one method, `enter(&ThreadRequest) -> Result<ThreadGuard, ThreadPolicyError>`, called on the thread the request is for. `ThreadRequest { name, class }`, where `ThreadClass` is `Periodic { period }` or `Responsive` (both `#[non_exhaustive]`, so an audio class can be added). The request describes what the thread does, never an OS mechanism (P3). `ThreadGuard` carries a `ThreadGrant` (`Unchanged`, `Raised`, `RealTime`) and an optional restore closure run on drop; it is neither `Send` nor `Sync`, so the undo runs on the same thread. The port lives in the foundation crate, not a new port crate, because the engine (which may depend only on `rbs-core` and `rbs-graph`) is its only caller and it has no types beyond these.
2. **Engine wiring.** `Engine::set_thread_policy(Option<Arc<dyn ThreadPolicy>>)`, `None` by default; it applies to workers started afterwards. A worker calls `enter` on its own thread before its first tick, with `Periodic { period: cadence period }` for `Pacing::Cadence` and `Responsive` for `Pacing::Doorbell`, and drops the guard on that thread after its last tick. The call is made once per worker start, outside the loop; the loop itself does not change. An error or a panic in `enter` is caught: the worker runs at the default class, `WorkerStats::policy_errors` counts it and `WorkerStats::thread_grant` is `None`; otherwise `thread_grant` reports the grant.
3. **Runtime attach hook in `rbs-graph`.** `Runtime` gains `attach(shared, local)` and `detach(shared, local)`, empty by default; `Executor::attach()` / `Executor::detach()` call them inside `catch_unwind`. Every thread that ticks or swaps an executor attaches it first and detaches it before the executor leaves: a worker after `enter` and before its first tick, and before dropping its guard; the control thread around a swap it makes without a worker (`Executor::swap_pending` on an idle or closing partition). The hooks are not real-time. They are infallible: a runtime that cannot bind records that in `Local` and its nodes return `NodeError::NotReady`.
4. **One OS adapter crate, `rbs-thread-os`,** implements the port with `OsThreadPolicy` (per-OS modules; all `unsafe` lives there, each block with a `// SAFETY:` comment):
   - Windows: `timeBeginPeriod(1)` for every class (undone with `timeEndPeriod(1)`); `Periodic` joins the MMCSS task "Games" at `AVRT_PRIORITY_HIGH`, `Responsive` gets `THREAD_PRIORITY_ABOVE_NORMAL`.
   - macOS: `Periodic` gets `THREAD_TIME_CONSTRAINT_POLICY` with computation half the period (50 us to 50 ms) and constraint the period, preemptible; undone with `THREAD_STANDARD_POLICY`. `Responsive` gets QoS user-interactive, undone to the previous class.
   - Linux: per-thread timer slack of 1 ns for every class; `Periodic` tries `SCHED_FIFO` at a low priority (10), then rtkit (`MakeThreadRealtime` over the system D-Bus, feature `rtkit`, default on), then nice -10; `Responsive` tries nice -5. Restores the previous policy, priority and slack.
   - `OsThreadConfig { realtime: false }` keeps every thread out of real-time classes and only raises priorities and timers.
   One crate rather than one per OS because the three modules share the mapping and composition roots then need one dependency; each module compiles only on its OS.
5. **Composition roots own the choice.** `rbs-app` and `rbs-cli` decide whether to pass `OsThreadPolicy` to the engine; `rbs-engine` never names `rbs-thread-os`.

## Consequences

- Render and encoders can run in a real-time class with 1 ms (Windows) or 1 ns-slack (Linux) timers; a missing privilege degrades to a raised priority, never to an error that stops the engine.
- Runtimes can hold thread-bound contexts in `Local`; spec `docs/specs/engine.md` drops the "must not be thread-affine" rule and the open questions on priorities and the attach hook; `docs/specs/media-graph.md` lists the hooks.
- rtkit requires `RLIMIT_RTTIME`; the Linux adapter lowers it to rtkit's maximum for the whole process the first time it asks rtkit, and does not raise it again. A real-time thread that runs that long without blocking gets `SIGXCPU`; worker loops block every tick.
- `timeBeginPeriod(1)` raises the system timer rate while any worker runs (per process since Windows 10 2004); it ends when the last worker stops, so an idle engine costs nothing (P13).
- MMCSS "Games" and the macOS computation budget are first guesses; per-partition tuning (an audio class, encoder-specific budgets) can be added to `ThreadClass` later without breaking the port.
- Tests: the engine test checks that each worker enters the policy on its own thread, that the guard is dropped there, that a refusal leaves the worker ticking, and that every thread that ticks or swaps a runtime attached it first. The adapter test enters both classes on a fresh thread on each CI OS and requires a clean grant or refusal; real-time grants themselves are not verified on CI (no privileges).

## Alternatives considered

- **Thread setup inside `rbs-engine` behind `cfg`:** puts `unsafe` FFI and vendor-like OS mechanisms into an engine crate (CLAUDE.md rule 7, P1).
- **A hook closure on `Pacing` or per partition:** simpler type, but couples pacing to platform setup and gives no restore point; a policy object also lets the composition root share one configuration.
- **A separate port crate `rbs-thread`:** an extra crate and a new engine dependency (a layering change) for five small types.
- **Waitable high-resolution timers on Windows (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`) instead of `timeBeginPeriod`:** more precise and process-neutral, but the worker waits in `park_timeout`/`Doorbell::wait`; replacing the wait is a hot-path change for a later ADR if measurements ask for it.
- **Thread-affine `Local` created per tick:** costs work on every tick and cannot hold a context across ticks.

## Review

Pending: independent review per ADR-0014.
