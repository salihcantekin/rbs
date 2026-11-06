# ADR-0055: Crash resilience: a fault in the app must not end a recording or a stream

- Status: Proposed
- Date: 2026-11-06
- Deciders: Salih (owner)
- Builds on: P7 (failures contained to the node), P8 (headless core), ADR-0008 (UI as a control API client), ADR-0024 (node restart with back-off), ADR-0028 and ADR-0036 (preview across processes), ADR-0041 decision 7 (studio on its own control thread), ADR-0051 (crash-safe user data)

## Context

Not crashing is the main promise RBS makes over OBS. On 2026-11-06 the app aborted on macOS when the user chose Export (fixed in PR #143). The menu callback opened a blocking file dialog from a Slint timer. The dialog ran a nested AppKit event loop, Slint's timer code was entered again and panicked ("Recursion in timer code"), and the panic could not unwind through the AppKit frames between it and `main`. Rust then aborts. The recording and the stream on the studio's control thread in the same process ended with it.

P7 and ADR-0024 already contain failures inside the media graph: a node that errors, panics or stalls is bypassed and restarted, and `rbs-engine` catches panics in control jobs. Nothing covers the rest of the process. The crash classes, with what protects against each today:

| Class | Example | Today |
| --- | --- | --- |
| A. Re-entrancy and nested event loops | Modal dialog in a callback (the Export crash); a `RefCell` borrowed again by a callback the toolkit runs during another one | Convention only (`Ctx::update` queues while borrowed) |
| B. Panics in UI code | An index out of range in a callback | Unwinds into Slint or winit; on macOS through AppKit frames, which aborts |
| C. Panics on the studio's control thread | A bug in a scene edit command | Ends the thread; the studio and every output stop |
| D. Panics across FFI callbacks | A panic in a CoreAudio, AVFoundation or WASAPI callback | Some adapters catch; since Rust 1.81 an `extern "C"` function without a catch aborts |
| E. Native faults in vendor code | A segfault in a GPU driver, NVENC, AMF or a native plugin | Process dies |
| F. GPU device loss | Driver reset, eGPU unplugged, TDR on Windows | Encoders report `DeviceLost`; the compositor's device is not rebuilt |
| G. Hangs | A blocked UI thread or control thread | Nothing notices |

Forces:

- A crash must not cost the user their work: the recording file, the live stream and the scene edits.
- Real-time paths stay as they are (P6, P15). Defences live on control and UI threads and at process boundaries.
- Zero cost when unused (P13): defences cost nothing per frame.
- No data leaves the machine without the user's action. There is no telemetry.
- `rbs-app` is the composition root. The UI must not depend on the engine's internals (ADR-0008).

## Decision

1. **Unwinding stays on.** No profile may set `panic = "abort"` (CI checks the manifests), so a panic can be caught at a boundary.
2. **Every UI entry point is a panic boundary (class B).** Each closure the toolkit calls must run its body under `rbs_crash::contain`, which catches a panic, so it never unwinds into toolkit or OS frames. Only `rbs-app/src/entry.rs` may call the Slint functions that install closures (`Timer::single_shot`, `Timer::start`, `invoke_from_event_loop`, `spawn_local`, `Window::on_close_requested`, `Window::set_rendering_notifier`); its wrappers add the boundary (for a task: each poll and its drop), and the crate's `clippy.toml` bans the originals everywhere else. Callbacks of the generated `.slint` globals go through the `on!` macro and `Ctx::update`, whose change and queued changes run under the boundary, or start with `entry::guard`; a test (`tests/entry_points.rs`) checks every one. A caught panic is shown in the error bar, at most once every 5 s, and the app keeps running. A boundary only helps when the unwind path between the panic and it is Rust code: it cannot catch a panic raised below a nested OS loop, such as the Export crash. Decision 6 prevents that class.
3. **The studio's control thread is a panic boundary per round (class C).** Each round of its loop (the message that woke it, one API request, the studio's service) runs under `contain`; an API request also runs under its own `contain` and fails with an `internal` error. After a caught panic the host reconciles its bookkeeping with the studio:
   - the command that panicked gets the answer the UI waits for (a start gets "not started");
   - an output the studio runs but the host does not track (a start that panicked half-way) is stopped, so no file grows that Stop cannot end;
   - stopping a recording or stream keeps it tracked until the stop is done, so a tracked one that no longer runs (a stop that panicked half-way) is ended again with an error;
   - a panic during a canvas restart counts as the studio failing (the existing `dead` path);
   - after 3 caught panics in a row the studio counts as failed: the failure is recorded first, then every output is stopped, each step under its own boundary, so its outputs end cleanly instead of running on with a host that keeps panicking;
   - snapshots are published again.
4. **Crash reports, local only.** At start the app installs a panic hook, chained to the previous one, that writes a plain text report to `<config>/crash-reports/`: version, OS and architecture, time, whether the panic was caught, thread name, message, location and a backtrace.
   - Only threads that call `rbs_crash::report_panics_on_this_thread()` get reports: the UI thread and control threads. A real-time thread never captures a backtrace or writes a file in the hook (P6); its panics are already contained by the executor (ADR-0024).
   - The home directory is shown as `~` where it is a whole path (case-insensitive on Windows); everything after a stream URL's scheme (`rtmp`, `rtmps`, `srt`, `rist`, `whip`) is replaced; and the stream key and URL in use are registered with `rbs_crash::add_secret` when a stream starts and replaced wherever they appear, so a report holds no stream key.
   - At most 5 reports per process, of which caught panics may use 4, so the panic that ends the process still gets one; the newest 20 are kept.
   - The hook waits on no lock (the secret list is read with `try_lock`), uses thread-locals only through `try_with`, and has a re-entrancy flag; a failure while writing is ignored. A panic inside the hook itself aborts; the writer avoids every panicking operation.
   - No report is written for an out-of-memory abort, a stack overflow, a panic during unwinding, a panic in the hook or a native fault; decision 5 still detects those.
   - Reports never leave the machine on their own.
5. **Unclean exits are detected without a signal handler.** For its lifetime each app process holds an OS file lock (`File::try_lock`) on `<config>/crash-reports/running-<pid>.lock`, created under a temporary name and renamed once locked, so a second instance starting at the same moment never sees it unlocked. Every return from `main`, also with an error, writes `clean` into it while it is still locked and then deletes it, so an instance starting in between never reports a clean exit as a crash. Leftover `starting-*.tmp` files of dead processes are deleted, and a report the notice points to is never pruned. At start, a marker whose lock can be taken belongs to a process that ended without a clean exit, whatever the cause. The app then shows once: "RBS closed unexpectedly last time. Your scenes were kept as last saved, and a Matroska recording stays playable up to that moment.", with that process's newest report written after it started (pids are reused), if any. Scenes need no extra recovery: ADR-0051 saves them atomically shortly after each change. When the lock cannot be made (some network file systems), detection is off and the app says so on stderr only.
6. **No nested event loops and no blocking on the UI thread (class A).** UI code must not call anything that runs a nested event loop or blocks: blocking file dialogs and message boxes, `slint::run_event_loop`, `ComponentHandle::run` (except the one in `main`), or `std::thread::sleep`. It must use the asynchronous form (`rfd::AsyncFileDialog` under `entry::spawn_local`, as PR #143 does) or a sheet or window of our own. `rbs-app/clippy.toml` bans these (`disallowed-methods`), so CI fails on a new one. Blocking waits that are legitimate on the app's other threads (`recv`, `join`) are left to review.
7. **FFI callbacks catch panics (class D).** Every `extern "C"` function or block an OS or vendor library calls must wrap its body in `catch_unwind` and return a neutral value (silence, no frame, an error status), as the AudioToolbox, CoreAudio and VideoToolbox adapters already do. This is now on the `risky-review` checklist; an audit of the existing adapters is a follow-up.
8. **Tests.** `rbs-crash` has unit tests for every rule above; a fault-injecting capture adapter shows that the studio host survives a panicking command and goes on.

The crash classes left (E, F, G) are follow-up work, each to be decided in its own ADR. They are listed below and are not part of this decision.

## Consequences

- A bug in UI code or in a studio command becomes an error message instead of a lost recording, as long as its unwind path is Rust code. The Export crash class fails CI through decision 6.
- Users and maintainers get a local report for every panic on the UI and control threads, and the app says when the last session ended badly.
- A caught panic may leave `App` state inconsistent. Views are rebuilt from state on every sync, the studio's snapshots are republished, and repeated panics on the studio side end its outputs cleanly; on the UI side the error bar shows the problem, rate-limited. Rebuilding `App` after repeated UI panics is a follow-up if this proves insufficient.
- A clean OS shutdown or logout that kills the app without returning from `main` can show the notice once on the next start. That is accepted.
- New `rbs-crash` crate: standard library only, so `rbs-cli` and `rbs-mcp` can use it too (P8). It needs Rust 1.89 (`File::try_lock`), like `rbs-remote-settings`.
- Cost: one `catch_unwind` per UI callback and per control round (nothing per frame, nothing on the render or audio threads), one open file and one lock per process. Nothing runs while nothing fails.
- Native faults (E), device loss (F) and hangs (G) still end or freeze the app. Moving the studio into its own process is the largest remaining gap and the next work item.
- Spec: [crash-resilience.md](../specs/crash-resilience.md).

### Follow-up work (not decided here)

- **The studio in its own process** (ADR-0008 and ADR-0022's two-process mode): the studio process owns the engine, outputs, user data writer and control API and links no toolkit; the UI talks to it over the control API on local IPC (ADR-0044) and shows the preview through the existing cross-process surfaces (ADR-0028, ADR-0036). A dead or hung UI is restarted and attaches again while outputs keep running. Needs its own ADR: protocol, which process starts which, macOS app bundle and Dock behaviour, heartbeats, and measured cost.
- **Recording writer in a helper process:** a smaller step that protects recordings sooner, to compare with the full split in that ADR.
- **Native crash dumps:** `crash-handler` with an out-of-process `minidumper`, once a second process exists.
- **GPU device loss:** rebuild the `GpuContext` and apply the scene again; outputs not tied to the device keep running.
- **Isolated adapters and plugins:** vendor encoders and native-tier plugins in a helper process (the plugin SDK's `process` tier), so their crash is a node failure under P7.
- **Tests:** a smoke test that runs every command of the ADR-0050 registry with a fake dialog backend; property tests driving `rbs-ui` and the studio host with random command sequences; fuzzing the import and settings readers; `slow_soak_*` start, stop, record and stream runs; a chaos test that kills the UI during a test stream once the studio has its own process.
- **FFI audit** of the adapters for decision 7.

## Alternatives considered

- **`panic = "abort"` with a supervisor process only.** Smaller binaries, but every bug in a callback would end the app, and until the studio has its own process that ends the recording too.
- **A signal handler that writes a report for native faults.** Writing files in a signal handler is not async-signal-safe. `crash-handler` with an out-of-process `minidumper` does it right but needs the second process; it is a follow-up. The file lock already detects every unclean exit.
- **Restart the whole app automatically after a crash.** It would not save a recording or a stream in one process.
- **Run the studio in its own process right away.** It is the target for classes E and G, but it needs its own protocol ADR and platform work. The in-process boundaries fix the classes seen and are needed in both topologies.
- **Reports from every thread.** Simpler, but it puts backtrace capture and file I/O on real-time threads when a node panics (P6).

## Review

Independent review per ADR-0014.

Reviewer: independent AI session
Date: 2026-11-06
Verdict: changes requested (first draft); resolutions below
Concerns:
1. Decision 2 would not have stopped the Export crash (panic below AppKit frames) -> Decision 2 now says so; the Export crash class is prevented by decision 6, and Consequences claim CI coverage through decision 6 only.
2. The guard cannot be checked by review alone -> Every Slint function that installs a closure is wrapped in `entry.rs` and banned elsewhere by `rbs-app/clippy.toml`; generated callbacks go through `Ctx::update`, which is guarded.
3. A caught panic can corrupt state and nothing escalates; repeated panics flood reports -> The error bar is rate-limited (5 s), reports are capped at 5 per process, queued changes each run under their own boundary; repeated studio panics end the outputs cleanly (decision 3). Rebuilding `App` after repeated UI panics is a follow-up.
4. "The next snapshot shows the truth" is false for the studio host; only messages were guarded -> Each whole round is guarded; `Host::recover` stops untracked outputs, ends tracked ones that no longer run, treats a panic during a canvas restart as `dead`, escalates after 3 in a row, and republishes. Fault-injection at each step of record, stream and canvas change is a follow-up test.
5. The hook breaks P6 on real-time threads -> Only threads that opt in get reports; real-time threads never do.
6. Hook robustness gaps -> Re-entrancy flag, atomic serial, no locks of its own, directory created at install, caught/uncaught kind in the report, and the classes with no report are listed (decision 4).
7. Unclean-exit detection bugs -> (a) `rbs-crash` declares Rust 1.89. (b) Marker locked under a temporary name, then renamed. (c) A report must be newer than the marker's start time. (d) Every return from `main` ends cleanly; OS-terminated sessions may show the notice once (accepted in Consequences). (e) Lock failure turns detection off quietly. Windows behaviour is covered by the `rbs-crash` tests, which CI runs on Windows and macOS too.
8. The lint list is short and its scope unclear -> The list lives in `rbs-app/clippy.toml` (app crate only) and includes `run_event_loop`, `ComponentHandle::run`, `std::thread::sleep` and the blocking dialogs; `recv`/`join` on non-UI threads are left to review.
9. Privacy -> Home directory shown as `~`, stream URLs redacted; the notice says reports may contain file names.
10. Decisions 8 and 9 were not decisions -> Moved to a non-normative "Follow-up work" section.
11. Missing alternatives -> Recording writer in a helper process and `crash-handler` with `minidumper` added; the code is a toolkit-free crate (`rbs-crash`).
12. Decision 1 cannot be checked; FFI rule -> CI checks the manifests for `panic = "abort"`; the FFI rule is on the `risky-review` checklist and the audit is a follow-up.

Second pass, same reviewer, 2026-11-06. Verdict: changes requested; resolutions:
1. A clean exit could be seen as a crash between closing and deleting the marker -> `end_clean` writes `clean` while locked and deletes before closing; such markers are deleted without a notice; stale `starting-*.tmp` files are removed; the report shown is never pruned. Tests added.
2. Hook claims partly false -> The `catch_unwind` inside the hook is gone (a panic there aborts anyway, stated); thread-locals use `try_with`; one report slot is kept for an uncaught panic.
3. `Host::recover` gaps -> The failure is recorded before outputs end (also in the existing failure paths of the loop); at the limit every output is stopped, each under its own boundary; stops keep the output tracked until done, so a half-done stop is finished by `recover`; the panicking command gets the answer the UI waits for. Fault-injection tests (feature `fault-injection`, tests only) cover a panic after an output started and a panic in the middle of a recording stop.
4. `Guarded` dropped the inner future outside the boundary -> The task is dropped inside the boundary after a panic or completion, and `Drop for Guarded` is guarded.
5. Lint claims went further than the checks -> `tests/entry_points.rs` checks every generated callback; the risky-review text says which check covers what; the CI check also looks for `panic=abort` in `.cargo/` rustflags.
6. `redact` home prefix and unknown secret forms -> Home replaced only as a whole path, case-insensitive on Windows; the stream key and URL are registered as secrets at stream start.
7. Windows rename -> Commented (std's `FILE_SHARE_DELETE`); the `rbs-crash` tests run in CI on Windows and macOS.
