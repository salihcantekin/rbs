//! A hang guard for the whole test binary (ADR-0032): a test that never ends turns into a
//! failure that names it, instead of a run that stops with no output.
//!
//! Every studio host a test starts sends its updates through [`forward`], which keeps a
//! row per host until the host's control thread ends (it drops the closure). A watchdog
//! thread, started with the first host, looks at the rows; when one has been alive for
//! [`STALL`], far beyond any test (seconds) and every update wait (`LIMIT`, 20 s), it
//! prints every live host, the update each one sent last, the panic its test thread hit
//! (with the test's own frames), the last diagnostics log lines of its control thread
//! and of the whole process, then ends the process.
//!
//! A test thread that panicked (for example on a 20 s update wait) and is still listed is
//! stuck dropping its `StudioHost`: the drop joins a control thread that does not end.
//! A listed test with no panic is stuck in a wait of its own.
//!
//! The watchdog installs the process's log sink and turns every area to Trace, so the
//! report shows what the control threads did last (ADR-0158; a ring in memory, no file).

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use rbs_studio_host::studio_host::Update;

/// Hang guard only: how long a host may live before the run is called stuck.
const STALL: Duration = Duration::from_secs(120);

/// How often the watchdog looks.
const LOOK: Duration = Duration::from_secs(5);

/// Bytes of an update's debug text kept as "last update".
const LAST_TEXT: usize = 100;

/// Log lines kept for the report.
const LOG_LINES: usize = 400;

/// Of those, how many the report shows per stuck control thread and for the process.
const SHOWN_PER_HOST: usize = 25;
const SHOWN_LAST: usize = 40;

/// Test frames of a panic's backtrace kept.
const PANIC_FRAMES: usize = 12;

struct Live {
    test: String,
    since: Instant,
    updates: u64,
    last: String,
    last_at: Option<Instant>,
    control: Option<ThreadId>,
}

struct Line {
    at: Instant,
    thread: ThreadId,
    text: String,
}

struct Watch {
    hosts: Mutex<BTreeMap<u64, Live>>,
    /// Test thread name to its first panic (message, place and the test's frames).
    panics: Mutex<BTreeMap<String, String>>,
    log: Mutex<VecDeque<Line>>,
    next: AtomicU64,
    started: Instant,
}

/// Keeps the newest log lines in memory.
struct Ring;

impl rbs_core::log::Sink for Ring {
    fn write(&self, record: &rbs_core::log::Record<'_>) {
        let Some(w) = WATCH.get() else { return };
        let me = std::thread::current();
        let mut text = Short(String::new(), 300);
        let _ = write!(
            text,
            "[{}] {} {} {}:{} {}",
            me.name().unwrap_or("unnamed"),
            record.level.label(),
            record.area.name(),
            record.file,
            record.line,
            record.args
        );
        let mut log = w.log.lock().unwrap_or_else(PoisonError::into_inner);
        if log.len() == LOG_LINES {
            log.pop_front();
        }
        log.push_back(Line {
            at: Instant::now(),
            thread: me.id(),
            text: text.0,
        });
    }
}

static WATCH: OnceLock<Watch> = OnceLock::new();

fn watch() -> &'static Watch {
    WATCH.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let name = std::thread::current()
                .name()
                .unwrap_or("unnamed")
                .to_owned();
            if let Some(w) = WATCH.get() {
                let mut panics = w.panics.lock().unwrap_or_else(PoisonError::into_inner);
                panics
                    .entry(name)
                    .or_insert_with(|| format!("{info}\n{}", test_frames()));
            }
            previous(info);
        }));
        static RING: Ring = Ring;
        if rbs_core::log::install(&RING).is_ok() {
            rbs_core::log::set_all(rbs_core::log::Filter::Trace);
        }
        let _ = std::thread::Builder::new()
            .name("studio-test-watchdog".into())
            .spawn(|| {
                // Never sent to: `recv_timeout` is the watchdog's clock-free pause.
                let (_keep, pause) = mpsc::channel::<()>();
                loop {
                    let _ = pause.recv_timeout(LOOK);
                    if let Some(w) = WATCH.get() {
                        w.look();
                    }
                }
            });
        Watch {
            hosts: Mutex::new(BTreeMap::new()),
            panics: Mutex::new(BTreeMap::new()),
            log: Mutex::new(VecDeque::with_capacity(LOG_LINES)),
            next: AtomicU64::new(0),
            started: Instant::now(),
        }
    })
}

/// The frames of this test binary's own code in the current backtrace, innermost
/// first: where in the test the panic came from.
fn test_frames() -> String {
    let trace = std::backtrace::Backtrace::force_capture().to_string();
    let mut out = String::new();
    let mut lines = trace.lines().peekable();
    let mut kept = 0;
    while let Some(l) = lines.next() {
        let frame = l.trim_start();
        // `N: studio::record::...` and the `at path:line` line after it.
        let ours = frame.split_once(": ").is_some_and(|(n, f)| {
            n.chars().all(|c| c.is_ascii_digit()) && f.starts_with("studio::")
        });
        if !ours || frame.contains("studio::watch::") {
            continue;
        }
        let _ = writeln!(out, "      {frame}");
        if let Some(at) = lines.peek().filter(|a| a.trim_start().starts_with("at ")) {
            let _ = writeln!(out, "        {}", at.trim_start());
        }
        kept += 1;
        if kept == PANIC_FRAMES {
            break;
        }
    }
    out
}

impl Watch {
    fn look(&self) {
        let hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        if !hosts.values().any(|h| h.since.elapsed() >= STALL) {
            return;
        }
        let panics = self.panics.lock().unwrap_or_else(PoisonError::into_inner);
        let log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        let secs = |t: Instant| t.saturating_duration_since(self.started).as_secs_f64();
        let mut report = format!(
            "\nSTUDIO TESTS STUCK: a studio host is still alive after {} s (run time {:.1} s). Live hosts, oldest first:\n",
            STALL.as_secs(),
            self.started.elapsed().as_secs_f64(),
        );
        let mut rows: Vec<&Live> = hosts.values().collect();
        rows.sort_by_key(|h| h.since);
        for h in rows {
            let _ = writeln!(
                report,
                "- {} (started at {:.1} s, {} updates, last at {}: {})",
                h.test,
                secs(h.since),
                h.updates,
                h.last_at
                    .map_or_else(|| "never".to_owned(), |t| format!("{:.1} s", secs(t))),
                if h.last.is_empty() { "none" } else { &h.last },
            );
            match panics.get(&h.test) {
                Some(p) => {
                    let _ = writeln!(
                        report,
                        "    the test panicked, so it is stuck dropping the host (its control thread does not end): {p}"
                    );
                }
                None => {
                    let _ = writeln!(
                        report,
                        "    no panic: the test thread is in a wait without a hang guard"
                    );
                }
            }
            let mine: Vec<&Line> = log.iter().filter(|l| Some(l.thread) == h.control).collect();
            let _ = writeln!(
                report,
                "    its control thread's last log lines ({} kept):",
                mine.len()
            );
            for l in mine.iter().rev().take(SHOWN_PER_HOST).rev() {
                let _ = writeln!(report, "      {:.3} s {}", secs(l.at), l.text);
            }
        }
        let _ = writeln!(report, "The process's last log lines:");
        for l in log.iter().rev().take(SHOWN_LAST).rev() {
            let _ = writeln!(report, "  {:.3} s {}", secs(l.at), l.text);
        }
        rbs_crash::errln!("{report}");
        std::process::exit(101);
    }
}

/// The removal of a host's row when its control thread drops the notify closure.
struct Registered(u64);

impl Drop for Registered {
    fn drop(&mut self) {
        watch()
            .hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// A host's `notify`: sends each update to `tx` and records it for the watchdog. Call on
/// the test's own thread, which names the row.
pub(crate) fn forward(tx: Sender<Update>) -> impl Fn(Update) + Send + 'static {
    let w = watch();
    let id = w.next.fetch_add(1, Ordering::Relaxed);
    let test = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_owned();
    w.hosts
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            id,
            Live {
                test,
                since: Instant::now(),
                updates: 0,
                last: String::new(),
                last_at: None,
                control: None,
            },
        );
    let registered = Registered(id);
    move |u| {
        let Registered(id) = &registered;
        let mut last = Short(String::new(), LAST_TEXT);
        let _ = write!(last, "{u:?}");
        if let Some(h) = watch()
            .hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(id)
        {
            h.updates += 1;
            h.last = last.0;
            h.last_at = Some(Instant::now());
            // `notify` runs on the host's control thread.
            h.control = Some(std::thread::current().id());
        }
        let _ = tx.send(u);
    }
}

/// A string that takes the first `.1` bytes and then stops the formatting.
struct Short(String, usize);

impl std::fmt::Write for Short {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let room = self.1.saturating_sub(self.0.len());
        if s.len() <= room {
            self.0.push_str(s);
            return Ok(());
        }
        let mut end = room;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.0.push_str(&s[..end]);
        self.0.push('…');
        Err(std::fmt::Error)
    }
}
