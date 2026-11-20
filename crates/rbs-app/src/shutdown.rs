//! Stopping the studio off the UI thread, each step within a budget (ADR-0156
//! decisions 5 and 6).
//!
//! The app hands the steps to [`start`], which runs them in order on one thread named
//! `rbs-shutdown`. That thread sends each finished step on a channel the [`Watcher`]
//! owns, then calls `wake` so the UI thread looks. Only the watcher decides: it drains
//! the channel before it compares the running step's time with its budget, so a step
//! that has finished is never counted as over budget. The UI thread never joins or
//! waits.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rbs_core::time::{Clock, Timestamp};

/// One step of the shutdown, in the order [`STEPS`] gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Remote control apps are disconnected and the control API server stops.
    RemoteControl,
    /// The studio host stops: its control thread closes the recording, ends the
    /// outputs, stops the helpers and shuts the studio down.
    Studio,
    /// The last user-data changes are written and the writer ends, releasing its lock.
    UserData,
}

impl Step {
    /// The step's name in the log and in the unfinished-shutdown note.
    pub fn name(self) -> &'static str {
        match self {
            Self::RemoteControl => "remote control",
            Self::Studio => "studio",
            Self::UserData => "user data",
        }
    }
}

/// The steps in order, each with its budget. Remote control goes first so no client
/// sees a half-stopped studio; the studio goes before the user data because its
/// control thread still queues collection saves while it stops.
pub const STEPS: [(Step, Duration); 3] = [
    (Step::RemoteControl, Duration::from_secs(2)),
    (Step::Studio, Duration::from_secs(8)),
    (Step::UserData, Duration::from_secs(2)),
];

// What the app moves to the shutdown thread is `Send` (ADR-0156 decision 5); every
// `Rc` and Slint handle stays on the UI thread.
const _: () = {
    const fn send<T: Send>() {}
    send::<crate::remote_control::RemoteServer>();
    send::<crate::studio_host::StudioHost>();
    send::<crate::user_session::Saver>();
};

/// The work of one step. It runs on the shutdown thread, so it holds only `Send`
/// values: never an `Rc` or a Slint handle.
pub type Work = Box<dyn FnOnce() + Send>;

/// What the watcher sees when it looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// A step runs.
    Running {
        /// The step.
        step: Step,
        /// What is left of its budget.
        left: Duration,
    },
    /// Every step has finished.
    Finished,
    /// `step` ran past its budget.
    OverBudget(Step),
}

/// Follows the shutdown thread's steps against their budgets. Lives on the UI thread.
pub struct Watcher {
    done: Receiver<(Step, Timestamp)>,
    clock: Arc<dyn Clock>,
    /// The index in [`STEPS`] of the step that runs now.
    current: usize,
    /// When it started.
    since: Timestamp,
}

impl Watcher {
    /// A watcher whose first step started now.
    fn new(done: Receiver<(Step, Timestamp)>, clock: Arc<dyn Clock>) -> Self {
        let since = clock.now();
        Self {
            done,
            clock,
            current: 0,
            since,
        }
    }

    /// Looks: first takes every step reported done, then checks the running step's
    /// budget.
    pub fn check(&mut self) -> State {
        loop {
            match self.done.try_recv() {
                Ok((step, at)) => {
                    // Steps finish in order; one out of order is ignored.
                    if STEPS.get(self.current).map(|(s, _)| *s) == Some(step) {
                        self.current += 1;
                        self.since = at;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        let Some(&(step, budget)) = STEPS.get(self.current) else {
            return State::Finished;
        };
        let spent = self.clock.now().saturating_since(self.since);
        if spent >= budget {
            State::OverBudget(step)
        } else {
            State::Running {
                step,
                left: budget - spent,
            }
        }
    }
}

/// The steps' work, handed back when the shutdown thread could not be started so the
/// caller can run it as before ADR-0156.
pub struct NotStarted {
    /// The work of every step, in [`STEPS`] order.
    pub work: Vec<Work>,
    /// Why the thread did not start.
    pub error: std::io::Error,
}

/// Starts the shutdown thread with `work`, one entry per [`STEPS`] entry in that order.
/// A step whose work panics counts as finished, so the next one still runs. `wake`
/// runs on the shutdown thread after each step and must not block.
pub fn start(
    work: Vec<Work>,
    clock: Arc<dyn Clock>,
    wake: impl Fn() + Send + 'static,
) -> Result<Watcher, NotStarted> {
    let (tx, rx) = mpsc::channel();
    let watcher = Watcher::new(rx, Arc::clone(&clock));
    // Shared so that a failed spawn gives the work back instead of dropping it here,
    // which would stop the studio on this thread with no deadline.
    let shared = Arc::new(Mutex::new(Some(work)));
    let spawned = std::thread::Builder::new()
        .name("rbs-shutdown".into())
        .spawn({
            let shared = Arc::clone(&shared);
            move || {
                let work = shared.lock().unwrap_or_else(PoisonError::into_inner).take();
                if let Some(work) = work {
                    run(work, &tx, clock.as_ref(), &wake);
                }
            }
        });
    match spawned {
        Ok(_) => Ok(watcher),
        Err(error) => {
            let work = shared
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .unwrap_or_default();
            Err(NotStarted { work, error })
        }
    }
}

/// Runs the steps in order and reports each one.
fn run(work: Vec<Work>, done: &Sender<(Step, Timestamp)>, clock: &dyn Clock, wake: &dyn Fn()) {
    for ((step, _), work) in STEPS.iter().zip(work) {
        let _ = rbs_crash::contain(work);
        let _ = done.send((*step, clock.now()));
        wake();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use rbs_core::time::TestClock;

    fn watcher() -> (Sender<(Step, Timestamp)>, TestClock, Watcher) {
        let (tx, rx) = mpsc::channel();
        let clock = TestClock::new();
        let w = Watcher::new(rx, Arc::new(clock.clone()));
        (tx, clock, w)
    }

    #[test]
    fn every_step_has_a_budget_in_order() {
        let order: Vec<Step> = STEPS.iter().map(|(s, _)| *s).collect();
        assert_eq!(order, [Step::RemoteControl, Step::Studio, Step::UserData]);
        assert!(STEPS.iter().all(|(_, b)| !b.is_zero()));
    }

    #[test]
    fn a_running_step_has_the_rest_of_its_budget() {
        let (_tx, clock, mut w) = watcher();
        clock.advance(Duration::from_millis(500));
        assert_eq!(
            w.check(),
            State::Running {
                step: Step::RemoteControl,
                left: Duration::from_millis(1_500)
            }
        );
    }

    #[test]
    fn a_step_that_never_finishes_is_over_budget() {
        let (_tx, clock, mut w) = watcher();
        clock.advance(STEPS[0].1);
        assert_eq!(w.check(), State::OverBudget(Step::RemoteControl));
    }

    #[test]
    fn a_step_done_before_its_budget_wins_when_looked_at_late() {
        let (tx, clock, mut w) = watcher();
        clock.advance(Duration::from_secs(1));
        tx.send((Step::RemoteControl, clock.now())).unwrap();
        // The watcher looks only after the budget is over: the report decides.
        clock.advance(Duration::from_secs(5));
        assert_eq!(
            w.check(),
            State::Running {
                step: Step::Studio,
                left: Duration::from_secs(3)
            }
        );
    }

    #[test]
    fn each_step_gets_its_own_budget() {
        let (tx, clock, mut w) = watcher();
        clock.advance(Duration::from_millis(1_900));
        tx.send((Step::RemoteControl, clock.now())).unwrap();
        clock.advance(Duration::from_millis(7_900));
        tx.send((Step::Studio, clock.now())).unwrap();
        clock.advance(Duration::from_millis(1_000));
        assert_eq!(
            w.check(),
            State::Running {
                step: Step::UserData,
                left: Duration::from_secs(1)
            }
        );
        clock.advance(Duration::from_secs(1));
        assert_eq!(w.check(), State::OverBudget(Step::UserData));
    }

    #[test]
    fn all_steps_done_is_finished() {
        let (tx, clock, mut w) = watcher();
        for (step, _) in STEPS {
            tx.send((step, clock.now())).unwrap();
        }
        clock.advance(Duration::from_secs(60));
        assert_eq!(w.check(), State::Finished);
    }

    #[test]
    fn the_thread_runs_every_step_in_order_and_reports_it() {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let work: Vec<Work> = STEPS
            .iter()
            .map(|(step, _)| {
                let ran = Arc::clone(&ran);
                let step = *step;
                Box::new(move || {
                    // The studio step panics: the next step still runs.
                    assert!(step != Step::Studio, "studio failed");
                    ran.lock().unwrap().push(step);
                }) as Work
            })
            .collect();
        let (woke_tx, woke_rx) = mpsc::channel();
        let clock = TestClock::new();
        let mut w = start(work, Arc::new(clock), move || {
            let _ = woke_tx.send(());
        })
        .ok()
        .expect("thread started");
        // One wake per step; waits for the thread's reports, bounded as a hang guard.
        for _ in STEPS {
            woke_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("a step reported");
        }
        assert_eq!(w.check(), State::Finished);
        assert_eq!(*ran.lock().unwrap(), [Step::RemoteControl, Step::UserData]);
    }
}
