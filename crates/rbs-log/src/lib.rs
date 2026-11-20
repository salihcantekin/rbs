//! The RBS diagnostics log file (ADR-0158, spec `diagnostics-log.md`).
//!
//! [`start`] opens this run's file and starts the `rbs-log` thread; its
//! [`Logger::sink`] is installed with `rbs_core::log::install`. The sink formats a line
//! on the calling thread and hands it over without waiting; the thread redacts it,
//! writes it, rotates and prunes files and keeps the last lines in memory.
//! [`start_stderr`] does the same without a file, writing to standard error.
//!
//! Used only by composition roots and helper executables (xtask layer class
//! Diagnostics).

mod bridge;
mod files;
mod filters;
mod time;
mod writer;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime};

use rbs_core::log::{Record, Sink};

pub use bridge::bridge_log_crate;
pub use files::Limits;
pub use filters::{ParseError, apply_level, parse_filters, set_filter};

use files::Part;
use writer::{Line, Ring, Writer};

/// How long a repeat count waits before it is written anyway.
const REPEAT_WAIT: Duration = Duration::from_secs(60);

/// What [`start`] needs.
#[derive(Debug, Clone)]
pub struct Config {
    /// The logs folder; created when missing.
    pub dir: PathBuf,
    /// The file name prefix: `rbs`, or a helper's own (`rbs-browser`).
    pub prefix: String,
    /// The process id, part of the file name.
    pub pid: u32,
    /// First lines of the file (version, OS, levels).
    pub header: Vec<String>,
    /// Size limits.
    pub limits: Limits,
    /// Lines that may wait for the writer; more are dropped and counted.
    pub queue: usize,
    /// The home directory, shown as `~`.
    pub home: Option<String>,
    /// The clock for line times and the file name; tests pass a fixed one.
    pub now: fn() -> SystemTime,
    /// Also write each line to standard error (from the writer thread), for a
    /// developer who started RBS from a terminal.
    pub echo: bool,
}

impl Config {
    /// The defaults for a process with `prefix` writing into `dir`.
    pub fn new(dir: PathBuf, prefix: &str) -> Config {
        Config {
            dir,
            prefix: prefix.to_owned(),
            pid: std::process::id(),
            header: Vec::new(),
            limits: Limits::default(),
            queue: 1024,
            home: rbs_crash::home_dir(),
            now: SystemTime::now,
            echo: false,
        }
    }
}

enum Msg {
    Line(Line),
    Flush(Sender<()>),
}

struct FileSink {
    tx: Sender<Msg>,
    queued: Arc<AtomicUsize>,
    capacity: usize,
    dropped: Arc<AtomicU64>,
    now: fn() -> SystemTime,
}

impl Sink for FileSink {
    fn write(&self, r: &Record<'_>) {
        if self.queued.fetch_add(1, Ordering::AcqRel) >= self.capacity {
            self.queued.fetch_sub(1, Ordering::AcqRel);
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let line = Line {
            time: (self.now)(),
            level: r.level,
            area: r.area,
            thread: std::thread::current().name().unwrap_or("?").to_owned(),
            file: r.file,
            line: r.line,
            text: r.args.to_string(),
        };
        if self.tx.send(Msg::Line(line)).is_err() {
            // The writer thread is gone.
            self.queued.fetch_sub(1, Ordering::AcqRel);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A running log file.
pub struct Logger {
    sink: &'static FileSink,
    ring: Ring,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logger").field("path", &self.path).finish()
    }
}

/// Prunes old runs, opens this run's file, writes the header and starts the `rbs-log`
/// thread. Nothing is installed: pass [`Logger::sink`] to `rbs_core::log::install`.
pub fn start(config: Config) -> std::io::Result<Logger> {
    let part = Part {
        time: time::compact((config.now)()),
        pid: config.pid,
        segment: 0,
    };
    let writer = Writer::open(
        &config.dir,
        &config.prefix,
        part,
        config.limits,
        config.home.clone(),
        &config.header,
        config.echo,
    )?;
    spawn(writer, config.queue, config.now)
}

/// Starts the `rbs-log` thread with no file: each line, redacted and with repeats
/// collapsed, goes to standard error (the CLI, whose user reads its terminal).
/// `queue` and `home` as in [`Config`].
pub fn start_stderr(queue: usize, home: Option<String>) -> std::io::Result<Logger> {
    spawn(Writer::stderr(home), queue, SystemTime::now)
}

fn spawn(writer: Writer, queue: usize, now: fn() -> SystemTime) -> std::io::Result<Logger> {
    let ring = writer.ring();
    let path = writer.path();
    let (tx, rx) = mpsc::channel();
    let queued = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    {
        let queued = Arc::clone(&queued);
        let dropped = Arc::clone(&dropped);
        std::thread::Builder::new()
            .name("rbs-log".to_owned())
            .spawn(move || run(writer, &rx, &queued, &dropped))?;
    }
    let sink: &'static FileSink = Box::leak(Box::new(FileSink {
        tx,
        queued,
        capacity: queue.max(1),
        dropped,
        now,
    }));
    Ok(Logger { sink, ring, path })
}

/// The `rbs-log` thread: blocks on the queue (with a bound only while a repeat count is
/// pending) and flushes the file whenever the queue is empty.
fn run(mut w: Writer, rx: &Receiver<Msg>, queued: &AtomicUsize, dropped: &AtomicU64) {
    // When a pending repeat count is written at the latest: 60 s after it began.
    let mut deadline: Option<Instant> = None;
    loop {
        if !w.repeat_pending() {
            deadline = None;
        } else if deadline.is_none() {
            deadline = Some(Instant::now() + REPEAT_WAIT);
        }
        let msg = if let Some(at) = deadline {
            match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => {
                    w.end_repeat();
                    w.flush_file();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(m) => m,
                Err(_) => break,
            }
        };
        handle(&mut w, msg, queued, dropped);
        while let Ok(m) = rx.try_recv() {
            handle(&mut w, m, queued, dropped);
        }
        if deadline.is_some_and(|at| Instant::now() >= at) {
            w.end_repeat();
        }
        w.flush_file();
    }
    w.end_repeat();
    w.flush_file();
}

fn handle(w: &mut Writer, msg: Msg, queued: &AtomicUsize, dropped: &AtomicU64) {
    match msg {
        Msg::Line(line) => {
            queued.fetch_sub(1, Ordering::AcqRel);
            w.line(&line, dropped.swap(0, Ordering::Relaxed));
        }
        Msg::Flush(done) => {
            w.end_repeat();
            w.flush_file();
            let _ = done.send(());
        }
    }
}

impl Logger {
    /// The sink to install with `rbs_core::log::install`.
    pub fn sink(&self) -> &'static dyn Sink {
        self.sink
    }

    /// Writes every line queued so far, a pending repeat count included, and waits at
    /// most `wait` for that. True when it was written in time. Never call it on the UI
    /// thread.
    pub fn flush(&self, wait: Duration) -> bool {
        let (done, answer) = mpsc::channel();
        if self.sink.tx.send(Msg::Flush(done)).is_err() {
            return false;
        }
        answer.recv_timeout(wait).is_ok()
    }

    /// The last written lines (redacted), oldest first; empty when the writer holds
    /// them at this moment. Never waits.
    pub fn recent(&self) -> Vec<String> {
        match self.ring.try_lock() {
            Ok(ring) => ring.iter().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The file written when the run started; `None` for [`start_stderr`].
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

/// The logs folder of this system: `RBS_LOG_DIR` when set, else macOS
/// `~/Library/Logs/RBS`, Windows `%LOCALAPPDATA%\RBS\logs`, others
/// `$XDG_STATE_HOME/rbs/logs` (default `~/.local/state/rbs/logs`).
pub fn default_dir() -> Option<PathBuf> {
    let var = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if let Some(dir) = var("RBS_LOG_DIR") {
        return Some(dir);
    }
    if cfg!(target_os = "macos") {
        return var("HOME").map(|h| h.join("Library").join("Logs").join("RBS"));
    }
    if cfg!(windows) {
        return var("LOCALAPPDATA").map(|h| h.join("RBS").join("logs"));
    }
    var("XDG_STATE_HOME")
        .or_else(|| var("HOME").map(|h| h.join(".local").join("state")))
        .map(|s| s.join("rbs").join("logs"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_core::log::{Area, Level};

    #[test]
    fn a_full_queue_drops_and_counts_lines_without_waiting() {
        // Nobody reads this channel: the queue fills and stays full.
        let (tx, rx) = mpsc::channel();
        let sink = FileSink {
            tx,
            queued: Arc::new(AtomicUsize::new(0)),
            capacity: 2,
            dropped: Arc::new(AtomicU64::new(0)),
            now: SystemTime::now,
        };
        for _ in 0..5 {
            sink.write(&Record {
                level: Level::Info,
                area: Area::App,
                args: format_args!("x"),
                file: file!(),
                line: line!(),
            });
        }
        assert_eq!(sink.queued.load(Ordering::Relaxed), 2);
        assert_eq!(sink.dropped.load(Ordering::Relaxed), 3);
        assert_eq!(rx.try_iter().count(), 2);
    }
}
