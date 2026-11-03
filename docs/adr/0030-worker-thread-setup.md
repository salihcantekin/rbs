# ADR-0030: Worker thread setup: a thread policy port and a runtime attach hook

- Status: Accepted
- Date: 2026-11-03
- Deciders: Salih (owner)
- Amends: [ADR-0027](0027-engine-loop.md) Decision 2 (`Local` must not be thread-affine, no attach hook) and Decision 10 (thread priorities and high-resolution timers out of scope).

## Context

ADR-0027 runs each partition on its own worker thread at the platform's default class and leaves two gaps open (spec `docs/specs/engine.md`, open questions):

- **Scheduling class and timers.** Render and encoders must finish each frame inside its period while the desktop, the UI and capture threads compete for CPUs. Each OS has its own mechanism: MMCSS on Windows, `THREAD_TIME_CONSTRAINT_POLICY` on macOS, `SCHED_FIFO`/`SCHED_RR` on Linux (through rtkit when the process lacks the privilege). Timer precision differs too: Windows rounds `park_timeout` to its 15.6 ms default tick unless the timer resolution is raised, and Linux adds 50 us of timer slack per wake-up by default. All of these need `unsafe` OS calls, which CLAUDE.md keeps out of `rbs-engine` (rule 7) and P1 keeps out of core crates.
- **Thread-bound runtime resources.** A worker stops when its partition runs nothing and starts again on a new OS thread (ADR-0013), carrying `X::Local` with it. A runtime that needs a per-thread context (a COM apartment, a current CUDA or GL context) has no point to bind it on the new thread and unbind it on the old one.

Forces: zero cost when unused (P13: no policy means no call); the worker loop is a hot path (P6, P15) and must not change; one failing OS call must not stop a partition; tests must run without privileges on CI.

## Decision

1. **Port in `rbs-core`** (`rbs_core::thread`): `ThreadPolicy: Send + Sync` with one method, `enter(&ThreadRequest) -> Result<ThreadGuard, ThreadPolicyError>`, called on the thread the request is for. `ThreadRequest { name, class }`, where `ThreadClass` is `Periodic { period }` (built with `ThreadClass::periodic`) or `Responsive`; the enum and the `Periodic` variant are `#[non_exhaustive]`, so an audio class or a compute budget can be added in a minor release. The request describes what the thread does, never an OS mechanism (P3). `ThreadGuard` carries a `ThreadGrant` (`Unchanged`, `Raised`, `RealTime`) and an optional restore closure run on drop; a `PhantomData<*const ()>` makes it neither `Send` nor `Sync` (checked by a `compile_fail` doctest), so the undo runs on the same thread. `enter` must return within about a quarter second whatever the system does: every IPC call it makes is bounded by a timeout. The port lives in the foundation crate, not a new port crate, because the engine (which may depend only on `rbs-core` and `rbs-graph`) is its only caller and it has no types beyond these.
2. **Engine wiring.** `Engine::set_thread_policy(Option<Arc<dyn ThreadPolicy>>)`, `None` by default; it applies to workers started afterwards. A worker calls `enter` on its own thread before its first tick, with `Periodic { period: cadence period }` for `Pacing::Cadence` and `Responsive` for `Pacing::Doorbell`, and drops the guard on that thread after its last tick. The call is made once per worker start, outside the loop; the loop itself does not change. A panic in the guard's restore is caught too, so the executor is never lost. An error or a panic in `enter` is caught: the worker runs at the default class, `WorkerStats::policy_errors` counts it and `WorkerStats::thread_grant` is `None`; otherwise `thread_grant` reports the grant.
3. **Runtime attach hook in `rbs-graph`.** `Runtime` gains `attach(shared, local)` and `detach(shared, local)`, empty by default; `Executor::attach()` / `Executor::detach()` call them inside `catch_unwind`. Every thread that ticks or swaps an executor attaches it first and detaches it before the executor leaves: a worker after `enter` and before its first tick, and before dropping its guard; the control thread around a swap it makes without a worker (`Executor::swap_pending` on an idle or closing partition, also after a `DrainTimeout`). When a partition is dropped, the control thread retires the parked executor (`Executor::retire`: attach, cool the nodes still warm, detach, then drop). `attach` on the control thread saves a context already current there and `detach` restores it. Thread-bound state is released in `detach`; `Local`'s own `Drop` runs unattached (also when a worker thread dies from a panic outside the executor's catch) and must not rely on a bound context. The hooks are not real-time. They are infallible: a runtime that cannot bind records that in `Local` and its nodes return `NodeError::NotReady`.
4. **One OS adapter crate, `rbs-thread-os`,** implements the port with `OsThreadPolicy` (per-OS modules; all `unsafe` lives there, each block with a `// SAFETY:` comment):
   - Windows: `timeBeginPeriod(1)` for every class (undone with `timeEndPeriod(1)`); `Periodic` joins the MMCSS task "Games" at `AVRT_PRIORITY_HIGH`, `Responsive` gets `THREAD_PRIORITY_ABOVE_NORMAL`.
   - macOS: `Periodic` gets `THREAD_TIME_CONSTRAINT_POLICY` with computation half the period (50 us to 50 ms) and constraint the period, preemptible; undone with `THREAD_STANDARD_POLICY`. If the kernel refuses, it falls back to QoS user-interactive like the other platforms. `Responsive` gets QoS user-interactive, undone to the previous class (the default class when it had none).
   - Linux: per-thread timer slack of 1 ns for every class; `Periodic` tries `SCHED_FIFO` at a low priority (10), then rtkit (`MakeThreadRealtime` over the system D-Bus with a 250 ms method timeout on one cached connection, feature `rtkit`, **off by default**), then nice -10; `Responsive` tries nice -5. Restores the previous policy, priority and slack.
   - `OsThreadConfig { realtime: false }` keeps every thread out of real-time classes and only raises priorities and timers.
   One crate rather than one per OS because the three modules share the mapping and composition roots then need one dependency; each module compiles only on its OS.
5. **Composition roots own the choice.** `rbs-app` and `rbs-cli` decide whether to pass `OsThreadPolicy` to the engine; `rbs-engine` never names `rbs-thread-os`.

## Consequences

- Render and encoders can run in a real-time class with 1 ms (Windows) or 1 ns-slack (Linux) timers; a missing privilege degrades to a raised priority, never to an error that stops the engine.
- Runtimes can hold thread-bound contexts in `Local`; spec `docs/specs/engine.md` drops the "must not be thread-affine" rule and the open questions on priorities and the attach hook; `docs/specs/media-graph.md` lists the hooks.
- rtkit requires `RLIMIT_RTTIME`. With feature `rtkit`, the Linux adapter lowers it for the whole process (soft limit half of rtkit's maximum, hard limit the maximum) once rtkit answered, and never raises it again. A cadence worker that is late ticks again without blocking, so a sustained overload or a node that spins can exceed the limit: `SIGXCPU` first (which by default ends the process), then `SIGKILL`. That is why the feature is off by default; a composition root that enables it must handle `SIGXCPU` (for example demote the thread, as PipeWire does). `SCHED_FIFO` itself is bounded by the kernel's real-time throttling (95 % per second by default).
- `timeBeginPeriod(1)` raises the system timer rate while any worker runs (per process since Windows 10 2004); it ends when the last worker stops, so an idle engine costs nothing (P13). Windows 11 may ignore it for a process without a visible window, so a headless `rbs-cli` can keep the coarse timer; `SetProcessInformation(ProcessPowerThrottling)` or waitable high-resolution timers are the follow-up if that shows in measurements.
- While a thread is registered with MMCSS, Windows applies `NetworkThrottlingIndex` (10 packets per ms by default, about 120 Mbit/s at full-size packets) to non-multimedia network traffic system-wide; a stream output's bitrate is far below that, but a user with a fast upload and other traffic may notice. A thread that uses up the "Games" task's CPU share drops to low priority for the rest of the period.
- Semver: the `rbs-core` additions are new items; `Runtime` gains two methods with empty defaults, so implementations keep compiling (minor).
- MMCSS "Games" and the macOS computation budget are first guesses; per-partition tuning (an audio class, encoder-specific budgets) can be added to `ThreadClass` later without breaking the port.
- Measured (`cargo bench -p rbs-thread-os --bench wakeup`, Linux container with the privilege for `SCHED_FIFO`, 2000 wake-ups at 1 kHz, all CPUs kept busy by twice as many normal-priority spinners when loaded): lateness after the deadline

  | | p50 | p99 | max |
  | --- | --- | --- | --- |
  | idle, default class | 80.1 us | 146.2 us | 670.1 us |
  | idle, `OsThreadPolicy` (RealTime) | 33.3 us | 77.1 us | 187.1 us |
  | loaded, default class | 66.7 us | 3904.9 us | 6939.2 us |
  | loaded, `OsThreadPolicy` (RealTime) | 15.3 us | 47.9 us | 527.2 us |

  The worker loop is unchanged (one optional call per worker start), so the engine's `cross_thread` numbers are unaffected. Windows and macOS numbers are to be taken on the owner's machines.
- Tests: the engine test checks that each worker enters the policy on its own thread, that the guard is dropped there, that a refusal leaves the worker ticking, and that every thread that ticks or swaps a runtime attached it first; another engine test makes `enter` and the restore panic and checks that no executor is lost. The adapter's tests read back the timer slack, scheduling policy and (macOS) QoS class after the guard is dropped. The adapter test enters both classes on a fresh thread on each CI OS and requires a clean grant or refusal; real-time grants themselves are not verified on CI (no privileges).

## Alternatives considered

- **Thread setup inside `rbs-engine` behind `cfg`:** puts `unsafe` FFI and vendor-like OS mechanisms into an engine crate (CLAUDE.md rule 7, P1).
- **A hook closure on `Pacing` or per partition:** simpler type, but couples pacing to platform setup and gives no restore point; a policy object also lets the composition root share one configuration.
- **A separate port crate `rbs-thread`:** an extra crate and a new engine dependency (a layering change) for five small types.
- **Waitable high-resolution timers on Windows (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`) instead of `timeBeginPeriod`:** more precise and process-neutral, but the worker waits in `park_timeout`/`Doorbell::wait`; replacing the wait is a hot-path change for a later ADR if measurements ask for it.
- **Thread-affine `Local` created per tick:** costs work on every tick and cannot hold a context across ticks.

## Review

Reviewer: independent AI session (given only the ADR, the diff, principles, specs and code; architecture-review and risky-review skills)
Date: 2026-11-03
Verdict (first round): changes requested. Re-review of the fixes: ten of eleven resolved, item 4 changes requested (fixed as noted), item 6 open for the owner's human hot-path sign-off. Layering, P13, restore order and thread, and the SAFETY comments were checked and held; clippy clean on Linux, Windows and macOS targets.
Concerns:
1. (High) rtkit's `RLIMIT_RTTIME` (soft = hard) can kill the process when a late cadence worker keeps ticking -> feature `rtkit` off by default; soft limit half the hard one; lowered only once rtkit's properties were read (rtkit needs it before `MakeThreadRealtime`, so it stays lowered if rtkit then refuses); Consequences corrected.
2. `enter` could hang on a stuck D-Bus; a connection per worker start -> one cached connection with a 250 ms method timeout; the port documents the bound for `enter`.
3. A panicking restore lost the executor -> the guard drop is caught; test `a_panicking_thread_policy_loses_no_executor`.
4. `Executor`/`Local` dropped unattached -> new `Executor::retire` (attach, cool, detach, drop) used by `Partition::drop`; release in `detach`, `Local::drop` thread-agnostic, documented in `Runtime::attach` and here. (Re-review: an attach without detach broke the save/restore rule; fixed with `retire`.)
5. Windows side effects unstated -> MMCSS network throttling, the task's CPU share and `timeBeginPeriod` in windowless processes listed in Consequences and the module docs; "Games" kept as the first choice.
6. No numbers -> wake-up bench added (table above, Linux); Windows/macOS numbers and the human hot-path approval follow in the owner's review round (early-phase merge policy).
7. `Periodic` could not gain fields -> variant `#[non_exhaustive]` with `ThreadClass::periodic`; semver stated.
8. `!Send` by accident -> `PhantomData<*const ()>` and a `compile_fail` doctest.
9. macOS did not fall back and could restore "unspecified" -> falls back to QoS; restores the default class; test reads the class back.
10. Risky paths untested -> Linux test reads back slack, policy and nice value; macOS test reads back QoS; panic tests in the engine. Inline control-thread swaps go through the same attached path as before (`Part::settle`).
11. `prctl` zero arguments and overview rows -> `0 as c_ulong`; Foundation row lists the port; adapter row ordered.

Accepted 2026-11-03 under the owner's early-phase ADR policy, after the re-review's remaining change (item 4) was made as the reviewer proposed. Open for the owner: the human hot-path sign-off and Windows/macOS wake-up numbers (item 6).

- Record, 2026-11-03: the owner (Salih) signed off the human hot-path review (P15) for this decision; see the sign-off table in `docs/architecture/hot-paths.md`.
