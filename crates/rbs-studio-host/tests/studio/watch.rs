//! A hang guard for the whole test binary (ADR-0032): a test that never ends turns into a
//! failure that names it, instead of a run that stops with no output.
//!
//! Every studio host a test starts sends its updates through [`forward`], which keeps a
//! row per host until the host's control thread ends (it drops the closure). A watchdog
//! thread, started with the first host, looks at the rows; when one has been alive for
//! [`STALL`], far beyond any test (seconds) and every update wait (`LIMIT`, 20 s), it
//! prints every live host, the update each one sent last, and the panic its test thread
//! hit, if any, then ends the process.
//!
//! A test thread that panicked (for example on a 20 s update wait) and is still listed is
//! stuck dropping its `StudioHost`: the drop joins a control thread that does not end.
//! A listed test with no panic is stuck in a wait of its own.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use rbs_studio_host::studio_host::Update;

/// Hang guard only: how long a host may live before the run is called stuck.
const STALL: Duration = Duration::from_secs(120);

/// How often the watchdog looks.
const LOOK: Duration = Duration::from_secs(5);

/// Bytes of an update's debug text kept as "last update".
const LAST_TEXT: usize = 100;

struct Live {
    test: String,
    since: Instant,
    updates: u64,
    last: String,
    last_at: Option<Instant>,
}

struct Watch {
    hosts: Mutex<BTreeMap<u64, Live>>,
    /// Test thread name to its first panic (message and place).
    panics: Mutex<BTreeMap<String, String>>,
    next: AtomicU64,
}

fn watch() -> &'static Watch {
    static WATCH: OnceLock<Watch> = OnceLock::new();
    WATCH.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let name = std::thread::current()
                .name()
                .unwrap_or("unnamed")
                .to_owned();
            if let Some(w) = WATCH.get() {
                w.panics
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entry(name)
                    .or_insert_with(|| info.to_string());
            }
            previous(info);
        }));
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
            next: AtomicU64::new(0),
        }
    })
}

impl Watch {
    fn look(&self) {
        let hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        if !hosts.values().any(|h| h.since.elapsed() >= STALL) {
            return;
        }
        let panics = self.panics.lock().unwrap_or_else(PoisonError::into_inner);
        let mut report = format!(
            "\nSTUDIO TESTS STUCK: a studio host is still alive after {} s. Live hosts, oldest first:\n",
            STALL.as_secs()
        );
        let mut rows: Vec<&Live> = hosts.values().collect();
        rows.sort_by_key(|h| h.since);
        for h in rows {
            let _ = writeln!(
                report,
                "- {} (alive {} s, {} updates, last {} s ago: {})",
                h.test,
                h.since.elapsed().as_secs(),
                h.updates,
                h.last_at
                    .map_or_else(|| "never".to_owned(), |t| t.elapsed().as_secs().to_string()),
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
            },
        );
    let registered = Registered(id);
    move |u| {
        let Registered(id) = &registered;
        let mut last = Short(String::new());
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
        }
        let _ = tx.send(u);
    }
}

/// A string that takes the first [`LAST_TEXT`] bytes and then stops the formatting.
struct Short(String);

impl std::fmt::Write for Short {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let room = LAST_TEXT.saturating_sub(self.0.len());
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
