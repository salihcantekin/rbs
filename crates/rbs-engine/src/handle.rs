//! A non-blocking handle for UI threads: the engine runs on its own control thread,
//! started on the first command, and reports what happens through a callback.

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use rbs_graph::description::{GraphDescription, PartitionId};
use rbs_graph::runtime::{ControlWake, RuntimeEvent};

use crate::engine::{Applied, Engine, EngineError, Shutdown};

/// What the engine's control thread reports to an [`EngineHandle`]'s callback.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// A description sent with [`EngineHandle::apply`] was applied.
    Applied(Applied),
    /// A description could not be applied at all.
    ApplyFailed(EngineError),
    /// A failure, restart or recovery noticed by the control loop.
    Runtime(PartitionId, RuntimeEvent),
    /// An error noticed by the control loop.
    Error(EngineError),
    /// The engine was drained and stopped; the control thread ends.
    ShutDown(Shutdown),
}

/// The callback that receives [`EngineEvent`]s, on the control thread. A UI forwards
/// them to its event loop (for example with `slint::invoke_from_event_loop`); it must
/// not block or park: it shares this thread's park token (ADR-0154).
pub type EventSink = Box<dyn FnMut(EngineEvent) + Send>;

type Job = Box<dyn FnOnce(&mut Engine) + Send>;

enum Command {
    Apply(GraphDescription),
    Run(Job),
    Shutdown,
}

enum State {
    /// No command yet: no thread (ADR-0013).
    Idle(Box<Engine>, EventSink),
    Running {
        /// `None` once the shutdown was queued.
        commands: Option<Sender<Command>>,
        thread: JoinHandle<()>,
    },
    Done,
}

/// Owns an [`Engine`] on a control thread of its own. Every call returns at once: it
/// only queues a command. The thread starts with the first command, so an engine that
/// is never used costs no thread. Once started it stays until shutdown; while nothing
/// runs it parks until a command or a bypass (no timer, no CPU), an accepted cost of
/// one parked thread. The [`EventSink`] must not panic or park: a panic there ends the
/// control thread, and later calls return [`EngineError::ShutDown`].
pub struct EngineHandle {
    state: Mutex<State>,
    wake: Arc<ControlWake>,
}

impl fmt::Debug for EngineHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match &*self.state.lock().unwrap_or_else(PoisonError::into_inner) {
            State::Idle(..) => "idle",
            State::Running { .. } => "running",
            State::Done => "done",
        };
        f.debug_struct("EngineHandle")
            .field("state", &state)
            .finish()
    }
}

impl EngineHandle {
    /// Wraps `engine`; `events` receives everything the control thread reports.
    pub fn new(engine: Engine, events: EventSink) -> Self {
        let wake = engine.control_wake();
        Self {
            state: Mutex::new(State::Idle(Box::new(engine), events)),
            wake,
        }
    }

    /// Queues `logical` to be applied; the result arrives as [`EngineEvent::Applied`]
    /// or [`EngineEvent::ApplyFailed`].
    pub fn apply(&self, logical: GraphDescription) -> Result<(), EngineError> {
        self.send(Command::Apply(logical))
    }

    /// Queues `job` to run with the engine on the control thread (adding partitions,
    /// adopting node instances, retrying a node).
    pub fn run(&self, job: impl FnOnce(&mut Engine) + Send + 'static) -> Result<(), EngineError> {
        self.send(Command::Run(Box::new(job)))
    }

    /// Queues the shutdown; [`EngineEvent::ShutDown`] reports it.
    pub fn shutdown(&self) -> Result<(), EngineError> {
        self.send(Command::Shutdown)
    }

    /// True once the control thread was started.
    pub fn is_started(&self) -> bool {
        matches!(
            &*self.state.lock().unwrap_or_else(PoisonError::into_inner),
            State::Running { .. }
        )
    }

    fn send(&self, command: Command) -> Result<(), EngineError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(&*state, State::Idle(..)) {
            let State::Idle(engine, sink) = std::mem::replace(&mut *state, State::Done) else {
                return Err(EngineError::ShutDown);
            };
            let (tx, rx) = mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("rbs-control".to_owned())
                .spawn(move || control_loop(*engine, sink, rx));
            match spawned {
                Ok(thread) => {
                    *state = State::Running {
                        commands: Some(tx),
                        thread,
                    };
                }
                // The engine moved into the closure that failed to start and is
                // dropped (and so shut down) with it.
                Err(e) => return Err(EngineError::ControlThread(e.kind())),
            }
        }
        let State::Running { commands, .. } = &mut *state else {
            return Err(EngineError::ShutDown);
        };
        let last = matches!(command, Command::Shutdown);
        let sent = match commands.as_ref() {
            Some(tx) => tx.send(command).map_err(|_| EngineError::ShutDown),
            None => Err(EngineError::ShutDown),
        };
        if last {
            // Later calls fail at once instead of queueing behind the shutdown.
            *commands = None;
        }
        drop(state);
        if sent.is_ok() {
            self.wake.unpark();
        }
        sent
    }
}

impl Drop for EngineHandle {
    /// Shuts the engine down and waits for the control thread. Call
    /// [`EngineHandle::shutdown`] earlier to avoid waiting here.
    fn drop(&mut self) {
        let state = std::mem::replace(
            &mut *self.state.lock().unwrap_or_else(PoisonError::into_inner),
            State::Done,
        );
        if let State::Running { commands, thread } = state {
            if let Some(commands) = commands {
                let _ = commands.send(Command::Shutdown);
            }
            self.wake.unpark();
            let _ = thread.join();
        }
    }
}

fn control_loop(mut engine: Engine, mut sink: EventSink, commands: Receiver<Command>) {
    engine.bind_control_thread();
    // Service before the first park, so a bypass during startup is not slept through.
    let mut wait: Option<Duration> = Some(Duration::ZERO);
    loop {
        let command = match commands.try_recv() {
            Ok(c) => Some(c),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Command::Shutdown),
        };
        if command.is_none() {
            match wait {
                Some(d) if d.is_zero() => {}
                due => {
                    if engine.bypass_pending() {
                        wait = Some(Duration::ZERO);
                        continue;
                    }
                    match due {
                        Some(d) => std::thread::park_timeout(d),
                        None => std::thread::park(),
                    }
                    wait = Some(Duration::ZERO);
                    continue;
                }
            }
        }
        match command {
            Some(Command::Apply(logical)) => match engine.apply(&logical) {
                Ok(applied) => sink(EngineEvent::Applied(applied)),
                Err(e) => sink(EngineEvent::ApplyFailed(e)),
            },
            Some(Command::Run(job)) => {
                // A panicking job must not take the engine and its workers down.
                let run = catch_unwind(AssertUnwindSafe(|| job(&mut engine)));
                if run.is_err() {
                    sink(EngineEvent::Error(EngineError::JobPanicked));
                }
            }
            Some(Command::Shutdown) => {
                let report = engine.shutdown();
                sink(EngineEvent::ShutDown(report));
                return;
            }
            None => {}
        }
        let serviced = engine.service();
        for (p, e) in serviced.events {
            sink(EngineEvent::Runtime(p, e));
        }
        for e in serviced.errors {
            sink(EngineEvent::Error(e));
        }
        wait = serviced.wait;
    }
}
