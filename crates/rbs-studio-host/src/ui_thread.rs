//! The UI thread's rules at run time (docs/architecture/threads.md, crash-resilience
//! spec "UI thread waits and stalls").
//!
//! - [`loop_started`] marks the calling thread as the UI thread while the event loop
//!   runs, and starts the stall watch; [`loop_ended`] takes the mark away again, so the
//!   steps after the loop (which may join) are not checked.
//! - [`assert_off_ui`] is a debug check for helpers that wait on another thread (a join,
//!   a channel receive, a condition variable): they must never run on the UI thread.
//! - [`enter`] is called by every UI entry point (`entry.rs` in the app): it stamps when
//!   the outermost entry began and where it was installed. When that entry returns
//!   after [`STALL`] or more, the UI thread logs one line with the time and the place.
//!   An entry that does not return is seen by the watch thread `rbs-ui-watch`, which
//!   logs after [`HANG`], then again each time the wait doubles.
//! - [`os_loop`] wraps a loop the OS runs inside an entry (the projector's native menu,
//!   ADR-0137 decision 1): the time the user spends in it is not the entry's, and the
//!   entries that loop runs are watched as outermost ones, so a stall in one of them is
//!   still seen.
//!
//! Cost: two clock reads and three atomic operations per outermost entry, nothing in
//! nested ones. The watch thread is parked while the UI thread is idle; while the UI
//! thread works it wakes at most about once per [`HANG`].

use std::cell::Cell;
use std::panic::Location;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::Thread;
use std::time::{Duration, Instant};

use rbs_core::log::{Area, Level};

/// An entry that runs this long is logged when it returns.
pub const STALL: Duration = Duration::from_millis(100);
/// An entry that runs this long without returning is logged by the watch thread.
pub const HANG: Duration = Duration::from_secs(1);
/// An entry that runs this long is a long task: the UI could not answer input for
/// that time. The measure mode counts them (performance-budgets spec, "Desktop UI").
pub const LONG_TASK: Duration = Duration::from_millis(50);

/// Told about every long task ([`LONG_TASK`]) with its time and the place its entry
/// was installed ([`set_long_task_hook`]).
pub type LongTaskHook = fn(Duration, &'static Location<'static>);

thread_local! {
    /// This thread runs the UI event loop now.
    static UI: Cell<bool> = const { Cell::new(false) };
    /// Entries open on this thread; only the outermost one stamps.
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    /// Nanoseconds the open outermost entry spent in [`os_loop`]; taken when it ends.
    static AWAY: Cell<u64> = const { Cell::new(0) };
    /// Told about long tasks on this thread while set (the measure mode).
    static LONG_TASKS: Cell<Option<LongTaskHook>> = const { Cell::new(None) };
}

/// Tells `hook` about every long task on the calling (UI) thread from now on; `None`
/// stops it. Read only when an entry ran [`LONG_TASK`] or longer, so a quick entry
/// costs nothing more.
pub fn set_long_task_hook(hook: Option<LongTaskHook>) {
    LONG_TASKS.with(|h| h.set(hook));
}

static WATCH: OnceLock<Arc<Watch>> = OnceLock::new();

/// Whether the calling thread is the UI thread while its event loop runs.
pub fn on_ui_thread() -> bool {
    UI.with(Cell::get)
}

/// A debug check at the top of a helper that waits on another thread: `what` names
/// the wait in the message. Never call such a helper on the UI thread.
#[track_caller]
pub fn assert_off_ui(what: &str) {
    debug_assert!(
        !on_ui_thread(),
        "{what} waits on another thread; never on the UI thread (docs/architecture/threads.md)"
    );
}

/// The event loop starts on this thread: marks it, and starts the watch thread once.
pub fn loop_started() {
    UI.with(|u| u.set(true));
    if WATCH.get().is_some() {
        return;
    }
    let watch = Arc::new(Watch::new(Instant::now()));
    let seen = watch.clone();
    let spawned = std::thread::Builder::new()
        .name("rbs-ui-watch".into())
        .spawn(move || seen.run());
    match spawned {
        Ok(handle) => {
            let _ = watch.thread.set(handle.thread().clone());
            let _ = WATCH.set(watch);
        }
        Err(e) => rbs_core::warn!(Area::Ui, "no stall watch: {e}"),
    }
}

/// The event loop has ended: the thread is no longer the UI thread.
pub fn loop_ended() {
    UI.with(|u| u.set(false));
}

/// Runs the event loop `run` between [`loop_started`] and [`loop_ended`], so the UI
/// thread's waits are checked and its stalls logged only while the loop runs; the
/// steps after it may join.
pub fn run_loop<R>(run: impl FnOnce() -> R) -> R {
    loop_started();
    let ran = run();
    loop_ended();
    ran
}

/// Runs `run`, a call in which the OS runs its own loop on the UI thread until the user
/// is done (a native menu, ADR-0137 decision 1). While it runs, the open entry is not
/// timed, so the watch does not report the user's time in the menu as a stall, and the
/// entries the OS loop runs stamp as outermost ones, so a real stall in one of them is
/// reported with its own place. After it, the open entry is timed again without that
/// time. Off the UI thread, or before the watch started, it only runs `run`.
pub fn os_loop<R>(run: impl FnOnce() -> R) -> R {
    let Some(watch) = WATCH.get().filter(|_| on_ui_thread()) else {
        return run();
    };
    let depth = DEPTH.with(|d| d.replace(0));
    let away = AWAY.with(Cell::take);
    // The open entry's stamp (0: none is open); the watch sees the thread idle.
    let since = watch.since.swap(0, Ordering::SeqCst);
    let at = watch.try_place(since);
    let from = watch.now();
    let ran = run();
    let spent = watch.now().saturating_sub(from);
    DEPTH.with(|d| d.set(depth));
    if since == 0 {
        AWAY.with(|a| a.set(away));
    } else {
        AWAY.with(|a| a.set(away.saturating_add(spent)));
        watch.stamp(since.saturating_add(spent), at);
    }
    ran
}

/// Called by each UI entry point with the place it was installed. The returned value
/// ends the entry when dropped (also while unwinding).
pub fn enter(at: &'static Location<'static>) -> Entry {
    let depth = DEPTH.with(|d| {
        let n = d.get();
        d.set(n.saturating_add(1));
        n
    });
    if depth > 0 || !on_ui_thread() {
        return Entry { since: None };
    }
    let since = WATCH.get().map(|w| (w.clone(), w.begin(at), at));
    Entry { since }
}

/// One open UI entry ([`enter`]).
pub struct Entry {
    since: Option<(Arc<Watch>, u64, &'static Location<'static>)>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        if let Some((watch, since, at)) = self.since.take() {
            watch.since.store(0, Ordering::Release);
            // Time in an OS loop (`os_loop`) is not the entry's.
            let since = since.saturating_add(AWAY.with(Cell::take));
            let now = watch.now();
            tell_long(Duration::from_nanos(now.saturating_sub(since)), at);
            if let Some(stall) = finished(since, now, at) {
                stall.log();
            }
        }
    }
}

/// Tells the long-task hook, if one is set, about an entry that took `took`.
fn tell_long(took: Duration, at: &'static Location<'static>) {
    if took >= LONG_TASK
        && let Some(hook) = LONG_TASKS.with(Cell::get)
    {
        hook(took, at);
    }
}

/// The entry stamped `since`, installed at `at`, returned at `now`: a stall when it
/// took [`STALL`] or more.
fn finished(since: u64, now: u64, at: &'static Location<'static>) -> Option<Stall> {
    let took = Duration::from_nanos(now.saturating_sub(since));
    (took >= STALL).then_some(Stall {
        took,
        at: Some(at),
        done: true,
    })
}

/// The stamps the UI thread writes and the watch thread reads.
struct Watch {
    base: Instant,
    /// Nanoseconds since `base` + 1 when the open entry began; 0 while idle.
    since: AtomicU64,
    /// The open entry's stamp and where it was installed. The UI thread only tries the
    /// lock (a miss leaves the place unknown); the watch thread takes it to report.
    at: Mutex<(u64, Option<&'static Location<'static>>)>,
    /// The watch thread is parked (or about to be) and needs an unpark.
    parked: AtomicBool,
    thread: OnceLock<Thread>,
}

impl Watch {
    fn new(base: Instant) -> Self {
        Self {
            base,
            since: AtomicU64::new(0),
            at: Mutex::new((0, None)),
            parked: AtomicBool::new(false),
            thread: OnceLock::new(),
        }
    }

    fn now(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_nanos())
            .unwrap_or(u64::MAX - 1)
            .saturating_add(1)
    }

    /// The UI thread begins an outermost entry; returns its stamp.
    fn begin(&self, at: &'static Location<'static>) -> u64 {
        let since = self.now();
        self.stamp(since, Some(at));
        since
    }

    /// The open entry, installed at `at`, is timed from `since` on (UI thread).
    fn stamp(&self, since: u64, at: Option<&'static Location<'static>>) {
        if let Ok(mut place) = self.at.try_lock() {
            *place = (since, at);
        }
        // SeqCst with the watch thread's flag store and stamp load: one of the two sides
        // always sees the other, so a busy UI thread never leaves the watch parked.
        self.since.store(since, Ordering::SeqCst);
        if self.parked.swap(false, Ordering::SeqCst)
            && let Some(t) = self.thread.get()
        {
            t.unpark();
        }
    }

    /// Where the entry stamped `since` was installed, if known, without waiting for the
    /// lock (UI thread).
    fn try_place(&self, since: u64) -> Option<&'static Location<'static>> {
        let place = *self.at.try_lock().ok()?;
        (since != 0 && place.0 == since)
            .then_some(place.1)
            .flatten()
    }

    /// Where the entry stamped `since` was installed, if known.
    fn place(&self, since: u64) -> Option<&'static Location<'static>> {
        let place = *self
            .at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (place.0 == since).then_some(place.1).flatten()
    }

    /// The watch thread's loop: parks while idle, and looks at an open entry when it
    /// reaches its next report time (`Hang`).
    fn run(&self) {
        let mut hang = Hang::default();
        loop {
            let since = self.since.load(Ordering::Acquire);
            let now = self.now();
            match hang.look(now, since) {
                Look::Report(took) => {
                    let stall = Stall {
                        took,
                        at: self.place(since),
                        done: false,
                    };
                    stall.log();
                }
                Look::Wait(left) => std::thread::park_timeout(left),
                Look::Idle => {
                    self.parked.store(true, Ordering::SeqCst);
                    // An entry that began before the flag was set did not unpark.
                    if self.since.load(Ordering::SeqCst) == 0 {
                        std::thread::park();
                    }
                    self.parked.store(false, Ordering::Release);
                }
            }
        }
    }
}

/// One stall to log.
#[derive(Debug, PartialEq, Eq)]
struct Stall {
    took: Duration,
    at: Option<&'static Location<'static>>,
    /// The entry has returned (false: it still runs).
    done: bool,
}

impl Stall {
    /// Its level in the diagnostics log (ADR-0158 decision 9): a returned entry is
    /// Debug, Warn from 1 s; one still running is Warn, Error from 4 s.
    fn level(&self) -> Level {
        match (self.done, self.took.as_secs()) {
            (true, 0) => Level::Debug,
            (true, _) | (false, 0..=3) => Level::Warn,
            (false, _) => Level::Error,
        }
    }

    fn log(&self) {
        rbs_core::log_at!(self.level(), Area::Ui, "{}", self.line());
    }

    fn line(&self) -> String {
        let place = self
            .at
            .map_or_else(|| "an unknown entry".to_owned(), |l| format!("{l}"));
        let ms = self.took.as_millis();
        if self.done {
            format!("the UI thread was busy {ms} ms in the entry installed at {place}")
        } else {
            format!(
                "the UI thread is not answering: busy {ms} ms so far in the entry installed at {place}"
            )
        }
    }
}

/// What the watch thread does next.
#[derive(Debug, PartialEq, Eq)]
enum Look {
    /// Log that the open entry has run this long.
    Report(Duration),
    /// Look again after this long.
    Wait(Duration),
    /// Nothing is open: park.
    Idle,
}

/// The watch thread's decisions, apart from clocks and threads so tests can drive them.
#[derive(Debug, Default)]
struct Hang {
    /// The open entry's stamp and the run time at which it is reported next.
    next: Option<(u64, Duration)>,
}

impl Hang {
    /// `now` and `since` as in [`Watch`]: nanoseconds + 1, `since` 0 while idle.
    fn look(&mut self, now: u64, since: u64) -> Look {
        if since == 0 {
            self.next = None;
            return Look::Idle;
        }
        let due = match self.next {
            Some((s, due)) if s == since => due,
            _ => HANG,
        };
        self.next = Some((since, due));
        let ran = Duration::from_nanos(now.saturating_sub(since));
        if ran >= due {
            self.next = Some((since, due.saturating_mul(2)));
            Look::Report(ran)
        } else {
            Look::Wait(due - ran)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn idle_parks() {
        assert_eq!(Hang::default().look(5 * MS, 0), Look::Idle);
    }

    #[test]
    fn a_short_entry_is_never_reported() {
        let mut h = Hang::default();
        assert_eq!(
            h.look(11 * MS, MS),
            Look::Wait(HANG - Duration::from_millis(10))
        );
        assert_eq!(h.look(20 * MS, 0), Look::Idle);
    }

    #[test]
    fn a_hang_is_reported_when_it_doubles() {
        let mut h = Hang::default();
        let since = 1;
        assert_eq!(h.look(1000 * MS + 1, since), Look::Report(HANG));
        // Next at 2 s, then 4 s.
        assert_eq!(
            h.look(1500 * MS + 1, since),
            Look::Wait(Duration::from_millis(500))
        );
        assert_eq!(
            h.look(2000 * MS + 1, since),
            Look::Report(Duration::from_secs(2))
        );
        assert_eq!(
            h.look(2000 * MS + 1, since),
            Look::Wait(Duration::from_secs(2))
        );
    }

    #[test]
    fn a_new_entry_starts_over() {
        let mut h = Hang::default();
        assert_eq!(h.look(1000 * MS + 1, 1), Look::Report(HANG));
        let second = 1500 * MS + 1;
        assert_eq!(
            h.look(second + 10 * MS, second),
            Look::Wait(HANG - Duration::from_millis(10))
        );
    }

    #[test]
    fn an_entry_over_the_stall_time_is_logged_with_its_place() {
        let at = Location::caller();
        let stall = finished(MS, 201 * MS, at).expect("over the stall time");
        assert_eq!(stall.took, Duration::from_millis(200));
        assert_eq!(stall.at, Some(at));
        assert!(stall.line().contains(file!()), "{}", stall.line());
        assert!(stall.line().contains("200 ms"), "{}", stall.line());
    }

    #[test]
    fn stalls_log_at_the_level_of_their_length() {
        let at = |ms, done| Stall {
            took: Duration::from_millis(ms),
            at: None,
            done,
        };
        assert_eq!(at(200, true).level(), Level::Debug);
        assert_eq!(at(1_500, true).level(), Level::Warn);
        assert_eq!(at(1_000, false).level(), Level::Warn);
        assert_eq!(at(2_000, false).level(), Level::Warn);
        assert_eq!(at(4_000, false).level(), Level::Error);
    }

    #[test]
    fn a_quick_entry_is_not_logged() {
        assert_eq!(finished(MS, 100 * MS, Location::caller()), None);
    }

    #[test]
    fn the_place_belongs_to_the_open_entry() {
        let w = Watch::new(Instant::now());
        let at = Location::caller();
        let since = w.begin(at);
        assert_eq!(w.place(since), Some(at));
        assert_eq!(w.place(since + 1), None);
    }

    #[test]
    fn only_the_ui_thread_while_its_loop_runs_is_checked() {
        std::thread::spawn(|| {
            assert!(!on_ui_thread());
            assert_off_ui("a test wait");
            UI.with(|u| u.set(true));
            assert!(on_ui_thread());
            loop_ended();
            assert!(!on_ui_thread());
            assert_off_ui("a test wait");
        })
        .join()
        .unwrap();
    }

    #[test]
    #[cfg(debug_assertions)]
    fn a_wait_on_the_ui_thread_fails_in_debug_builds() {
        let r = std::thread::spawn(|| {
            UI.with(|u| u.set(true));
            assert_off_ui("a test wait");
        })
        .join();
        assert!(r.is_err());
    }

    #[test]
    fn only_long_tasks_reach_the_hook_while_it_is_set() {
        thread_local! {
            static SEEN: Cell<u32> = const { Cell::new(0) };
        }
        fn hook(took: Duration, _: &'static Location<'static>) {
            assert!(took >= LONG_TASK);
            SEEN.with(|s| s.set(s.get() + 1));
        }
        std::thread::spawn(|| {
            let at = Location::caller();
            tell_long(LONG_TASK, at);
            set_long_task_hook(Some(hook));
            tell_long(LONG_TASK - Duration::from_nanos(1), at);
            tell_long(LONG_TASK, at);
            tell_long(Duration::from_secs(2), at);
            set_long_task_hook(None);
            tell_long(Duration::from_secs(2), at);
            assert_eq!(SEEN.with(Cell::get), 2);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn an_os_loop_is_not_the_entrys_time_but_its_entries_are_watched() {
        std::thread::spawn(|| {
            UI.with(|u| u.set(true));
            loop_started();
            let watch = WATCH.get().expect("the watch started");
            let at = Location::caller();
            let outer = enter(at);
            let since = watch.since.load(Ordering::SeqCst);
            assert_ne!(since, 0);
            let mut spent = 0;
            let mut inner_since = 0;
            os_loop(|| {
                assert_eq!(watch.since.load(Ordering::SeqCst), 0, "not timed");
                assert_eq!(DEPTH.with(Cell::get), 0);
                let from = watch.now();
                let inner = enter(Location::caller());
                inner_since = inner.since.as_ref().map_or(0, |s| s.1);
                drop(inner);
                spent = watch.now() - from;
            });
            assert!(inner_since != 0, "an entry the OS loop runs stamps");
            assert_eq!(DEPTH.with(Cell::get), 1);
            let after = watch.since.load(Ordering::SeqCst);
            assert!(after >= since + spent, "timed again without the loop");
            assert_eq!(after - since, AWAY.with(Cell::get));
            assert_eq!(watch.place(after), Some(at));
            drop(outer);
            assert_eq!(AWAY.with(Cell::get), 0, "taken when the entry ends");
            assert_eq!(watch.since.load(Ordering::SeqCst), 0);
            // With no entry open, nothing is shifted.
            os_loop(|| ());
            assert_eq!(AWAY.with(Cell::get), 0);
            loop_ended();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn nested_entries_stamp_once() {
        std::thread::spawn(|| {
            let outer = enter(Location::caller());
            assert_eq!(DEPTH.with(Cell::get), 1);
            {
                let inner = enter(Location::caller());
                assert!(inner.since.is_none());
                assert_eq!(DEPTH.with(Cell::get), 2);
            }
            drop(outer);
            assert_eq!(DEPTH.with(Cell::get), 0);
        })
        .join()
        .unwrap();
    }
}
