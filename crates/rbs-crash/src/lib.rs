//! Crash resilience for the RBS composition roots (ADR-0054, spec `crash-resilience.md`).
//!
//! - [`contain`]: the panic boundary every UI entry point and every studio command runs
//!   under, so a panic never unwinds into toolkit or OS frames.
//! - [`install`]: the panic hook that writes a local crash report, and this process's
//!   running marker, which tells the next start that a session ended without a clean
//!   exit, whatever ended it.
//! - [`report_panics_on_this_thread`]: only threads that opt in get reports, so a panic
//!   on a real-time thread never does file I/O there (P6).
//!
//! Nothing here sends data anywhere.

use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::panic::{AssertUnwindSafe, PanicHookInfo, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The error bar's text for a caught panic.
pub const CAUGHT_TEXT: &str = "Something went wrong in RBS. The recording and the stream keep running; a report was saved in the crash-reports folder of the settings folder.";

/// Reports kept; older ones are deleted at start.
pub const KEEP_REPORTS: usize = 20;

/// Reports one process writes at most, so a panic that repeats cannot push the first,
/// most useful report out of [`KEEP_REPORTS`].
pub const MAX_REPORTS_PER_PROCESS: u32 = 5;

/// A caught panic.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Caught {
    /// The panic's message (`"unknown panic"` when the payload was not text).
    pub message: String,
}

/// Runs `f`; a panic is caught and returned, never unwound further (ADR-0054 decisions
/// 2 and 3). The panic hook has written its report by the time this returns.
pub fn contain<R>(f: impl FnOnce() -> R) -> Result<R, Caught> {
    let _ = CONTAINED.try_with(|d| d.set(d.get() + 1));
    let result = catch_unwind(AssertUnwindSafe(f));
    let _ = CONTAINED.try_with(|d| d.set(d.get().saturating_sub(1)));
    result.map_err(|payload| Caught {
        message: payload_text(payload.as_ref()),
    })
}

/// Panics on the calling thread get a crash report from now on. Call it on the UI
/// thread and on control threads, never on a real-time thread: the report captures a
/// backtrace and writes a file.
pub fn report_panics_on_this_thread() {
    let _ = REPORTING.try_with(|r| r.set(true));
}

/// Text that must never appear in a report, such as a stream key or a stream URL
/// in use: the hook replaces it with `<redacted>`. Kept for the life of the process
/// (at most [`MAX_SECRETS`], the oldest dropped first).
pub fn add_secret(secret: &str) {
    if secret.len() < 4 {
        return; // Too short to redact without hiding ordinary text.
    }
    let mut secrets = SECRETS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if secrets.iter().any(|s| s == secret) {
        return;
    }
    if secrets.len() == MAX_SECRETS {
        secrets.remove(0);
    }
    secrets.push(secret.to_owned());
}

/// Secrets [`add_secret`] keeps at most.
pub const MAX_SECRETS: usize = 16;

static SECRETS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn payload_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

/// A process that ended without a clean exit since the last start.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Unclean {
    /// Its process id.
    pub pid: u32,
    /// Its newest crash report, when it wrote one.
    pub report: Option<PathBuf>,
}

/// This process's running marker, held locked for the life of the session.
#[derive(Debug)]
pub struct Session {
    marker: Option<(File, PathBuf)>,
    unclean: Vec<Unclean>,
}

impl Session {
    /// Processes that ended without a clean exit since the last start.
    pub fn unclean(&self) -> &[Unclean] {
        &self.unclean
    }

    /// Removes the marker: call on a clean exit only. Dropping the session without it
    /// keeps the marker, as a crash would.
    pub fn end_clean(mut self) {
        if let Some((mut file, path)) = self.marker.take() {
            // Marked clean while still locked: another instance that takes the lock
            // between our close and the delete sees a clean exit, not a crash.
            let _ = file.set_len(0);
            let _ = file.write_all(CLEAN.as_bytes());
            let _ = file.flush();
            // Deleted while open: Unix unlinks at once; Windows deletes on close, which
            // std's default share mode (`FILE_SHARE_DELETE`) allows.
            let _ = fs::remove_file(&path);
            drop(file);
            let _ = fs::remove_file(path);
        }
    }

    /// The marker's path.
    pub fn marker(&self) -> Option<&Path> {
        self.marker.as_ref().map(|(_, p)| p.as_path())
    }
}

/// Installs the panic hook (once per process) and this process's running marker in
/// `dir`, which is created when missing. Returns what earlier sessions left behind.
/// The hook is installed also when the marker fails.
pub fn install(dir: &Path) -> io::Result<Session> {
    set_hook(dir.to_owned());
    open_session(dir, std::process::id(), unix_now())
}

static REPORT_DIR: OnceLock<PathBuf> = OnceLock::new();
static REPORT_SERIAL: AtomicU32 = AtomicU32::new(0);
static CAUGHT_REPORTS: AtomicU32 = AtomicU32::new(0);

/// What a marker holds after a clean exit.
const CLEAN: &str = "clean\n";

thread_local! {
    /// Set while this thread writes a report: a panic in the writer must not write again.
    static WRITING: Cell<bool> = const { Cell::new(false) };
    /// Set on threads that asked for reports ([`report_panics_on_this_thread`]).
    static REPORTING: Cell<bool> = const { Cell::new(false) };
    /// How many [`contain`] calls this thread is inside: a panic there is caught.
    static CONTAINED: Cell<u32> = const { Cell::new(0) };
}

fn set_hook(dir: PathBuf) {
    if REPORT_DIR.set(dir).is_err() {
        return; // Installed already.
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // `try_with`: thread-locals may be gone while a thread exits, and a panic in
        // the hook aborts the process. The writer itself must not panic: nothing in
        // it unwraps or indexes.
        let asked = REPORTING.try_with(Cell::get).unwrap_or(false);
        if asked && !WRITING.try_with(|w| w.replace(true)).unwrap_or(true) {
            if let Some(dir) = REPORT_DIR.get() {
                report_panic(dir, info);
            }
            let _ = WRITING.try_with(|w| w.set(false));
        }
        previous(info);
    }));
}

fn report_panic(dir: &Path, info: &PanicHookInfo<'_>) {
    let thread = std::thread::current();
    let message = payload_text(info.payload());
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "unknown".to_owned());
    let caught = CONTAINED.try_with(Cell::get).unwrap_or(0) > 0;
    // The last slot is kept for an uncaught panic, the one that may end the process.
    if caught && CAUGHT_REPORTS.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_REPORTS_PER_PROCESS {
        return;
    }
    let serial = REPORT_SERIAL.fetch_add(1, Ordering::Relaxed);
    if serial >= MAX_REPORTS_PER_PROCESS {
        return;
    }
    let backtrace = std::backtrace::Backtrace::force_capture().to_string();
    let home = home_dir();
    let report = Report {
        secs: unix_now(),
        caught,
        thread: thread.name().unwrap_or("unnamed"),
        message: &redact_secrets(&redact(&message, home.as_deref())),
        location: &location,
        backtrace: &redact_secrets(&redact(&backtrace, home.as_deref())),
    };
    let _ = write_report(dir, std::process::id(), serial, &report);
}

/// One panic, as the report file shows it.
#[derive(Debug)]
pub struct Report<'a> {
    /// When, in seconds since the Unix epoch.
    pub secs: u64,
    /// Inside [`contain`]: the app went on. Otherwise the panic may have ended the
    /// process or a thread.
    pub caught: bool,
    /// The panicking thread's name.
    pub thread: &'a str,
    /// The panic message.
    pub message: &'a str,
    /// `file:line:column`.
    pub location: &'a str,
    /// The captured backtrace.
    pub backtrace: &'a str,
}

impl Report<'_> {
    /// The report file's text (spec: crash-resilience.md, "Crash reports").
    pub fn text(&self) -> String {
        format!(
            "RBS {}\nOS: {} {}\nTime: {}\nKind: {}\nThread: {}\nMessage: {}\nLocation: {}\n\n{}\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            rfc3339(self.secs),
            if self.caught { "caught" } else { "uncaught" },
            self.thread,
            self.message,
            self.location,
            self.backtrace,
        )
    }
}

/// Writes `report` for process `pid` as `panic-<pid>-<secs>-<serial>.txt` in `dir`.
pub fn write_report(dir: &Path, pid: u32, serial: u32, report: &Report<'_>) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("panic-{pid}-{}-{serial}.txt", report.secs));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(report.text().as_bytes())?;
    Ok(path)
}

/// A report file's name: `(pid, secs, serial)`.
fn report_name(name: &str) -> Option<(u32, u64, u32)> {
    let rest = name.strip_prefix("panic-")?.strip_suffix(".txt")?;
    let mut parts = rest.split('-');
    let pid = parts.next()?.parse().ok()?;
    let secs = parts.next()?.parse().ok()?;
    let serial = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((pid, secs, serial))
}

/// Opens the session of process `pid`, started at `now` (seconds since the epoch):
/// reports and removes the markers of processes that ended without a clean exit,
/// prunes old reports and creates this process's marker, locked.
fn open_session(dir: &Path, pid: u32, now: u64) -> io::Result<Session> {
    fs::create_dir_all(dir)?;
    let mut reports: Vec<((u64, u32), u32, PathBuf)> = Vec::new();
    let mut markers: Vec<(u32, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir)?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some((pid, secs, serial)) = report_name(name) {
            reports.push(((secs, serial), pid, entry.path()));
        } else if let Some(pid) = name
            .strip_prefix("running-")
            .and_then(|r| r.strip_suffix(".lock"))
            .and_then(|r| r.parse::<u32>().ok())
        {
            markers.push((pid, entry.path()));
        } else if name.starts_with("starting-") && name.ends_with(".tmp") {
            // Left by a process that ended while it started; one still starting holds
            // its lock, and is left alone.
            if stale_marker(&entry.path()).is_some() {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    reports.sort_by_key(|r| r.0);
    let mut unclean = Vec::new();
    for (old_pid, path) in markers {
        let started = match stale_marker(&path) {
            None => continue, // Locked: a running instance.
            Some(Stale::Clean) => {
                // Ended cleanly; only its delete was not done yet.
                let _ = fs::remove_file(&path);
                continue;
            }
            Some(Stale::Unclean { started }) => started,
        };
        // The newest report of that process, written after it started (pids are
        // reused, so an older report belongs to another process).
        let report = reports
            .iter()
            .rev()
            .find(|((secs, _), p, _)| *p == old_pid && *secs >= started)
            .map(|(_, _, path)| path.clone());
        unclean.push(Unclean {
            pid: old_pid,
            report,
        });
        let _ = fs::remove_file(&path);
    }
    // Old reports go, except those the notice is about to show.
    let excess = reports.len().saturating_sub(KEEP_REPORTS);
    for (_, _, path) in reports.drain(..excess) {
        if !unclean.iter().any(|u| u.report.as_ref() == Some(&path)) {
            let _ = fs::remove_file(path);
        }
    }
    // Locked under a name the scan ignores, then renamed: another instance starting at
    // the same moment never sees an unlocked marker of a live process.
    let starting = dir.join(format!("starting-{pid}.tmp"));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&starting)?;
    file.try_lock().map_err(io::Error::from)?;
    file.write_all(format!("started {now}\n").as_bytes())?;
    let marker = dir.join(format!("running-{pid}.lock"));
    // Renaming the open, locked file works on Windows too: std opens files with
    // `FILE_SHARE_DELETE`.
    fs::rename(&starting, &marker)?;
    Ok(Session {
        marker: Some((file, marker)),
        unclean,
    })
}

/// A marker no live process holds.
enum Stale {
    /// Its process ended cleanly but had not deleted it yet.
    Clean,
    /// Its process ended without a clean exit; `started` is its start time (0 when
    /// unreadable).
    Unclean { started: u64 },
}

/// What the marker at `path` says when no live process holds it; `None` while one
/// does (or it cannot be opened).
fn stale_marker(path: &Path) -> Option<Stale> {
    let mut file = OpenOptions::new().read(true).write(true).open(path).ok()?;
    file.try_lock().ok()?;
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    if text == CLEAN {
        return Some(Stale::Clean);
    }
    let started = text
        .trim()
        .strip_prefix("started ")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Some(Stale::Unclean { started })
}

/// The user's home directory, from the environment.
fn home_dir() -> Option<String> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(var).ok().filter(|h| h.len() > 1)
}

/// URL schemes whose rest may hold a stream key or a passphrase.
const SECRET_SCHEMES: [&str; 5] = ["rtmp://", "rtmps://", "srt://", "rist://", "whip://"];

/// `text` with the home directory shown as `~` and everything after a stream URL's
/// scheme (host, path, key, query) replaced, so a report holds no stream key and no
/// user name from a path (ADR-0054 decision 4).
pub fn redact(text: &str, home: Option<&str>) -> String {
    let mut out = match home {
        Some(home) => replace_home(text, home, cfg!(windows)),
        None => text.to_owned(),
    };
    for scheme in SECRET_SCHEMES {
        let mut from = 0;
        while let Some(at) = lower_find(&out[from..], scheme) {
            let start = from + at + scheme.len();
            let end = out[start..]
                .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '>'))
                .map_or(out.len(), |e| start + e);
            out.replace_range(start..end, "<redacted>");
            from = start + "<redacted>".len();
        }
    }
    out
}

/// `text` with `home` shown as `~` where it is a whole path: followed by a separator,
/// the end, or a character that cannot be part of a file name in a report (quote,
/// space, colon). On Windows paths are matched ignoring ASCII case.
fn replace_home(text: &str, home: &str, ignore_case: bool) -> String {
    let home = home.trim_end_matches(['/', '\\']);
    if home.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let found = if ignore_case {
            lower_find(rest, home)
        } else {
            rest.find(home)
        };
        let Some(at) = found else {
            out.push_str(rest);
            return out;
        };
        let after = &rest[at + home.len()..];
        let whole = after
            .chars()
            .next()
            .is_none_or(|c| matches!(c, '/' | '\\' | '"' | '\'' | ' ' | ':' | '\n' | ')'));
        out.push_str(&rest[..at]);
        out.push_str(if whole {
            "~"
        } else {
            &rest[at..at + home.len()]
        });
        rest = after;
    }
}

/// `text` with every secret from [`add_secret`] replaced. Never waits: when another
/// thread holds the list, the text is returned as it is.
fn redact_secrets(text: &str) -> String {
    let Ok(secrets) = SECRETS.try_lock() else {
        return text.to_owned();
    };
    let mut out = text.to_owned();
    for secret in secrets.iter() {
        if out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), "<redacted>");
        }
    }
    out
}

/// Case-insensitive `find` of a `needle` (ASCII case only).
fn lower_find(hay: &str, needle: &str) -> Option<usize> {
    hay.as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `secs` since the Unix epoch as an RFC 3339 UTC time.
fn rfc3339(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rbs-crash-{name}-{}-{}",
            std::process::id(),
            REPORT_SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn report(secs: u64) -> Report<'static> {
        Report {
            secs,
            caught: true,
            thread: "main",
            message: "boom",
            location: "src/main.rs:1:2",
            backtrace: "0: frame",
        }
    }

    #[test]
    fn contain_returns_the_value_or_the_message() {
        assert_eq!(contain(|| 7), Ok(7));
        // The default hook prints these two panics to stderr; that is expected.
        let s = contain(|| -> u8 { panic!("static text") }).unwrap_err();
        assert_eq!(s.message, "static text");
        let n = 3;
        let f = contain(|| -> u8 { panic!("formatted {n}") }).unwrap_err();
        assert_eq!(f.message, "formatted 3");
        let other = contain(|| std::panic::panic_any(5_u32)).unwrap_err();
        assert_eq!(other.message, "unknown panic");
    }

    #[test]
    fn report_has_the_fields_in_order() {
        let text = report(86_400 * 365 + 3_661).text();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("RBS "));
        assert!(lines[1].starts_with("OS: "));
        assert_eq!(lines[2], "Time: 1971-01-01T01:01:01Z");
        assert_eq!(lines[3], "Kind: caught");
        assert_eq!(lines[4], "Thread: main");
        assert_eq!(lines[5], "Message: boom");
        assert_eq!(lines[6], "Location: src/main.rs:1:2");
        assert_eq!(lines[7], "");
        assert_eq!(lines[8], "0: frame");
    }

    #[test]
    fn redact_hides_home_and_stream_urls() {
        let text = "open /Users/sc/Movies/a.mkv to RTMP://live.example/app/sk_live_123 \
                    and srt://h:9000?streamid=x&passphrase=y\nok";
        assert_eq!(
            redact(text, Some("/Users/sc")),
            "open ~/Movies/a.mkv to RTMP://<redacted> \
                    and srt://<redacted>\nok"
        );
        assert_eq!(redact("rtmp://", None), "rtmp://<redacted>");
        assert_eq!(redact("nothing", None), "nothing");
    }

    #[test]
    fn home_is_replaced_only_as_a_whole_path() {
        assert_eq!(
            replace_home(
                "/Users/sc/a and /Users/scott/b and /Users/sc",
                "/Users/sc",
                false
            ),
            "~/a and /Users/scott/b and ~"
        );
        assert_eq!(
            replace_home(r"C:\users\SC\x.mkv", r"C:\Users\sc", true),
            r"~\x.mkv"
        );
        assert_eq!(replace_home("/home/a/b", "/home/a/", false), "~/b");
    }

    #[test]
    fn secrets_are_redacted() {
        add_secret("live_abcdef123");
        add_secret("ab"); // Too short: ignored.
        assert_eq!(
            redact_secrets("key live_abcdef123 in https://x/live_abcdef123/ ab"),
            "key <redacted> in https://x/<redacted>/ ab"
        );
    }

    #[test]
    fn a_clean_marker_left_behind_is_not_a_crash() {
        let dir = temp_dir("clean-left");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("running-600.lock"), CLEAN).unwrap();
        fs::write(dir.join("starting-601.tmp"), "").unwrap();
        let s = open_session(&dir, 602, 1_000).unwrap();
        assert!(s.unclean().is_empty());
        assert!(!dir.join("running-600.lock").exists());
        assert!(!dir.join("starting-601.tmp").exists());
        s.end_clean();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_report_shown_is_never_pruned() {
        let dir = temp_dir("keep-shown");
        let crashed = open_session(&dir, 700, 1_000).unwrap();
        let shown = write_report(&dir, 700, 0, &report(1_001)).unwrap();
        for i in 0..KEEP_REPORTS as u64 {
            write_report(&dir, 800 + i as u32, 0, &report(2_000 + i)).unwrap();
        }
        drop(crashed);
        let next = open_session(&dir, 701, 3_000).unwrap();
        assert_eq!(next.unclean()[0].report.as_ref(), Some(&shown));
        assert!(shown.exists());
        next.end_clean();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn contain_marks_the_thread_while_inside() {
        assert_eq!(CONTAINED.with(Cell::get), 0);
        let inner = contain(|| CONTAINED.with(Cell::get)).unwrap();
        assert_eq!(inner, 1);
        let _ = contain(|| -> u8 { panic!("unwinds out") });
        assert_eq!(CONTAINED.with(Cell::get), 0);
    }

    #[test]
    fn rfc3339_handles_leap_days() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_793_966_400), "2026-11-06T12:00:00Z");
    }

    #[test]
    fn dropped_session_is_reported_once_with_its_report() {
        let dir = temp_dir("dropped");
        let crashed = open_session(&dir, 100, 1_000).unwrap();
        // An older report of the same pid belongs to an earlier process.
        write_report(&dir, 100, 0, &report(900)).unwrap();
        let newest = write_report(&dir, 100, 1, &report(1_010)).unwrap();
        write_report(&dir, 200, 2, &report(1_020)).unwrap();
        drop(crashed);
        let next = open_session(&dir, 101, 2_000).unwrap();
        assert_eq!(
            next.unclean(),
            &[Unclean {
                pid: 100,
                report: Some(newest)
            }]
        );
        assert!(!dir.join("running-100.lock").exists());
        next.end_clean();
        let after = open_session(&dir, 102, 3_000).unwrap();
        assert!(after.unclean().is_empty());
        after.end_clean();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn live_session_is_left_alone() {
        let dir = temp_dir("live");
        let live = open_session(&dir, 300, 1_000).unwrap();
        let other = open_session(&dir, 301, 1_001).unwrap();
        assert!(other.unclean().is_empty());
        assert!(dir.join("running-300.lock").exists());
        other.end_clean();
        live.end_clean();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_exit_leaves_no_marker() {
        let dir = temp_dir("clean");
        let s = open_session(&dir, 400, 1_000).unwrap();
        let marker = s.marker().unwrap().to_owned();
        s.end_clean();
        assert!(!marker.exists());
        let next = open_session(&dir, 401, 2_000).unwrap();
        assert!(next.unclean().is_empty());
        next.end_clean();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_the_newest_reports() {
        let dir = temp_dir("retention");
        for i in 0..(KEEP_REPORTS as u64 + 5) {
            // Pids differ so name order is not time order.
            write_report(&dir, 900 - i as u32, 0, &report(10_000 + i)).unwrap();
        }
        let s = open_session(&dir, 500, 20_000).unwrap();
        let mut left: Vec<u64> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| report_name(e.unwrap().file_name().to_str()?).map(|r| r.1))
            .collect();
        left.sort_unstable();
        let expected: Vec<u64> = (5..KEEP_REPORTS as u64 + 5).map(|i| 10_000 + i).collect();
        assert_eq!(left, expected);
        s.end_clean();
        fs::remove_dir_all(&dir).unwrap();
    }
}
