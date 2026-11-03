//! The worker thread of one partition: ticks its executor on the partition's pacing.
//!
//! Hot path (docs/architecture/hot-paths.md): the loop around `Executor::run_tick`
//! runs every frame. Per iteration it reads the worker's mode and the clock, and parks or
//! waits on the doorbell; it never allocates or does I/O (P6). A cadence worker takes
//! no lock. A doorbell worker takes the doorbell's registration lock once per wait;
//! the only other party is a ringer's `try_lock`, which never waits, so the worker can
//! wait at most for one `unpark` call. Doorbell workers run outputs, not render.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
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

/// Most ticks a doorbell worker runs to drain: the first, plus one per ring left by a
/// reader with packets still queued. 4096 ticks pass 64 Ki packets at
/// `PER_TICK_EVENTS` = 16, far above a default queue (256 packets).
pub(crate) const MAX_DRAIN_TICKS: u32 = 4096;

/// The worker's mode, one atomic read per loop iteration.
pub(crate) mod mode {
    /// Ticks on its pacing.
    pub(crate) const RUN: u8 = 0;
    /// Asked to run its final tick(s) now.
    pub(crate) const DRAIN: u8 = 1;
    /// Drained: swaps pending schedules on wake-up but runs no tick.
    pub(crate) const HELD: u8 = 2;
    /// Exits after its current tick.
    pub(crate) const STOP: u8 = 3;
}

/// What the control side shares with a running worker.
pub(crate) struct Signals {
    pub(crate) mode: AtomicU8,
    /// The worker sends one message after its drain ticks.
    pub(crate) drained: SyncSender<()>,
}

impl Signals {
    pub(crate) fn new() -> (Self, Receiver<()>) {
        let (drained, rx) = sync_channel(1);
        (
            Self {
                mode: AtomicU8::new(mode::RUN),
                drained,
            },
            rx,
        )
    }

    fn get(&self) -> u8 {
        self.mode.load(Ordering::Acquire)
    }

    /// After the drain ticks: hold, unless the control side asked to stop meanwhile.
    fn drained(&self) {
        let _ = self.mode.compare_exchange(
            mode::DRAIN,
            mode::HELD,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let _ = self.drained.try_send(());
    }
}

/// A running worker thread that owns its partition's executor.
pub(crate) struct Worker<X: Runtime> {
    thread: Thread,
    join: JoinHandle<Option<(Executor<X>, u64)>>,
    signals: Arc<Signals>,
    drained: Receiver<()>,
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
        let (signals, drained) = Signals::new();
        let signals = Arc::new(signals);
        // The executor passes through a slot so that it is not lost when the thread
        // cannot be started (`Builder::spawn` drops its closure on failure). Locked
        // once on each side, at start-up only.
        let slot = Arc::new(Mutex::new(parked.take()));
        let thread_slot = Arc::clone(&slot);
        let thread_signals = Arc::clone(&signals);
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
                    &thread_signals,
                    &stats,
                    next,
                )
            })
        });
        match spawned {
            Ok(join) => Ok(Self {
                thread: join.thread().clone(),
                join,
                signals,
                drained,
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

    /// Asks the worker for its final tick(s) and waits up to `timeout` for them. A
    /// cadence worker runs one tick at once, without waiting for its time; a doorbell
    /// worker runs one, then one more for each ring a reader left because packets were
    /// still queued (at most [`MAX_DRAIN_TICKS`]). Afterwards the worker runs no tick
    /// but still swaps schedules, so the empty schedule is swapped and cooled on the
    /// worker's own thread. True once the worker drained.
    pub(crate) fn drain(&self, timeout: Duration) -> bool {
        let asked = self.signals.mode.compare_exchange(
            mode::RUN,
            mode::DRAIN,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        match asked {
            Ok(_) => {}
            Err(mode::HELD) => return true,
            // STOP: unreachable today (a worker is stopped by `stop`, which consumes
            // it); reported as not drained.
            Err(_) => return false,
        }
        if self.join.is_finished() {
            // The thread died outside the executor's catch; nothing will answer.
            return false;
        }
        self.wake();
        self.drained.recv_timeout(timeout).is_ok()
    }

    /// Stops the thread after its current tick and returns the executor with the next
    /// free tick index. `None` if the thread panicked outside the executor's own
    /// catch (the executor is then lost).
    pub(crate) fn stop(self) -> Option<(Executor<X>, u64)> {
        self.signals.mode.store(mode::STOP, Ordering::Release);
        if let Some(bell) = &self.bell {
            bell.ring();
        }
        self.thread.unpark();
        self.join.join().ok().flatten()
    }
}

/// The worker loop. Returns the executor and the next free tick index. Exits when
/// `signals` is set to [`mode::STOP`].
pub(crate) fn run<X: Runtime>(
    mut executor: Executor<X>,
    pacing: Pacing,
    clock: &dyn Clock,
    bell: Option<&Doorbell>,
    signals: &Signals,
    stats: &WorkerStats,
    mut next: u64,
) -> (Executor<X>, u64) {
    stats.starts.fetch_add(1, Ordering::Relaxed);
    match pacing {
        Pacing::Cadence { cadence, offset } => {
            // The cadence time a tick that is due at `t` carries.
            let index_due = |t: Timestamp| cadence.index_at_or_before(t - offset);
            // Start at the current tick, or later if an earlier worker got further.
            if let Some(now_index) = index_due(clock.now()) {
                next = next.max(now_index);
            }
            loop {
                match signals.get() {
                    mode::RUN => {}
                    mode::DRAIN => {
                        // The final tick, at once: the next index, which may be up to
                        // one cadence period ahead of its time (or the newest due one).
                        let newest = index_due(clock.now()).unwrap_or(next).max(next);
                        if newest > next {
                            stats.late.fetch_add(newest - next, Ordering::Relaxed);
                        }
                        executor.run_tick(Tick::of(&cadence, newest));
                        stats.ticks.fetch_add(1, Ordering::Relaxed);
                        next = newest.saturating_add(1);
                        signals.drained();
                        continue;
                    }
                    mode::HELD => {
                        thread::park();
                        executor.swap_pending();
                        continue;
                    }
                    _ => break,
                }
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
        Pacing::Doorbell { timer } => {
            // The engine always gives doorbell-paced partitions their doorbell. Without
            // one, a doorbell nobody rings makes this a plain timer (`wake` unparks).
            let own;
            let bell = match bell {
                Some(bell) => bell,
                None => {
                    own = Doorbell::new();
                    &own
                }
            };
            let mut tick = |executor: &mut Executor<X>| {
                executor.run_tick(Tick::new(next, clock.now()));
                stats.ticks.fetch_add(1, Ordering::Relaxed);
                next = next.saturating_add(1);
            };
            loop {
                match signals.get() {
                    mode::RUN => {
                        bell.wait(timer);
                        // Asked to drain or stop while waiting: handled above.
                        if signals.get() == mode::RUN {
                            tick(&mut executor);
                        }
                    }
                    mode::DRAIN => {
                        // Readers with packets left ring the doorbell again.
                        tick(&mut executor);
                        let mut ticks = 1;
                        while ticks < MAX_DRAIN_TICKS
                            && bell.wait(Duration::ZERO)
                            && signals.get() == mode::DRAIN
                        {
                            tick(&mut executor);
                            ticks += 1;
                        }
                        signals.drained();
                    }
                    mode::HELD => {
                        bell.wait(timer);
                        executor.swap_pending();
                    }
                    _ => break,
                }
            }
        }
    }
    (executor, next)
}

#[cfg(test)]
mod tests {
    //! Allocation guard for the worker loop: `run` runs on the test thread (the
    //! counter is per thread) and a node stops it after a number of ticks.

    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

    use std::sync::atomic::AtomicU64;

    use rbs_core::lifecycle::Lifecycle;
    use rbs_core::time::{MonotonicClock, TestClock};
    use rbs_core::timing::{FrameRate, Rational};
    use rbs_graph::description::GraphDescription;
    use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
    use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, channel};

    use super::*;

    const TICKS: u64 = 1_000;
    const FRAME: Duration = Duration::from_nanos(16_666_667);

    /// Counts ticks and drives the loop: advances the test clock one frame (cadence),
    /// rings the doorbell on every other tick (doorbell), and stops the worker after
    /// `stop_after` ticks.
    struct Driver {
        desc: NodeDescriptor,
        ticks: Arc<AtomicU64>,
        indices_ok: Arc<AtomicU64>,
        last: Option<u64>,
        stop_after: u64,
        /// Asks for the drain at this tick instead.
        drain_at: Option<u64>,
        signals: Arc<Signals>,
        clock: Option<TestClock>,
        bell: Option<Arc<Doorbell>>,
    }

    impl Node<()> for Driver {
        fn descriptor(&self) -> &NodeDescriptor {
            &self.desc
        }

        fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
            Ok(())
        }

        fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
            let n = self.ticks.fetch_add(1, Ordering::Relaxed) + 1;
            let index = ctx.tick().index;
            if self.last.is_none_or(|last| index > last) {
                self.indices_ok.fetch_add(1, Ordering::Relaxed);
            }
            self.last = Some(index);
            if let Some(clock) = &self.clock {
                clock.advance(FRAME);
            }
            if let Some(bell) = &self.bell {
                if n.is_multiple_of(2) {
                    bell.ring();
                }
            }
            if self.drain_at == Some(n) {
                let _ = self.signals.mode.compare_exchange(
                    mode::RUN,
                    mode::DRAIN,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            if n >= self.stop_after {
                self.signals.mode.store(mode::STOP, Ordering::Release);
            }
            Ok(())
        }

        fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    }

    /// A source that writes nothing.
    struct Quiet(NodeDescriptor);

    impl Node<()> for Quiet {
        fn descriptor(&self) -> &NodeDescriptor {
            &self.0
        }

        fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
            Ok(())
        }

        fn process(&mut self, _: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
            Ok(())
        }

        fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    }

    struct Run {
        ticks: Arc<AtomicU64>,
        indices_ok: Arc<AtomicU64>,
        allocations: u64,
        stats: WorkerStats,
    }

    /// How a measured run ends.
    #[derive(Clone, Copy)]
    enum End {
        /// The driver stops the loop at this tick.
        StopAfter(u64),
        /// The driver asks for the drain at this tick; another thread stops the worker
        /// once it holds, so the final ticks and the held wait are measured too.
        DrainAt(u64),
    }

    /// Runs `run` on this thread under the allocation counter. `advance`: the driver
    /// moves this test clock one frame per tick.
    fn run_on_this_thread(
        pacing: Pacing,
        clock: Arc<dyn Clock>,
        advance: Option<TestClock>,
        bell: Option<Arc<Doorbell>>,
        end: End,
    ) -> Run {
        let (signals, _drained) = Signals::new();
        let signals = Arc::new(signals);
        let ticks = Arc::new(AtomicU64::new(0));
        let indices_ok = Arc::new(AtomicU64::new(0));
        let desc = NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.driver"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        );
        let driver = Driver {
            desc: desc.clone(),
            ticks: Arc::clone(&ticks),
            indices_ok: Arc::clone(&indices_ok),
            last: None,
            stop_after: match end {
                End::StopAfter(n) => n,
                End::DrainAt(_) => u64::MAX,
            },
            drain_at: match end {
                End::StopAfter(_) => None,
                End::DrainAt(n) => Some(n),
            },
            signals: Arc::clone(&signals),
            clock: advance,
            bell: bell.clone(),
        };
        let (mut controller, mut executor) = channel::<()>(Arc::new(()), ());
        let mut graph = GraphDescription::new();
        let source = Quiet(NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.quiet"),
            NodeKind::Source,
            vec![],
            vec![Pin::one("out", LinkKind::Video)],
        ));
        let ids = [
            graph.add_node(Arc::new(source.0.clone())).unwrap(),
            graph.add_node(Arc::new(desc)).unwrap(),
        ];
        for id in ids {
            graph.set_state(id, Lifecycle::Standby).unwrap();
            graph.set_state(id, Lifecycle::Active).unwrap();
        }
        graph.connect(ids[0], "out", ids[1], "in").unwrap();
        controller.adopt(ids[0], Box::new(source)).unwrap();
        controller.adopt(ids[1], Box::new(driver)).unwrap();
        controller.apply(&graph).unwrap();
        // The swap happens before the measurement, as on a worker that already runs.
        executor.swap_pending();
        controller.collect().unwrap().unwrap();
        assert_eq!(executor.scheduled().count(), 2);
        // Registers this thread's handle once (as a worker's first wait does).
        let _ = std::thread::current();
        let stats = WorkerStats::default();
        // Stops a held worker (its allocations are not counted: another thread).
        let stopper = matches!(end, End::DrainAt(_)).then(|| {
            let (signals, bell, worker) =
                (Arc::clone(&signals), bell.clone(), std::thread::current());
            std::thread::spawn(move || {
                while signals.get() != mode::HELD {
                    std::thread::yield_now();
                }
                signals.mode.store(mode::STOP, Ordering::Release);
                if let Some(bell) = bell {
                    bell.ring();
                }
                worker.unpark();
            })
        });
        let mut out = None;
        let info = allocation_counter::measure(|| {
            out = Some(run(
                executor,
                pacing,
                &*clock,
                bell.as_deref(),
                &signals,
                &stats,
                0,
            ));
        });
        if let Some(stopper) = stopper {
            stopper.join().unwrap();
        }
        // Dropping the executor (and its nodes) happens outside the measurement.
        drop(out);
        Run {
            ticks,
            indices_ok,
            allocations: info.count_total,
            stats,
        }
    }

    #[test]
    fn a_cadence_worker_loop_does_not_allocate() {
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
        // Each tick moves the clock to the next one's time, so no tick waits or is late.
        let clock = TestClock::new();
        clock.advance(FRAME / 4);
        let run = run_on_this_thread(
            Pacing::cadence_after(cadence, FRAME / 4),
            Arc::new(clock.clone()),
            Some(clock),
            None,
            End::StopAfter(TICKS),
        );
        assert_eq!(run.allocations, 0, "the cadence worker loop allocated");
        assert_eq!(run.ticks.load(Ordering::Relaxed), TICKS);
        assert_eq!(run.indices_ok.load(Ordering::Relaxed), TICKS);
        assert_eq!(run.stats.ticks(), TICKS);
        assert_eq!(run.stats.late(), 0);
    }

    #[test]
    fn a_doorbell_worker_loop_does_not_allocate() {
        // Every other tick follows a ring; the others follow the (short) timer, which
        // takes the doorbell's registration lock and parks.
        let bell = Arc::new(Doorbell::new());
        let run = run_on_this_thread(
            Pacing::doorbell(Duration::from_micros(50)),
            Arc::new(TestClock::new()),
            None,
            Some(bell),
            End::StopAfter(TICKS),
        );
        assert_eq!(run.allocations, 0, "the doorbell worker loop allocated");
        assert_eq!(run.ticks.load(Ordering::Relaxed), TICKS);
        assert_eq!(run.indices_ok.load(Ordering::Relaxed), TICKS);
        assert_eq!(run.stats.ticks(), TICKS);
    }

    #[test]
    fn a_cadence_worker_that_parks_between_ticks_does_not_allocate() {
        // The production path: a real clock and a 10 kHz cadence, so the worker parks
        // (`park_timeout` and `swap_pending`) before each tick.
        let clock = MonotonicClock::new();
        let rate = Rational::new(10_000, 1).unwrap();
        let cadence = Cadence::new(clock.now(), rate);
        let run = run_on_this_thread(
            Pacing::cadence(cadence),
            Arc::new(clock),
            None,
            None,
            End::StopAfter(200),
        );
        assert_eq!(
            run.allocations, 0,
            "the parking cadence worker loop allocated"
        );
        assert_eq!(run.ticks.load(Ordering::Relaxed), 200);
        assert_eq!(run.indices_ok.load(Ordering::Relaxed), 200);
    }

    #[test]
    fn draining_and_holding_does_not_allocate() {
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
        let clock = TestClock::new();
        let run = run_on_this_thread(
            Pacing::cadence(cadence),
            Arc::new(clock.clone()),
            Some(clock),
            None,
            End::DrainAt(100),
        );
        assert_eq!(run.allocations, 0, "a cadence drain allocated");
        // One final tick after the one that asked for the drain.
        assert_eq!(run.ticks.load(Ordering::Relaxed), 101);
        assert_eq!(run.indices_ok.load(Ordering::Relaxed), 101);

        let bell = Arc::new(Doorbell::new());
        let run = run_on_this_thread(
            Pacing::doorbell(Duration::from_micros(50)),
            Arc::new(TestClock::new()),
            None,
            Some(bell),
            End::DrainAt(100),
        );
        assert_eq!(run.allocations, 0, "a doorbell drain allocated");
        // The final tick, plus one per ring the driver left (it rings on even ticks).
        let ticks = run.ticks.load(Ordering::Relaxed);
        assert!((101..=103).contains(&ticks), "{ticks}");
        assert_eq!(run.indices_ok.load(Ordering::Relaxed), ticks);
    }
}
