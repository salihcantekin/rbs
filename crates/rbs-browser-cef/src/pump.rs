//! CEF's external message pump (stability plan WP-J, Faz 3.4).
//!
//! The runner's loop used to call `do_message_loop_work` and then sleep half a page
//! frame, at most 10 ms: about 100 wake-ups a second for a page that does nothing, and
//! up to one sleep of delay before an input event or a paint task ran. With
//! `external_message_pump` on, CEF says when it has work through
//! `on_schedule_message_pump_work(delay_ms)`, from any thread. The loop now waits on
//! [`PUMP`] until that work is due, a control message or the stop arrives, or its own
//! next timer (heartbeat, page cycle, file check, repaint) comes.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The longest the loop leaves CEF without a run: a longer request is run early, and
/// after each run with no request waiting the next one is due this late (see
/// [`Pump::worked`]). CEF's sample uses 1000/30 ms (`kMaxTimerDelay`); measured on
/// Linux (WP-J), one run in this mode costs about 0.3 ms of CPU, so 33 ms made an idle
/// page cost more than the old 10 ms sleep loop (1.2 % against 0.8 % of a core) while
/// 100 ms costs less (0.6 %), with the same input-to-frame time.
pub const MAX_DELAY: Duration = Duration::from_millis(100);

/// The pump of this process; it has one browser.
pub static PUMP: Pump = Pump::new();

/// When CEF work is due and whether the loop was woken for something else.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct State {
    /// The earliest time CEF asked to run (one request replaces a later one).
    due: Option<Instant>,
    /// A control message or the stop arrived.
    woken: bool,
}

/// The loop's wait point between CEF calls.
#[derive(Debug)]
pub struct Pump {
    state: Mutex<State>,
    cond: Condvar,
}

/// Why [`Pump::wait`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Woke {
    /// CEF work is due: call `do_message_loop_work` now.
    pub work: bool,
}

impl Pump {
    /// A pump with nothing due. Before the first request one run is due at once, so
    /// work CEF queued while it started is not left waiting.
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(State {
                due: None,
                woken: true,
            }),
            cond: Condvar::new(),
        }
    }

    /// CEF's request: run in `delay_ms` (0 or less: now). Any thread; it never waits
    /// on more than the short state lock.
    pub fn schedule(&self, delay_ms: i64) {
        let now = Instant::now();
        let mut state = self.lock();
        state.due = Some(merge(state.due, now, delay_ms));
        drop(state);
        self.cond.notify_one();
    }

    /// Called after each `do_message_loop_work`: with no request waiting, the next run
    /// is due in [`MAX_DELAY`]. CEF's own external pump does the same (`DoWork` in
    /// cefclient's `main_message_loop_external_pump.cc`): not every piece of work is
    /// announced through a request, and without this fallback a page could wait for
    /// its first paint forever (seen in the WP-J measurement on Linux).
    pub fn worked(&self) {
        let now = Instant::now();
        let mut state = self.lock();
        if state.due.is_none() {
            state.due = Some(now + MAX_DELAY);
        }
    }

    /// Wakes the loop for something other than CEF work: a control message, the stop.
    pub fn wake(&self) {
        self.lock().woken = true;
        self.cond.notify_one();
    }

    /// Waits until CEF work is due, [`wake`](Self::wake) is called, or `until`. The CEF
    /// request is consumed when it is reported.
    pub fn wait(&self, until: Instant) -> Woke {
        let mut state = self.lock();
        loop {
            let now = Instant::now();
            if let Some(woke) = take_ready(&mut state, now, until) {
                return woke;
            }
            let end = state.due.map_or(until, |due| due.min(until));
            let (next, _) = self
                .cond
                .wait_timeout(state, end.saturating_duration_since(now))
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for Pump {
    fn default() -> Self {
        Self::new()
    }
}

/// The due time after a request of `delay_ms` at `now`: the earlier of the waiting one
/// and the new one, which is clamped to [`MAX_DELAY`]. CEF documents that a request
/// replaces the earlier one; keeping the earlier of the two only runs work sooner, and
/// a run with nothing to do is cheap.
fn merge(due: Option<Instant>, now: Instant, delay_ms: i64) -> Instant {
    let delay = Duration::from_millis(u64::try_from(delay_ms).unwrap_or(0)).min(MAX_DELAY);
    let at = now + delay;
    due.map_or(at, |due| due.min(at))
}

/// The answer of a wait at `now` if it is over: CEF work due, a wake, or `until`
/// passed. A due request is consumed.
fn take_ready(state: &mut State, now: Instant, until: Instant) -> Option<Woke> {
    let work = state.due.is_some_and(|due| due <= now);
    if work {
        state.due = None;
    }
    let woken = std::mem::take(&mut state.woken);
    (work || woken || now >= until).then_some(Woke { work })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_request_is_clamped_and_the_earlier_one_kept() {
        let t = Instant::now();
        assert_eq!(merge(None, t, 0), t);
        assert_eq!(merge(None, t, -5), t);
        assert_eq!(merge(None, t, 10), t + 10 * MS);
        assert_eq!(merge(None, t, 10_000), t + MAX_DELAY);
        assert_eq!(merge(Some(t + 5 * MS), t, 10), t + 5 * MS);
        assert_eq!(merge(Some(t + 20 * MS), t, 10), t + 10 * MS);
    }

    #[test]
    fn due_work_is_reported_once() {
        let t = Instant::now();
        let mut state = State {
            due: Some(t),
            woken: false,
        };
        let later = t + Duration::from_secs(1);
        assert_eq!(take_ready(&mut state, t, later), Some(Woke { work: true }));
        assert_eq!(take_ready(&mut state, t, later), None);
    }

    #[test]
    fn work_not_yet_due_waits_and_a_wake_returns_without_it() {
        let t = Instant::now();
        let later = t + Duration::from_secs(1);
        let mut state = State {
            due: Some(t + 10 * MS),
            woken: false,
        };
        assert_eq!(take_ready(&mut state, t, later), None);
        state.woken = true;
        assert_eq!(take_ready(&mut state, t, later), Some(Woke { work: false }));
        assert_eq!(state.due, Some(t + 10 * MS), "the request stays for later");
    }

    #[test]
    fn after_work_a_run_is_due_within_the_longest_delay() {
        let pump = Pump::new();
        let before = Instant::now();
        pump.worked();
        let due = pump.lock().due;
        assert!(due.is_some_and(|d| d >= before + MAX_DELAY && d <= Instant::now() + MAX_DELAY));
        // A sooner request is kept.
        pump.schedule(0);
        let soon = pump.lock().due;
        pump.worked();
        assert_eq!(pump.lock().due, soon);
    }

    #[test]
    fn the_loop_timer_ends_the_wait() {
        let t = Instant::now();
        let mut state = State::default();
        assert_eq!(take_ready(&mut state, t, t), Some(Woke { work: false }));
    }

    #[test]
    fn the_first_wait_runs_cef_at_once() {
        let pump = Pump::new();
        // Returns at once (woken at start); the hang guard is the far `until`.
        let woke = pump.wait(Instant::now() + Duration::from_secs(10));
        assert_eq!(woke, Woke { work: false });
    }

    #[test]
    fn a_request_from_another_thread_ends_the_wait() {
        let pump = std::sync::Arc::new(Pump::new());
        let _ = pump.wait(Instant::now());
        let other = std::sync::Arc::clone(&pump);
        let t = std::thread::spawn(move || other.schedule(0));
        // Ends with the request; 10 s only bounds a hang.
        let woke = pump.wait(Instant::now() + Duration::from_secs(10));
        assert!(t.join().is_ok());
        assert_eq!(woke, Woke { work: true });
    }

    #[test]
    fn a_wake_from_another_thread_ends_the_wait() {
        let pump = std::sync::Arc::new(Pump::new());
        let _ = pump.wait(Instant::now());
        let other = std::sync::Arc::clone(&pump);
        let t = std::thread::spawn(move || other.wake());
        let woke = pump.wait(Instant::now() + Duration::from_secs(10));
        assert!(t.join().is_ok());
        assert_eq!(woke, Woke { work: false });
    }
}
