# Spec: Crash resilience

- Status: Draft
- Owner crate(s): `rbs-crash` (boundary, reports, running marker); `rbs-app` (`src/entry.rs`, `clippy.toml`, `studio_host`)
- Related ADRs: [ADR-0054](../adr/0054-crash-resilience.md), [ADR-0024](../adr/0024-node-restart-with-back-off.md), [ADR-0041](../adr/0041-studio-recording-on-a-running-engine.md), [ADR-0051](../adr/0051-user-data-profiles-and-scene-collections.md)

## Purpose

How the desktop app keeps running when its own code panics, how it records what happened, and how it tells the user after a session that ended badly. The media graph's own containment (P7) is in [media-graph.md](media-graph.md) and [engine.md](engine.md).

## Scope

In scope: panic boundaries on the UI thread and the studio's control thread, crash reports, unclean-exit detection, the ban on nested event loops and blocking on the UI thread.

Out of scope (ADR-0054 follow-up work): the studio in its own process, heartbeats, native crash dumps, GPU device loss, isolated adapters and plugins.

## Contract

### Panic boundary (`rbs-crash`)

```rust
/// Runs `f`; a panic is caught and returned, never unwound further.
pub fn contain<R>(f: impl FnOnce() -> R) -> Result<R, Caught>;

/// A caught panic.
#[non_exhaustive]
pub struct Caught { pub message: String }

/// The error bar's text for a caught panic.
pub const CAUGHT_TEXT: &str;
```

- `contain` returns `Ok` with `f`'s value when `f` returns, and `Err(Caught)` with the panic's message (`&str` or `String` payload, else `"unknown panic"`) when it panics.
- While `f` runs, the thread is marked as inside a boundary, so the hook writes `Kind: caught`.

### UI thread (`rbs-app`)

- `src/entry.rs` is the only module that calls `slint::Timer::single_shot`, `slint::Timer::start`, `slint::invoke_from_event_loop`, `slint::spawn_local` and `slint::Window::on_close_requested`. Its wrappers (`single_shot`, `start`, `invoke`, `spawn_local`, `on_close_requested`) run the body (for a task: each poll) under `contain`. A panic in a close handler keeps the window; a panic in a task ends the task.
- `Ctx::update` runs the change, and each change queued while the state was borrowed, under `contain`. Other callbacks of the generated globals that do not go through `Ctx::update` wrap their body in `entry::guard`.
- A caught panic shows `CAUGHT_TEXT` in the error bar, at most once every 5 s.

### Studio control thread (`rbs-app::studio_host`)

- Each round of the control loop (the message that woke it, one API request, the studio's service and output checks) runs under `contain`. A caught panic sends `Update::Error(CAUGHT_TEXT)`, then runs `Host::recover`.
- An API request runs under its own `contain`; a panic answers it with error kind `internal` and message `CAUGHT_TEXT`, and the round then runs `Host::recover`.
- `Host::recover`:
  1. a panic while the studio restarted (canvas change) sets the studio's failure (`dead`), which the next round handles as any studio failure (outputs end, `Update::Failed`);
  2. every output the studio runs that is neither the tracked recording nor the tracked stream is stopped;
  3. a tracked recording or stream that the studio no longer runs is ended with an error;
  4. after 3 rounds in a row that panicked, the studio's failure is set;
  5. snapshots are published.
- Starting the studio runs under `contain`; a panic there is reported as `Update::Failed(CAUGHT_TEXT)`.

### Crash reports (`rbs-crash`)

```rust
/// Installs the panic hook (once per process) and this process's running marker in
/// `dir`. Returns what the previous sessions left behind.
pub fn install(dir: &Path) -> std::io::Result<Session>;

/// Panics on the calling thread get a crash report from now on.
pub fn report_panics_on_this_thread();

pub struct Session { /* holds the marker's lock */ }
impl Session {
    /// Processes that ended without a clean exit since the last start.
    pub fn unclean(&self) -> &[Unclean];
    /// Removes the marker: call on a clean exit only.
    pub fn end_clean(self);
}

#[non_exhaustive]
pub struct Unclean { pub pid: u32, pub report: Option<PathBuf> }

/// `text` with the home directory as `~` and stream URLs redacted.
pub fn redact(text: &str, home: Option<&str>) -> String;

pub const KEEP_REPORTS: usize = 20;
pub const MAX_REPORTS_PER_PROCESS: u32 = 5;
```

- `dir` is `<config>/crash-reports/` and is created when missing. The hook is installed even when the marker fails.
- **Who gets a report:** only threads that called `report_panics_on_this_thread` (the app: the UI thread and the studio's control thread). The hook does nothing else on other threads, so real-time threads never capture a backtrace or write a file. The previous hook runs on every thread (the message on stderr stays).
- **Report file:** `panic-<pid>-<unix seconds>-<n>.txt`, written once per panic with `create_new`, UTF-8 text with these lines first: `RBS <version>`, `OS: <os> <arch>`, `Time: <RFC 3339 UTC>`, `Kind: caught|uncaught`, `Thread: <name or unnamed>`, `Message: <message>`, `Location: <file>:<line>:<column>`, then a blank line and the backtrace. Message and backtrace pass through `redact`.
- **Redaction:** every occurrence of the home directory (`HOME`, on Windows `USERPROFILE`) becomes `~`. After `rtmp://`, `rtmps://`, `srt://`, `rist://` or `whip://` (any case), everything up to whitespace or one of `"'>)` becomes `<redacted>`.
- **Limits:** at most `MAX_REPORTS_PER_PROCESS` per process; at `install`, reports beyond the newest `KEEP_REPORTS` (by time, then `n`) are deleted.
- **Hook robustness:** no locks of its own; a thread-local flag stops a panic while writing from writing again; the writer runs under `catch_unwind`; a failed write is ignored.
- **Running marker:** created as `starting-<pid>.tmp`, locked with `File::try_lock`, `started <unix seconds>` written, then renamed to `running-<pid>.lock`, and held for the life of the `Session`. `end_clean` closes and deletes it. Dropping the `Session` without `end_clean` releases the lock and keeps the file, as a crash would.
- **Unclean detection:** at `install`, every `running-*.lock` whose lock can be taken belongs to a process that ended without `end_clean`. Each is listed in `unclean()` with that pid's newest report whose time is not before the marker's start, then deleted. A marker still locked belongs to a running instance and is left alone.
- The app calls `end_clean` on every return from `main`, also with an error, and shows one notice per start when `unclean()` is not empty, with the newest report's path and a note that reports may contain file names. When `install` fails, the app goes on and says so on stderr.
- Nothing in this crate sends data anywhere.

### Nested event loops and blocking (`rbs-app/clippy.toml`)

`disallowed-methods` in the app crate bans: the Slint functions wrapped by `entry.rs` (allowed inside it only); `rfd::FileDialog::{pick_file, pick_files, pick_folder, pick_folders, save_file}` and `rfd::MessageDialog::show`; `slint::run_event_loop`; `slint::ComponentHandle::run` (allowed once, in `main`); `std::thread::sleep` (allowed at the studio thread's shutdown, with a comment). Examples that are measurement tools allow them at file level. UI code uses `rfd::AsyncFileDialog` (or `AsyncMessageDialog`) under `entry::spawn_local`.

### Build

No `Cargo.toml` may set `panic = "abort"`; CI checks it.

## Errors and failure behaviour

- `install` fails only when `dir` cannot be created or the marker cannot be created, locked or renamed (for example on a file system without locks). The app then runs without unclean-exit detection.
- A failed report write is ignored (the hook runs while the process may be dying).
- Not reported (the notice still appears): out-of-memory aborts, stack overflows, panics during unwinding, a panic inside the hook, native faults, panics on threads that did not opt in that end the process.

## Performance requirements

None on hot paths: `contain` is used on the UI and control threads only, and the hook does nothing on threads that did not opt in. Installed cost: one open file and one lock per process.

## Conformance tests

`rbs-crash` (unit and `tests/hook.rs`):

1. `contain` returns the value of a closure that returns, and `Caught` with the message for `&str`, `String` and other payloads; the inside-a-boundary mark is cleared after a panic.
2. A report has the fields above in order; RFC 3339 times are right across leap days.
3. A marker left by a dropped `Session` is reported as unclean by the next `install` with that pid's newest report written after it started, and is deleted.
4. A marker held by a live `Session` is not reported and not deleted.
5. `end_clean` leaves no marker and the next `install` reports nothing.
6. Retention keeps the newest `KEEP_REPORTS`.
7. `redact` hides the home directory and stream URLs.
8. With the hook installed, a panic on a thread that did not opt in writes no report; one on a thread that did writes one with `Kind: caught`.

`rbs-app` (`tests/studio_faults.rs`): with a capture adapter whose `enumerate` panics, `ListTargets` answers `Update::Error(CAUGHT_TEXT)`, the studio does not fail, and the next commands (list, add a source) work.

## Open questions

- Should the notice offer to open the reports folder? It needs a "reveal in Finder / Explorer" helper in the shell.
