# Spec: Crash resilience

- Status: Draft
- Owner crate(s): `rbs-crash` (boundary, reports, running marker); `rbs-app` (`src/entry.rs`, `clippy.toml`, `studio_host`)
- Related ADRs: [ADR-0055](../adr/0055-crash-resilience.md), [ADR-0024](../adr/0024-node-restart-with-back-off.md), [ADR-0041](../adr/0041-studio-recording-on-a-running-engine.md), [ADR-0051](../adr/0051-user-data-profiles-and-scene-collections.md)

## Purpose

How the desktop app keeps running when its own code panics, how it records what happened, and how it tells the user after a session that ended badly. The media graph's own containment (P7) is in [media-graph.md](media-graph.md) and [engine.md](engine.md).

## Scope

In scope: panic boundaries on the UI thread and the studio's control thread, crash reports, unclean-exit detection, the ban on nested event loops and blocking on the UI thread.

Out of scope (ADR-0055 follow-up work): the studio in its own process, heartbeats, native crash dumps, GPU device loss, isolated adapters and plugins.

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

- `src/entry.rs` is the only module that calls `slint::Timer::single_shot`, `slint::Timer::start`, `slint::invoke_from_event_loop`, `slint::spawn_local`, `slint::Window::on_close_requested` and `slint::Window::set_rendering_notifier`. Its wrappers (`single_shot`, `start`, `invoke`, `spawn_local`, `on_close_requested`, `set_rendering_notifier`) run the body (for a task: each poll, and its drop) under `contain`. A panic in a close handler keeps the window; a panic in a task ends the task, which is then dropped inside the boundary.
- `Ctx::update` runs the change, and each change queued while the state was borrowed, under `contain`. Callbacks of the generated globals are installed through the `on!` macro (its body runs in `Ctx::update`) or start with `entry::guard`; `tests/entry_points.rs` checks every `.on_*(` in `src/`.
- A caught panic shows `CAUGHT_TEXT` in the error bar, at most once every 5 s. Crash texts (`CAUGHT_TEXT`, also from the studio's thread, and the unclean-exit notice) use the error bar's notice kind (`Shell.error-kind` 3), which shows the text as it is, without the "That did not work:" prefix of failed actions.

### Studio control thread (`rbs-app::studio_host`)

- Each round of the control loop (the message that woke it, one API request, the studio's service and output checks) runs under `contain`. A caught panic sends `Update::Error(CAUGHT_TEXT)`, then runs `Host::recover`.
- An API request runs under its own `contain`; a panic answers it with error kind `internal` and message `CAUGHT_TEXT`, and the round then runs `Host::recover`.
- While a UI command runs, the host keeps the answer the UI waits for if it panics: `StartRecording` → `Update::Recording(NotStarted(CAUGHT_TEXT))`, `StartStream` → `Update::Stream(NotStarted(CAUGHT_TEXT))`.
- Stopping a recording or a stream keeps it tracked until the stop is done.
- When the studio fails (its own failure or `dead`), the loop records the failure first and then ends the outputs.
- `Host::recover`, each step under its own `contain`:
  1. sends the kept answer, if any;
  2. a panic while the studio restarted (canvas change) sets the studio's failure (`dead`), which the next round handles as any studio failure (outputs end, `Update::Failed`);
  3. every output the studio runs that is neither the tracked recording nor the tracked stream is stopped;
  4. a tracked recording or stream that the studio no longer runs is ended again, with an error;
  5. after 3 rounds in a row that panicked: the failure is recorded, the recording and stream are ended, every remaining output is stopped, and `Update::Failed` is sent;
  6. snapshots are published.
- With the `fault-injection` feature (tests only), `HostConfig::fault` makes the host panic once at a chosen step (`Fault::AfterOutputStart`, `Fault::InRecordingStop`), and `recover` sends `Update::Recovered { stopped }`.
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

/// Text that must never appear in a report (a stream key, a stream URL in use).
pub fn add_secret(secret: &str);
pub const MAX_SECRETS: usize = 16;

pub const KEEP_REPORTS: usize = 20;
pub const MAX_REPORTS_PER_PROCESS: u32 = 5;
```

- `dir` is `<config>/crash-reports/` and is created when missing. The hook is installed even when the marker fails.
- **Who gets a report:** only threads that called `report_panics_on_this_thread` (the app: the UI thread and the studio's control thread). The hook does nothing else on other threads, so real-time threads never capture a backtrace or write a file. The previous hook runs on every thread (the message on stderr stays).
- **Report file:** `panic-<pid>-<unix seconds>-<n>.txt`, written once per panic with `create_new`, UTF-8 text with these lines first: `RBS <version>`, `OS: <os> <arch>`, `Time: <RFC 3339 UTC>`, `Kind: caught|uncaught`, `Thread: <name or unnamed>`, `Message: <message>`, `Location: <file>:<line>:<column>`, then a blank line and the backtrace. Message and backtrace pass through `redact`.
- **Redaction:** the home directory (`HOME`, on Windows `USERPROFILE`, without a trailing separator) becomes `~` where it is followed by a separator, the end or one of `"' :)` and a newline (on Windows ignoring ASCII case), so `/Users/sc` does not touch `/Users/scott`. Every secret from `add_secret` (4 bytes or longer; the newest 16) becomes `<redacted>`; the studio host adds the stream key and URL when a stream starts. After `rtmp://`, `rtmps://`, `srt://`, `rist://` or `whip://` (any case), everything up to whitespace or one of `"'>)` becomes `<redacted>`.
- **Limits:** at most `MAX_REPORTS_PER_PROCESS` per process, of which caught panics use at most one less; at `install`, reports beyond the newest `KEEP_REPORTS` (by time, then `n`) are deleted, except one that `unclean()` points to.
- **Hook robustness:** it never waits on a lock (secrets are read with `try_lock`; when busy they are not applied); thread-locals are read with `try_with`; a thread-local flag stops a nested panic from writing again; the writer uses no panicking operation (a panic inside a hook aborts); a failed write is ignored.
- **Running marker:** created as `starting-<pid>.tmp`, locked with `File::try_lock`, `started <unix seconds>` written, then renamed to `running-<pid>.lock`, and held for the life of the `Session`. `end_clean` writes `clean\n` into it while still locked, deletes it, then closes it. Renaming and deleting the open file works on Windows because std opens files with `FILE_SHARE_DELETE`. Dropping the `Session` without `end_clean` releases the lock and keeps the file, as a crash would.
- **Unclean detection:** at `install`, every `running-*.lock` whose lock can be taken belongs to a process that ended without `end_clean`. A marker that holds `clean\n` is deleted without a notice. Each other one is listed in `unclean()` with that pid's newest report whose time is not before the marker's start, then deleted. A marker still locked belongs to a running instance and is left alone. A `starting-*.tmp` whose lock can be taken is deleted.
- The app calls `end_clean` on every return from `main`, also with an error, and shows one notice per start when `unclean()` is not empty, with the newest report's path and a note that reports may contain file names. When `install` fails, the app goes on and says so on stderr.
- Nothing in this crate sends data anywhere.

### Nested event loops and blocking (`rbs-app/clippy.toml`)

`disallowed-methods` in the app crate bans: the Slint functions wrapped by `entry.rs` (allowed inside it only); `rfd::FileDialog::{pick_file, pick_files, pick_folder, pick_folders, save_file}` and `rfd::MessageDialog::show`; `slint::run_event_loop`; `slint::ComponentHandle::run` (allowed once, in `main`); `std::thread::sleep` (allowed at the studio thread's shutdown, with a comment). Examples that are measurement tools allow them at file level. UI code uses `rfd::AsyncFileDialog` (or `AsyncMessageDialog`) under `entry::spawn_local`.

### Build

No `Cargo.toml` may set `panic = "abort"`, and no `.cargo/` config may pass `panic=abort`; CI checks both.

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
7. `redact` hides the home directory (only as a whole path, ignoring case on Windows) and stream URLs; registered secrets are hidden.
9. A marker that says `clean` and a leftover `starting-*.tmp` are deleted without a notice; the report a notice points to survives pruning.
8. With the hook installed, a panic on a thread that did not opt in writes no report; one on a thread that did writes one with `Kind: caught`.

`rbs-app`:

- `tests/studio_faults.rs`: with a capture adapter whose `enumerate` panics, `ListTargets` answers `Update::Error(CAUGHT_TEXT)`, the studio does not fail, and the next commands (list, add a source) work.
- `tests/studio_record.rs`: a panic after a recording's output started answers `NotStarted(CAUGHT_TEXT)`, stops that one untracked output, and the next recording starts and stops normally; a panic in the middle of a recording stop still ends it with an error, the file is complete, and a new recording starts.
- `tests/entry_points.rs`: every generated callback is guarded.

## Open questions

- Should the notice offer to open the reports folder? It needs a "reveal in Finder / Explorer" helper in the shell.
