//! One partition: its controller, its executor and the worker thread that runs it, with
//! the partition's runtime type erased so the engine can hold partitions of different
//! runtimes side by side.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use rbs_core::time::Clock;
use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
use rbs_graph::partition::{QueueEnds, QueueReaderEnd};
use rbs_graph::runtime::queue::Doorbell;
use rbs_graph::runtime::{
    ApplyError, ApplyReport, Collected, ControlError, Controller, Executor, FaultStatus, Node,
    RestartPolicy, Runtime, channel,
};

use crate::worker::{Pacing, Worker, WorkerSpec, WorkerStats};

/// A node instance with its runtime type erased (a `Box<dyn Node<X>>` inside).
pub(crate) type AnyNode = Box<dyn Any + Send>;

/// Why a worker could not be started.
#[derive(Debug)]
pub(crate) struct SpawnFailed(pub(crate) std::io::ErrorKind);

/// The runtime-erased operations the engine needs on a partition.
pub(crate) trait Part: Send {
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn in_flight(&self) -> bool;
    fn apply(&mut self, description: &GraphDescription) -> Result<ApplyReport, ApplyError>;
    /// Waits up to `timeout` for the schedule in flight to be swapped in, and collects
    /// it. Starts the worker if the new schedule has nodes; stops it once the running
    /// schedule is empty.
    fn settle(&mut self, timeout: Duration) -> Result<Option<Collected>, SettleError>;
    /// Collects without waiting.
    fn collect(&mut self) -> Result<Option<Collected>, SettleError>;
    fn restart_due(&self) -> bool;
    fn next_due(&self) -> Option<Duration>;
    fn has_faults(&self) -> bool;
    fn retry(&mut self, id: NodeId) -> bool;
    fn fault(&self, id: NodeId) -> Option<FaultStatus>;
    fn set_restart_policy(&mut self, policy: RestartPolicy);
    fn adopt_writer(&mut self, id: NodeId, ends: QueueEnds)
    -> Result<QueueReaderEnd, ControlError>;
    fn adopt_reader(&mut self, id: NodeId, end: QueueReaderEnd) -> Result<(), ControlError>;
    fn take_any(&mut self, id: NodeId) -> Result<Option<AnyNode>, ControlError>;
    /// Adopts an instance taken from another partition; hands it back if its runtime
    /// type differs.
    fn adopt_any(&mut self, id: NodeId, node: AnyNode) -> Result<(), AdoptAnyError>;
    fn has_worker(&self) -> bool;
    /// True if the controller has an instance of `id`, idle or scheduled.
    fn holds(&self, id: NodeId) -> bool;
    fn wake(&self);
    fn stats(&self) -> Arc<WorkerStats>;
    fn stop_worker(&mut self);
    /// Runs the worker's final tick(s) (see `Worker::drain`); true if the worker
    /// drained within `timeout` or there is no worker.
    fn drain(&mut self, timeout: Duration) -> bool;
    /// From now on no worker starts; swaps without one run inline (shutdown).
    fn close(&mut self);
}

/// Why an instance moved between partitions was not adopted.
pub(crate) enum AdoptAnyError {
    /// The instance is for another runtime; it is handed back.
    WrongRuntime(AnyNode),
    /// The controller refused it (and dropped it).
    Control(ControlError),
}

/// Errors from settling a partition.
#[derive(Debug)]
pub(crate) enum SettleError {
    Control(ControlError),
    Spawn(SpawnFailed),
}

impl From<ControlError> for SettleError {
    fn from(e: ControlError) -> Self {
        SettleError::Control(e)
    }
}

pub(crate) struct Partition<X: Runtime> {
    id: PartitionId,
    controller: Controller<X>,
    /// The executor while no worker runs it.
    parked: Option<Executor<X>>,
    worker: Option<Worker<X>>,
    pacing: Pacing,
    clock: Arc<dyn Clock>,
    bell: Option<Arc<Doorbell>>,
    stats: Arc<WorkerStats>,
    next_tick: u64,
    /// The schedule last sent has no nodes.
    sent_empty: bool,
    /// Shutting down: never start a worker again.
    closing: bool,
}

impl<X: Runtime> Partition<X> {
    pub(crate) fn new(
        id: PartitionId,
        shared: Arc<X::Shared>,
        local: X::Local,
        pacing: Pacing,
        clock: Arc<dyn Clock>,
        bell: Option<Arc<Doorbell>>,
    ) -> Self {
        let (mut controller, executor) = channel::<X>(shared, local);
        controller.set_clock(Arc::clone(&clock));
        Self {
            id,
            controller,
            parked: Some(executor),
            worker: None,
            pacing,
            clock,
            bell,
            stats: Arc::new(WorkerStats::default()),
            next_tick: 0,
            sent_empty: true,
            closing: false,
        }
    }

    pub(crate) fn adopt(&mut self, id: NodeId, node: Box<dyn Node<X>>) -> Result<(), ControlError> {
        self.controller.adopt(id, node)
    }

    pub(crate) fn take(&mut self, id: NodeId) -> Result<Option<Box<dyn Node<X>>>, ControlError> {
        self.controller.take(id)
    }

    fn start_worker(&mut self) -> Result<(), SpawnFailed> {
        if self.worker.is_some() {
            return Ok(());
        }
        if self.parked.is_none() {
            // The executor was lost with a worker that panicked; nothing can run.
            return Err(SpawnFailed(std::io::ErrorKind::Other));
        }
        let spec = WorkerSpec {
            name: if self.id == PartitionId::RENDER {
                "rbs-render".to_owned()
            } else {
                format!("rbs-partition-{}", self.id.0)
            },
            pacing: self.pacing,
            clock: Arc::clone(&self.clock),
            bell: self.bell.clone(),
            stats: Arc::clone(&self.stats),
            next: self.next_tick,
        };
        match Worker::spawn(&mut self.parked, spec) {
            Ok(worker) => {
                self.worker = Some(worker);
                Ok(())
            }
            Err(e) => Err(SpawnFailed(e.kind())),
        }
    }

    /// After a collect: a worker whose running schedule is empty is stopped, so an
    /// idle partition has no thread (ADR-0013).
    fn after_collect(&mut self) {
        if self.sent_empty && !self.controller.in_flight() {
            self.stop_worker();
        }
    }
}

impl<X: Runtime> Part for Partition<X> {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn in_flight(&self) -> bool {
        self.controller.in_flight()
    }

    fn apply(&mut self, description: &GraphDescription) -> Result<ApplyReport, ApplyError> {
        let report = self.controller.apply(description)?;
        self.sent_empty = report.plan.order.is_empty();
        Ok(report)
    }

    fn settle(&mut self, timeout: Duration) -> Result<Option<Collected>, SettleError> {
        if !self.controller.in_flight() {
            return Ok(None);
        }
        if self.worker.is_none() && !self.sent_empty && !self.closing {
            self.start_worker().map_err(SettleError::Spawn)?;
        }
        let collected = match (&self.worker, self.parked.as_mut()) {
            (Some(worker), _) => {
                worker.wake();
                self.controller.collect_timeout(timeout)?
            }
            (None, Some(executor)) => {
                // No worker and an empty schedule on its way (or shutting down). Swap
                // it here, between ticks and without running one (no `begin_tick` or
                // `end_tick` on this thread, no tick index used). Normally the running
                // schedule is empty too. At shutdown, after a timed-out swap, this may
                // warm or cool nodes on the control thread before they are released.
                executor.swap_pending();
                self.controller.collect()?
            }
            (None, None) => return Err(SettleError::Spawn(SpawnFailed(std::io::ErrorKind::Other))),
        };
        self.after_collect();
        Ok(collected)
    }

    fn collect(&mut self) -> Result<Option<Collected>, SettleError> {
        if !self.controller.in_flight() {
            return Ok(None);
        }
        let collected = self.controller.collect()?;
        if collected.is_some() {
            self.after_collect();
        }
        Ok(collected)
    }

    fn restart_due(&self) -> bool {
        self.controller.restart_due()
    }

    fn next_due(&self) -> Option<Duration> {
        self.controller.next_due()
    }

    fn has_faults(&self) -> bool {
        !self.controller.faulted().is_empty()
    }

    fn retry(&mut self, id: NodeId) -> bool {
        self.controller.retry(id)
    }

    fn fault(&self, id: NodeId) -> Option<FaultStatus> {
        self.controller.fault(id)
    }

    fn set_restart_policy(&mut self, policy: RestartPolicy) {
        self.controller.set_restart_policy(policy);
    }

    fn adopt_writer(
        &mut self,
        id: NodeId,
        ends: QueueEnds,
    ) -> Result<QueueReaderEnd, ControlError> {
        let (writer, reader) = ends.writer::<X>();
        self.controller.adopt(id, writer)?;
        Ok(reader)
    }

    fn adopt_reader(&mut self, id: NodeId, end: QueueReaderEnd) -> Result<(), ControlError> {
        self.controller.adopt(id, end.into_node::<X>())
    }

    fn take_any(&mut self, id: NodeId) -> Result<Option<AnyNode>, ControlError> {
        Ok(self
            .controller
            .take(id)?
            .map(|node| Box::new(node) as AnyNode))
    }

    fn adopt_any(&mut self, id: NodeId, node: AnyNode) -> Result<(), AdoptAnyError> {
        match node.downcast::<Box<dyn Node<X>>>() {
            Ok(node) => self
                .controller
                .adopt(id, *node)
                .map_err(AdoptAnyError::Control),
            Err(node) => Err(AdoptAnyError::WrongRuntime(node)),
        }
    }

    fn has_worker(&self) -> bool {
        self.worker.is_some()
    }

    fn holds(&self, id: NodeId) -> bool {
        self.controller.is_idle(id) || self.controller.is_running(id)
    }

    fn wake(&self) {
        if let Some(worker) = &self.worker {
            worker.wake();
        }
    }

    fn stats(&self) -> Arc<WorkerStats> {
        Arc::clone(&self.stats)
    }

    fn close(&mut self) {
        self.closing = true;
    }

    fn drain(&mut self, timeout: Duration) -> bool {
        self.worker.as_ref().is_none_or(|w| w.drain(timeout))
    }

    fn stop_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            if let Some((executor, next)) = worker.stop() {
                self.parked = Some(executor);
                self.next_tick = next;
            }
        }
    }
}

impl<X: Runtime> Drop for Partition<X> {
    fn drop(&mut self) {
        // The engine drains before dropping; this only joins the thread.
        self.stop_worker();
    }
}
