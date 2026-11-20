# Spec: Diagnostics log

- Status: Draft
- Owner crate(s): `rbs-core` (`log`), `rbs-log`; wired by `rbs-app`, `rbs-cli`, `rbs-browser-runner`
- Related ADRs: [ADR-0158](../adr/0158-diagnostics-log.md), [ADR-0055](../adr/0055-crash-resilience.md) (redaction, crash reports), [ADR-0013](../adr/0013-zero-cost-when-unused.md)

## Purpose

Leveled lines that say what RBS did, kept in a rotating file a user can share with a bug report. Hot paths never log.

## Scope

In scope: the facade in `rbs-core`, the file writer in `rbs-log`, levels, files, redaction, what each area logs, how a user changes the level and exports the logs.

Out of scope: metrics export (OpenTelemetry), structured fields, `tracing` records (`zbus` on Linux), the control API commands (a later contract change).

## Contract

### Facade (`rbs_core::log`)

```rust
#[repr(u8)]
pub enum Level { Error = 1, Warn = 2, Info = 3, Debug = 4, Trace = 5 }

/// The most detailed level an area writes; `Off` writes nothing.
#[repr(u8)]
pub enum Filter { Off = 0, Error = 1, Warn = 2, Info = 3, Debug = 4, Trace = 5 }

#[repr(u8)]
#[non_exhaustive]
pub enum Area { App, Ui, Studio, Engine, Gpu, Capture, Audio, Encode, Record, Stream,
                Browser, Remote, AddOns, Plugin, UserData, Lib }

pub struct Record<'a> {
    pub level: Level,
    pub area: Area,
    pub args: fmt::Arguments<'a>,
    pub file: &'static str,
    pub line: u32,
}

pub trait Sink: Send + Sync {
    fn write(&self, record: &Record<'_>);
}

/// Per-area filters, all `Off` at first; tests make their own value.
pub struct Filters { /* one AtomicU8 per area */ }
impl Filters {
    pub const fn new() -> Filters;
    pub fn set(&self, area: Area, filter: Filter);
    pub fn get(&self, area: Area) -> Filter;
    pub fn enabled(&self, area: Area, level: Level) -> bool;  // one relaxed load
}

// The process-wide filters and sink:
pub fn install(sink: &'static dyn Sink) -> Result<(), AlreadyInstalled>;
pub fn set_all(filter: Filter);
pub fn set_filter(area: Area, filter: Filter);
pub fn filter(area: Area) -> Filter;
pub fn enabled(area: Area, level: Level) -> bool;   // one relaxed load

pub fn write(record: &Record<'_>);                   // to the sink, if installed

// Called by path, never imported: rbs_core::info!(Area::Record, "Recording started: {}", name);
// Macros: error!, warn!, info!, debug!, trace!, and log_at!(level, area, ...).
```

Invariants (each a test):

- Before `install`, every area's filter is `Off`, `enabled` is false and a macro call evaluates none of its format arguments.
- `install` succeeds once; a second call returns `AlreadyInstalled` and leaves the first sink.
- `set_filter` before `install` is kept: `install` does not reset filters. Composition roots set filters after installing.
- A call whose filter lets it through and finds no sink does nothing.
- `Filters` values are independent of the process-wide filters, so their tests run in parallel; the process-wide sink and filters are tested in one integration test file with one test.
- A macro call at a level above its area's filter evaluates no argument and calls no sink.
- A macro call at or below the filter calls the sink exactly once with its level, area, file and line.
- `Area::name()` gives the lower-case name used in files and in `RBS_LOG` (`app`, `ui`, `studio`, `engine`, `gpu`, `capture`, `audio`, `encode`, `record`, `stream`, `browser`, `remote`, `addons`, `plugin`, `userdata`, `lib`); `Area::ALL` lists every area once.

### Writer (`rbs-log`)

```rust
pub struct Config {
    pub dir: PathBuf,             // logs folder
    pub prefix: String,           // "rbs" or "rbs-browser"
    pub pid: u32,
    pub header: Vec<String>,      // first lines of the file
    pub limits: Limits,           // file 10 MB, runs 10, total 50 MB
    pub queue: usize,             // 1024 lines
    pub home: Option<String>,     // shown as `~`; rbs_crash::home_dir() by default
    pub now: fn() -> SystemTime,  // injected for tests
    pub echo: bool,               // also to standard error, from the writer thread
}
pub fn start(config: Config) -> io::Result<Logger>;   // prunes, opens, spawns `rbs-log`
pub fn start_stderr(queue: usize, home: Option<String>) -> io::Result<Logger>; // no file: standard error only (CLI)
impl Logger {
    pub fn sink(&self) -> &'static dyn rbs_core::log::Sink;  // to pass to install
    pub fn flush(&self, wait: Duration) -> bool;              // true when written in time
    pub fn recent(&self) -> Vec<String>;                      // last 500 lines, never waits
    pub fn path(&self) -> Option<&Path>;                      // the first file; None for start_stderr
}
pub fn default_dir() -> Option<PathBuf>;                     // per OS, RBS_LOG_DIR first
pub fn parse_filters(text: &str) -> Result<Vec<(Option<Area>, Filter)>, ParseError>;
pub fn bridge_log_crate();                                    // `log` records into Area::Lib
pub fn set_filter(area: Area, filter: Filter);               // also keeps log::max_level = Lib, capped at Debug
pub fn apply_level(filter: Filter);                          // every area but Lib
```

- The sink formats the message on the calling thread and sends it with its level, area, file, line, time and thread name on an unbounded channel whose length a counter caps at `queue` (the send never waits). A full queue drops the line and adds one to a dropped count; the writer writes `(<n> lines dropped)` before the next line it writes. Line: `<UTC time> <LEVEL> <area> [<thread name>] <message>`.
- The writer thread is named `rbs-log` (listed in [threads.md](../architecture/threads.md)). It blocks on the queue with no timeout when idle (with a deadline 60 s after a repeat count began, only while one is pending) and flushes the file whenever the queue is empty.
- Lines pass through `rbs_crash::LogRedactor::redact` (one redactor per writer; `rbs_crash::redact_for_log(text, home)` for a single line) before they are written. The redactor keeps its own copy of the secrets list and takes the list's lock only to copy it again after `add_secret` changed it (a change counter), never while it scans a line, so the panic hook's non-waiting `try_lock` is not turned away by the writer: home directory to `~`; registered secrets (`rbs_crash::add_secret`, up to 64 distinct values; a secret added again moves to the end, the least recently added is dropped first; callers register a secret before any line holding it is logged) to `<redacted>`; the path and query of `rtmp://`, `rtmps://`, `srt://`, `rist://`, `whip://` URLs to `<redacted>` with scheme and host kept; the query and fragment of `http://` and `https://` URLs removed (`?<redacted>`).
- Repeats: a message equal to the previous one from the same file and line (time not compared) is counted, not written. The count is kept across flushes and written as `(repeated once)` or `(repeated <n> times)` when a different line comes, 60 s after the first repeat at the latest (a deadline, not an idle time), or at `Logger::flush`; the file flush when the queue empties leaves it pending.
- File name `<prefix>-<YYYYMMDDTHHMMSSZ>-<pid>.log`, created with `create_new`. While written it is marked as in use: on Unix an exclusive `flock` (`File::try_lock`); on Windows it is opened with share mode `FILE_SHARE_READ` only (no delete sharing, no Windows file lock, so other programs can still read it). When the file passes `limits.file` bytes the writer continues in the segment `<prefix>-<time>-<pid>.<n>.log` (n = 1, 2, ...).
- Pruning, at start and whenever a segment is added: only files whose names match `<prefix>-<8 digits>T<6 digits>Z-<digits>[.<digits>].log` exactly count (so `rbs` never matches `rbs-browser-...`). Runs are ordered by the time in the name, the pid breaking ties. A file in use is skipped: on Unix one whose lock is held, on Windows one that cannot be removed (another instance or helper still writes it). The oldest runs are removed until at most `limits.runs` remain, counting the current run, and their total size is at most `limits.total`; when only the current run is left over the total, its oldest segments are removed, never its first file (it holds the header).
- The memory ring keeps the last 500 redacted lines; the writer holds its lock only to push one line. `recent()` copies it with a `try_lock` and returns an empty list rather than wait (it is called from the panic hook); a crash report then says the recent lines were not available. Crash reports run `rbs_crash::redact` over the copied lines, which also removes stream hosts.
- `flush(wait)` sends a flush request and waits at most `wait` for its answer: the lines are then written to the file (handed to the OS; no `fsync`). It must not be called on the UI thread.
- `rbs_log::set_filter` sets the process-wide filter and, for `Area::Lib`, `log::set_max_level` to the same level (Trace becomes Debug). `rbs_log::apply_level(filter)` is the Settings level: every area but `Lib`. Apps set levels only through these two; `rbs_core::log::set_filter` and `set_all` are low level (tests, and `set_all` also raises `Lib`).
- The bridge maps `log` records to `Area::Lib` with their target in front of the message. Before formatting it counts lines per call site: a site (file, line and level; 256 slots; records without a static file share one) writes its first 10 lines, then one line in 1000, and errors are never thinned, so a library that warns on every frame costs a load, a branch and an atomic add per frame on that thread, never a format.

Folders: macOS `~/Library/Logs/RBS/`, Windows `%LOCALAPPDATA%\RBS\logs\`, Linux `$XDG_STATE_HOME/rbs/logs/` (default `~/.local/state/rbs/logs/`). `RBS_LOG_DIR` wins.

`RBS_LOG` holds comma-separated items: a bare level sets every area, `<area>=<level>` one area; later items win. Levels: `off`, `error`, `warn`, `info`, `debug`, `trace`.

## What is logged

Default filter: `Info` for every area except `Lib` (`Warn`).

| Area | Error | Warn | Info | Debug / Trace |
| --- | --- | --- | --- | --- |
| `app` | settings or profile could not be saved | unreadable setting fell back to its default | start (header), quit and its step times, profile and collection loaded | which settings file was written, by name (Trace) |
| `ui` | not answering for 4 s or longer | not answering for 1 s or 2 s; an entry that returned after 1 s or more | windows opened and closed | an entry that returned after more than 100 ms |
| `studio` | the studio could not start or restart | a command refused | scene switch, transition, studio restart | each command by kind, never its payload (Trace) |
| `engine` | a partition stopped after its restarts | a node restarted | graph built (node and partition counts) | rebuild details, stats summary every 10 s (Trace: 1 s) |
| `gpu` | device lost | software adapter in use | adapter and backend chosen | surface and format details |
| `capture`, `audio` | a device failed to open or vanished | a format fallback | device opened or closed, format chosen | formats offered, negotiation |
| `encode`, `record`, `stream` | a recording or stream failed to start or stopped on an error | encoder fallback, reconnect attempt, frames dropped in the last 5 s | start and stop with encoder, size, rate and bitrate; reconnected | candidates tried, keyframe and pause events |
| `browser` | the helper crashed | GPU path fell back to shared memory | helper started and exited (exit code) | page host and path (Debug), IPC message kinds (Trace) |
| `remote` | the server could not start | a refused connection or token | client connected and left (address with its last part hidden) | each request (Trace) |
| `addons`, `plugin`, `userdata` | install, load or import failed | a newer file kept read only | installed, imported, exported | file details |
| `lib` | library errors | library warnings | - | library info and debug |

Never logged: text source contents, command and setting payloads, stream keys and passphrases, tokens, URL queries, user names in home paths. Window titles of captured windows and page paths only at Debug and above. Known limit: a user name in another path (`/Volumes/<name>`) is not detected; Save logs asks the user to look the files over.

`Lib` follows the `RBS_LOG` and per-area (Pro) settings only; the five Settings levels leave it at Warn, because a library may log per frame at Debug.

## Settings › Advanced (planned)

Shown at every experience level, last in the App group. Planned cards (visual change; screenshots and owner approval first, CLAUDE.md rule 16):

- Logging: "Log detail" (Errors only, Warnings, Normal, Detailed, Everything); a Toggle "Back to Normal at next start" (on); a status line when a detailed level is on or `RBS_LOG` decides. Pro: a level per area and "Reset all".
- Bug report: "Save logs…" (zip: the last three runs, crash reports, a system summary, a settings summary without keys or paths; non-blocking save dialog, ADR-0055), "Open the logs folder", "Report a problem on GitHub" (issue template `.github/ISSUE_TEMPLATE/bug_report.yml`, version and OS in the URL), and a note to look over the files before sending them.

## Errors and failure behaviour

- The folder or file cannot be created: `start` returns the error; the composition root logs to standard error and runs without a file.
- A write fails (disk full): the writer stops writing the file, keeps the memory ring, and never panics or stops the app.
- The writer thread is gone: the sink's `try_send` fails and the line is counted as dropped.

## Performance requirements

- A call whose level is off: one relaxed atomic load and a branch; no argument is evaluated.
- A call whose level is on: formatting and one non-blocking queue send on the calling thread, a few microseconds; never on a hot path.
- The writer thread is parked when there is nothing to write.

## Hot-path rule

No log macro in code listed in [hot-paths.md](../architecture/hot-paths.md). `cargo xtask check-layers` rejects the macros in these files and folders, which are hot as a whole: `rbs-audio` `src/{input,resample,mixer,meter,sync_probe,sink,output,encoder}.rs` and `src/loudness/`, `rbs-audio-filters/src/`, `rbs-denoise/src/`, `rbs-audio-io` `src/{ring,edge}.rs`, `rbs-graph/src/runtime/`, `rbs-filters/src/node.rs`, `rbs-engine/src/worker.rs`, `rbs-sources/src/browser_sound.rs`, `rbs-browser-shm/src/sound.rs`, `rbs-nodes` `src/media/{picture,sound,control}.rs` and `src/{preview,text,sync_probe}.rs`, `rbs-gpu` `src/{yuv,download,upload,budget,rgba,motion}.rs` and `src/share/`, `rbs-vcam` `src/{handoff,mailbox}.rs`, `rbs-media-format/src/`, `rbs-core` `src/{time,timing}.rs` and `src/media/`. Files with hot and cold code (adapters, muxers, transports) are checked by the `hot-path-review` checklist. The same check rejects `use rbs_core::{info, ...}`: the macros are called by path. The list lives in `xtask/src/check_logs.rs`.

## Conformance tests

- `rbs-core` `log`: filters off before install; arguments not evaluated when off; one sink call when on; install once; `Area::ALL` and names round-trip.
- `rbs-log`: line format with a fixed time; redaction (home, secret, URL host kept, http query removed, a redactor sees secrets added after it was made; a secret registered as a whole URL is replaced whole); lines dropped through the sink when its queue is full; repeat collapsing across flushes; dropped-line count with a full queue; rotation at the size limit; pruning by run count and total size, ignoring other prefixes and locked files, name clash on the same second; `recent` keeps the last 500; `parse_filters` accepts and rejects; `flush` returns after the lines are on disk. Tests use temporary folders and the injected time, and wait for `flush`, never for a duration (ADR-0032).

## Status

- Facade (`crates/rbs-core/src/log.rs`), writer (`crates/rbs-log`), `rbs_crash::LogRedactor` and the xtask check: implemented with tests (2026-11-20).
- `rbs-app`: the log starts first in `main` (`diag_log`), Normal levels, `Lib` at Warn, `RBS_LOG`; lines echo to standard error at a terminal or with `RBS_LOG`; every former `errln!` and `studio_ui::log` line and the UI stall watch log through the macros (2026-11-20).
- `rbs-cli`: `start_stderr` before the arguments are parsed (`diag_log`); Warn for every area and `Lib`, then `RBS_LOG`, then `--log-level <levels>` (same grammar, anywhere on the line); its answers to its user stay `outln!`/`errln!` (2026-11-20).
- Not yet: `rbs-browser-runner` (after stability WP-A), the recent lines in crash reports, new per-area lines, the summary timer, Settings › Advanced.

## Open questions

- None.
