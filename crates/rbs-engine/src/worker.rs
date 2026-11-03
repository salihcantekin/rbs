//! The worker thread of one partition: ticks its executor on the partition's pacing.
//!
//! Hot path (docs/architecture/hot-paths.md): the loop around `Executor::run_tick`
//! runs every frame. Per iteration it reads the stop flag and the clock, and parks or
//! waits on the doorbell; it never allocates, locks or does I/O (P6).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle, Thread};
use std::time::Duration;

use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::Cadence;
use rbs_graph::runtime::queue::Doorbell;
use rbs_graph::runtime::{Executor, Runtime, Tick};

/// How a partition's worker decides when to tick.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pacing {
    /// Tick on a cadence of the master clock: the render executor of a canvas and
    /// encoder workers, which run at frame cadence (ADR-0026, Decision 4). Tick `i`
    /// runs at `cadence.time_of(i) + offset` and carries `cadence.time_of(i)`. An
    /// encoder on the render cadence uses an offset (a quarter frame is a good start),
    /// so it reads the frame the render tick just wrote instead of the one before. A
    /// worker that is late runs the newest due tick only and counts the ones it
    /// skipped ([`WorkerStats::late`]).
    Cadence {
        /// The ticks.
        cadence: Cadence,
        /// How long after each tick's time the worker runs it.
        offset: Duration,
    },
    /// Tick when one of the partition's packet queues rings its doorbell, or after
    /// `timer` without a ring (output workers: the timer is their low-rate
    /// `poll_events` heartbeat).
    Doorbell {
        /// The longest wait between ticks.
        timer: Duration,
    },
}

/// The default heartbeat of a doorbell-paced worker.
pub const DEFAULT_DOORBELL_TIMER: Duration = Duration::from_millis(100);

impl Pacing {
    /// Ticks on `cadence`, at each tick's time.
    pub const fn cadence(cadence: Cadence) -> Self {
        Pacing::Cadence {
            cadence,
            offset: Duration::ZERO,
        }
    }

    /// Ticks on `cadence`, `offset` after each tick's time (encoders after render).
    pub const fn cadence_after(cadence: Cadence, offset: Duration) -> Self {
        Pacing::Cadence { cadence, offset }
    }

    /// Ticks on doorbell rings, or after `timer`.
    pub const fn doorbell(timer: Duration) -> Self {
        Pacing::Doorbell { timer }
    }

    /// Ticks on doorbell rings, or after [`DEFAULT_DOORBELL_TIMER`].
    pub const fn output() -> Self {
        Pacing::Doorbell {
            timer: DEFAULT_DOORBELL_TIMER,
        }
    }
}

/// Counters of a partition's worker, shared with the control side. Relaxed atomics;
/// they survive the worker being stopped and started again.
#[derive(Debug, Default)]
pub struct WorkerStats {
    ticks: AtomicU64,
    late: AtomicU64,
    starts: AtomicU64,
}

impl WorkerStats {
    /// Ticks run.
    pub fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }

    /// Cadence ticks skipped because the worker was late (a dropped frame each).
    pub fn late(&self) -> u64 {
        self.late.load(Ordering::Relaxed)
    }

    /// How many times a worker thread was started for the partition.
    pub fn starts(&self) -> u64 {
        self.starts.load(Ordering::Relaxed)
    }
}

/// A running worker thread that owns its partition's executor.
pub(crate) struct Worker<X: Runtime> {
    thread: Thread,
    join: JoinHandle<Option<(Executor<X>, u64)>>,
    stop: Arc<AtomicBool>,
    bell: Option<Arc<Doorbell>>,
}

/// What a worker needs besides its executor.
pub(crate) struct WorkerSpec {
    pub(crate) name: String,
    pub(crate) pacing: Pacing,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) bell: Option<Arc<Doorbell>>,
    pub(crate) stats: Arc<WorkerStats>,
    /// The first tick index the worker may use (ticks never repeat an index).
    pub(crate) next: u64,
}

impl<X: Runtime> Worker<X> {
    /// Starts a thread that ticks the executor taken from `parked`. On failure the
    /// executor is put back.
    pub(crate) fn spawn(
        parked: &mut Option<Executor<X>>,
        spec: WorkerSpec,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        // The executor passes through a slot so that it is not lost when the thread
        // cannot be started (`Builder::spawn` drops its closure on failure). Locked
        // once on each side, at start-up only.
        let slot = Arc::new(Mutex::new(parked.take()));
        let thread_slot = Arc::clone(&slot);
        let thread_stop = Arc::clone(&stop);
        let WorkerSpec {
            name,
            pacing,
            clock,
            bell,
            stats,
            next,
        } = spec;
        let thread_bell = bell.clone();
        let spawned = thread::Builder::new().name(name).spawn(move || {
            let executor = thread_slot
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            // The slot is filled before the thread starts and only emptied here or
            // after a failed spawn, so it always holds the executor.
            executor.map(|executor| {
                run(
                    executor,
                    pacing,
                    &*clock,
                    thread_bell.as_deref(),
                    &thread_stop,
                    &stats,
                    next,
                )
            })
        });
        match spawned {
            Ok(join) => Ok(Self {
                thread: join.thread().clone(),
                join,
                stop,
                bell,
            }),
            // The thread did not start, so the slot still holds the executor.
            Err(e) => {
                *parked = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
                Err(e)
            }
        }
    }

    /// Makes the worker act soon: rings a doorbell worker (it ticks), unparks a
    /// cadence worker (it swaps a pending schedule, and ticks only if a tick is due).
    pub(crate) fn wake(&self) {
        match &self.bell {
            Some(bell) => bell.ring(),
            None => self.thread.unpark(),
        }
    }

    /// Stops the thread after its current tick and returns the executor with the next
    /// free tick index. `None` if the thread panicked outside the executor's own
    /// catch (the executor is then lost).
    pub(crate) fn stop(self) -> Option<(Executor<X>, u64)> {
        self.stop.store(true, Ordering::Release);
        if let Some(bell) = &self.bell {
            bell.ring();
        }
        self.thread.unpark();
        self.join.join().ok().flatten()
    }
}

/// The worker loop. Returns the executor and the next free tick index.
fn run<X: Runtime>(
    mut executor: Executor<X>,
    pacing: Pacing,
    clock: &dyn Clock,
    bell: Option<&Doorbell>,
    stop: &AtomicBool,
    stats: &WorkerStats,
    mut next: u64,
) -> (Executor<X>, u64) {
    stats.starts.fetch_add(1, Ordering::Relaxed);
    match (pacing, bell) {
        (Pacing::Cadence { cadence, offset }, _) => {
            // The cadence time a tick that is due at `t` carries.
            let index_due = |t: Timestamp| cadence.index_at_or_before(t - offset);
            // Start at the current tick, or later if an earlier worker got further.
            if let Some(now_index) = index_due(clock.now()) {
                next = next.max(now_index);
            }
            while !stop.load(Ordering::Acquire) {
                let due = cadence.time_of(next) + offset;
                let now = clock.now();
                if now < due {
                    thread::park_timeout(due.saturating_since(now));
                    // Woken by the control thread after an `apply`, or a spurious
                    // wake-up: swap a pending schedule now (a cheap `try_recv` when
                    // there is none), then re-check the time.
                    executor.swap_pending();
                    continue;
                }
                // Late: run the newest due tick only, so timestamps stay on cadence.
                let newest = index_due(now).unwrap_or(next).max(next);
                if newest > next {
                    stats.late.fetch_add(newest - next, Ordering::Relaxed);
                }
                executor.run_tick(Tick::of(&cadence, newest));
                stats.ticks.fetch_add(1, Ordering::Relaxed);
                next = newest.saturating_add(1);
            }
        }
        (Pacing::Doorbell { timer }, Some(bell)) => {
            while !stop.load(Ordering::Acquire) {
                bell.wait(timer);
                if stop.load(Ordering::Acquire) {
                    break;
                }
                executor.run_tick(Tick::new(next, clock.now()));
                stats.ticks.fetch_add(1, Ordering::Relaxed);
                next = next.saturating_add(1);
            }
        }
        (Pacing::Doorbell { timer }, None) => {
            // Not reachable through the engine, which always gives doorbell-paced
            // partitions their doorbell; behaves as a plain timer.
            while !stop.load(Ordering::Acquire) {
                thread::park_timeout(timer);
                if stop.load(Ordering::Acquire) {
                    break;
                }
                executor.run_tick(Tick::new(next, clock.now()));
                stats.ticks.fetch_add(1, Ordering::Relaxed);
                next = next.saturating_add(1);
            }
        }
    }
    (executor, next)
}
