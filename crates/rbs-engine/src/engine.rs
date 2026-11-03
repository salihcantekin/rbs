//! The engine: partitions, the logical graph's lowering, the control loop and shutdown.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use rbs_core::thread::ThreadPolicy;
use rbs_core::time::{Clock, MonotonicClock};
use rbs_graph::description::{GraphDescription, Link, NodeId, PartitionId};
use rbs_graph::partition::{Crossing, LowerProblem, Lowering};
use rbs_graph::runtime::{
    ApplyError, ApplyReport, Collected, ControlError, FaultStatus, Node, RestartPolicy, Runtime,
    RuntimeEvent,
};

use crate::part::{AdoptAnyError, Part, Partition, SettleError};
use crate::worker::{Pacing, WorkerStats};

/// Settings of an [`Engine`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    /// How long the control thread waits for a worker to swap in a schedule before it
    /// reports [`EngineError::SwapTimeout`] and moves on (the swap is collected later
    /// by [`Engine::service`]).
    pub swap_timeout: Duration,
    /// The longest sleep of the control loop while a worker runs: bypasses are only
    /// seen when the loop looks (ADR-0024, Decision 9).
    pub control_interval: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            swap_timeout: Duration::from_secs(1),
            control_interval: Duration::from_millis(250),
        }
    }
}

impl EngineConfig {
    /// Sets [`EngineConfig::swap_timeout`].
    pub fn swap_timeout(mut self, timeout: Duration) -> Self {
        self.swap_timeout = timeout;
        self
    }

    /// Sets [`EngineConfig::control_interval`].
    pub fn control_interval(mut self, interval: Duration) -> Self {
        self.control_interval = interval;
        self
    }
}

/// What went wrong in an engine call. Most are per partition and do not stop the
/// other partitions.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// No partition with this id was added.
    UnknownPartition(PartitionId),
    /// A partition with this id exists already.
    DuplicatePartition(PartitionId),
    /// The partition runs another `Runtime` than the call or the node instance.
    WrongRuntime(PartitionId),
    /// A node of the logical graph moved to a partition with another runtime; its
    /// instance stays in the old partition and the node is missing from the new one.
    MoveWrongRuntime {
        /// The node.
        node: NodeId,
        /// Where its instance stays.
        from: PartitionId,
        /// Where the graph wants it.
        to: PartitionId,
    },
    /// A moved node's instance was not idle in its old partition (its swap did not
    /// complete in time, or it had no instance).
    MoveNotIdle {
        /// The node.
        node: NodeId,
        /// The old partition.
        from: PartitionId,
    },
    /// The controller refused a call.
    Control(PartitionId, ControlError),
    /// A part could not be applied.
    Apply(PartitionId, ApplyError),
    /// The worker did not swap in time; [`Engine::service`] collects it later.
    SwapTimeout(PartitionId),
    /// At shutdown the worker did not finish its final tick(s) in time (a node hung in
    /// `process`, most likely): the worker is joined, which waits for that node, and
    /// the partition is drained on the control thread.
    DrainTimeout(PartitionId),
    /// The worker thread could not be started.
    Spawn(PartitionId, std::io::ErrorKind),
    /// The [`EngineHandle`](crate::EngineHandle)'s control thread could not be started.
    ControlThread(std::io::ErrorKind),
    /// A job queued with [`EngineHandle::run`](crate::EngineHandle::run) panicked; the
    /// engine keeps running.
    JobPanicked,
    /// The engine was shut down.
    ShutDown,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::UnknownPartition(p) => write!(f, "unknown partition {p}"),
            EngineError::DuplicatePartition(p) => write!(f, "partition {p} exists already"),
            EngineError::WrongRuntime(p) => write!(f, "partition {p} runs another runtime"),
            EngineError::MoveWrongRuntime { node, from, to } => write!(
                f,
                "node {node} cannot move from partition {from} to {to}: another runtime"
            ),
            EngineError::MoveNotIdle { node, from } => {
                write!(f, "node {node} is not idle in partition {from}")
            }
            EngineError::Control(p, e) => write!(f, "partition {p}: {e}"),
            EngineError::Apply(p, e) => write!(f, "partition {p}: {e}"),
            EngineError::SwapTimeout(p) => write!(f, "partition {p} did not swap in time"),
            EngineError::DrainTimeout(p) => write!(f, "partition {p} did not drain in time"),
            EngineError::Spawn(p, e) => write!(f, "partition {p}: cannot start worker: {e}"),
            EngineError::ControlThread(e) => write!(f, "cannot start the control thread: {e}"),
            EngineError::JobPanicked => f.write_str("an engine job panicked"),
            EngineError::ShutDown => f.write_str("engine is shut down"),
        }
    }
}

impl std::error::Error for EngineError {}

impl EngineError {
    fn settle(p: PartitionId, e: SettleError) -> Self {
        match e {
            SettleError::Control(e) => EngineError::Control(p, e),
            SettleError::Spawn(s) => EngineError::Spawn(p, s.0),
        }
    }
}

/// The result of [`Engine::apply`].
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct Applied {
    /// What each partition's controller did, for the partitions this edit touched.
    pub reports: BTreeMap<PartitionId, ApplyReport>,
    /// Failures, restarts and recoveries noticed, with their partition.
    pub events: Vec<(PartitionId, RuntimeEvent)>,
    /// Queues created by this edit. Give each `stats` to the consumer node if it wants
    /// drop counts (an encoder reads `video_drops`), again after a move.
    pub new_queues: Vec<Crossing>,
    /// Links that could not cross partitions; the graph runs without them.
    pub problems: Vec<(Link, LowerProblem)>,
    /// Nodes moved between partitions: `(node, from, to)`.
    pub moved: Vec<(NodeId, PartitionId, PartitionId)>,
    /// Errors; the rest of the edit was still applied.
    pub errors: Vec<EngineError>,
}

/// The result of [`Engine::service`].
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct Serviced {
    /// Failures, restarts and recoveries, with their partition.
    pub events: Vec<(PartitionId, RuntimeEvent)>,
    /// Errors seen (for example a swap that timed out again).
    pub errors: Vec<EngineError>,
    /// How long the control loop may sleep before calling `service` again. `None`:
    /// nothing runs and no restart is pending, so it may sleep until the next edit.
    pub wait: Option<Duration>,
}

/// The result of [`Engine::shutdown`].
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct Shutdown {
    /// Partitions in the order they were drained.
    pub order: Vec<PartitionId>,
    /// Events seen while draining.
    pub events: Vec<(PartitionId, RuntimeEvent)>,
    /// Errors while draining (a partition that did not swap in time is stopped anyway).
    pub errors: Vec<EngineError>,
}

/// Runs a logical graph split into partitions, each on its own worker thread
/// (ADR-0026, ADR-0027).
///
/// All methods run on the control thread and may block for up to
/// [`EngineConfig::swap_timeout`] per partition while a worker swaps. A UI thread uses
/// [`EngineHandle`](crate::EngineHandle) instead, whose calls return at once.
///
/// Zero cost when unused (ADR-0013): a partition has a worker thread only while its
/// running schedule has nodes. An engine whose partitions run nothing has no thread.
pub struct Engine {
    config: EngineConfig,
    clock: Arc<dyn Clock>,
    lowering: Lowering,
    parts: BTreeMap<PartitionId, Box<dyn Part>>,
    /// The part last applied to each partition, re-applied for restarts.
    current: BTreeMap<PartitionId, GraphDescription>,
    crossings: Vec<Crossing>,
    /// Where each logical node is.
    placement: HashMap<NodeId, PartitionId>,
    /// Endpoints of retired queues whose swap was not collected yet.
    retiring: Vec<(PartitionId, NodeId)>,
    /// Partitions whose current part was not sent yet (a swap timed out, or a moved
    /// instance arrived after their part was applied).
    dirty: BTreeSet<PartitionId>,
    /// Moves whose instance was not idle yet: `(node, from, to)`.
    moves: Vec<(NodeId, PartitionId, PartitionId)>,
    /// Raises worker threads' scheduling class (ADR-0030); none by default.
    thread_policy: Option<Arc<dyn ThreadPolicy>>,
    /// Partitions emptied by [`Engine::apply_draining`] whose empty part is not sent
    /// yet: they run their final tick(s) right before it is, also when that happens later
    /// in [`Engine::service`] (a swap still in flight).
    draining: BTreeSet<PartitionId>,
    shut_down: bool,
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("partitions", &self.parts.keys().collect::<Vec<_>>())
            .field("crossings", &self.crossings.len())
            .field("shut_down", &self.shut_down)
            .finish_non_exhaustive()
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    /// An engine on the production master clock with default settings.
    pub fn new() -> Self {
        Self::with_clock(Arc::new(MonotonicClock::new()), EngineConfig::default())
    }

    /// An engine on `clock` (tests use `TestClock`). Workers tick on it, and
    /// controllers measure restart back-off on it.
    pub fn with_clock(clock: Arc<dyn Clock>, config: EngineConfig) -> Self {
        Self {
            config,
            clock,
            lowering: Lowering::new(),
            parts: BTreeMap::new(),
            current: BTreeMap::new(),
            crossings: Vec::new(),
            placement: HashMap::new(),
            retiring: Vec::new(),
            dirty: BTreeSet::new(),
            moves: Vec::new(),
            thread_policy: None,
            draining: BTreeSet::new(),
            shut_down: false,
        }
    }

    /// The engine's clock.
    pub fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    /// Capacity of packet queues created from now on.
    pub fn set_packet_capacity(&mut self, packets: usize) {
        self.lowering.set_packet_capacity(packets);
    }

    /// Adds partition `id`, run by runtime `X` with the given resources and pacing.
    /// No thread starts until a schedule with nodes is applied to it.
    pub fn add_partition<X: Runtime>(
        &mut self,
        id: PartitionId,
        shared: Arc<X::Shared>,
        local: X::Local,
        pacing: Pacing,
    ) -> Result<(), EngineError> {
        if self.shut_down {
            return Err(EngineError::ShutDown);
        }
        if self.parts.contains_key(&id) {
            return Err(EngineError::DuplicatePartition(id));
        }
        let bell = match pacing {
            Pacing::Doorbell { .. } => Some(self.lowering.doorbell(id)),
            Pacing::Cadence { .. } => None,
        };
        let mut part =
            Partition::<X>::new(id, shared, local, pacing, Arc::clone(&self.clock), bell);
        part.set_thread_policy(self.thread_policy.clone());
        self.parts.insert(id, Box::new(part));
        Ok(())
    }

    /// Sets the policy that raises each worker thread's scheduling class (ADR-0030):
    /// called on the worker thread before its first tick, with
    /// [`ThreadClass::Periodic`](rbs_core::thread::ThreadClass::Periodic) for cadence
    /// partitions and [`ThreadClass::Responsive`](rbs_core::thread::ThreadClass::Responsive)
    /// for doorbell partitions, and undone on that thread after its last tick. Applies
    /// to workers started from now on; `None` (the default) calls nothing.
    pub fn set_thread_policy(&mut self, policy: Option<Arc<dyn ThreadPolicy>>) {
        for part in self.parts.values_mut() {
            part.set_thread_policy(policy.clone());
        }
        self.thread_policy = policy;
    }

    fn typed<X: Runtime>(&mut self, p: PartitionId) -> Result<&mut Partition<X>, EngineError> {
        self.parts
            .get_mut(&p)
            .ok_or(EngineError::UnknownPartition(p))?
            .as_any_mut()
            .downcast_mut::<Partition<X>>()
            .ok_or(EngineError::WrongRuntime(p))
    }

    /// Hands over the instance of node `id` to partition `p` (Standby until a plan
    /// schedules it). The node's partition in the graph must be `p`.
    pub fn adopt<X: Runtime>(
        &mut self,
        p: PartitionId,
        id: NodeId,
        node: Box<dyn Node<X>>,
    ) -> Result<(), EngineError> {
        self.typed::<X>(p)?
            .adopt(id, node)
            .map_err(|e| EngineError::Control(p, e))
    }

    /// Takes back the idle instance of node `id` from partition `p`.
    pub fn take<X: Runtime>(
        &mut self,
        p: PartitionId,
        id: NodeId,
    ) -> Result<Option<Box<dyn Node<X>>>, EngineError> {
        self.typed::<X>(p)?
            .take(id)
            .map_err(|e| EngineError::Control(p, e))
    }

    /// Partitions in pipeline order: producers before the partitions their queues
    /// feed (render, then encoders, then outputs), ties and cycles in id order.
    pub fn pipeline_order(&self) -> Vec<PartitionId> {
        pipeline_order(self.parts.keys().copied(), &self.crossings)
    }

    /// Every queue in use.
    pub fn crossings(&self) -> &[Crossing] {
        &self.crossings
    }

    /// Counters of partition `p`'s worker.
    pub fn worker_stats(&self, p: PartitionId) -> Option<Arc<WorkerStats>> {
        self.parts.get(&p).map(|part| part.stats())
    }

    /// True while partition `p` has a worker thread.
    pub fn has_worker(&self, p: PartitionId) -> bool {
        self.parts.get(&p).is_some_and(|part| part.has_worker())
    }

    /// Makes partition `p`'s worker look at its pacing now: a doorbell worker ticks, a
    /// cadence worker re-reads the clock (tests that move a `TestClock` call this).
    pub fn wake(&self, p: PartitionId) {
        if let Some(part) = self.parts.get(&p) {
            part.wake();
        }
    }

    /// Number of worker threads running.
    pub fn workers(&self) -> usize {
        self.parts.values().filter(|p| p.has_worker()).count()
    }

    /// Sets the restart policy of partition `p` (ADR-0024).
    pub fn set_restart_policy(
        &mut self,
        p: PartitionId,
        policy: RestartPolicy,
    ) -> Result<(), EngineError> {
        self.parts
            .get_mut(&p)
            .ok_or(EngineError::UnknownPartition(p))?
            .set_restart_policy(policy);
        Ok(())
    }

    /// The failure state of logical node `id`.
    pub fn fault(&self, id: NodeId) -> Option<FaultStatus> {
        let p = self.placement.get(&id)?;
        self.parts.get(p)?.fault(id)
    }

    /// The user's "retry" for logical node `id`; the next [`Engine::service`] restarts
    /// it. Returns whether it had a failure record.
    pub fn retry(&mut self, id: NodeId) -> bool {
        let Some(p) = self.placement.get(&id) else {
            return false;
        };
        self.parts.get_mut(p).is_some_and(|part| part.retry(id))
    }

    /// Applies the logical graph: lowers it, adopts new queue endpoints, applies each
    /// partition's part (two phases when nodes move between partitions, ADR-0026
    /// Decision 6) and drops the endpoints of retired queues.
    ///
    /// Every node's partition must have been added; otherwise nothing is applied.
    pub fn apply(&mut self, logical: &GraphDescription) -> Result<Applied, EngineError> {
        self.apply_inner(logical, false)
    }

    /// Applies the logical graph like [`Engine::apply`], but a partition whose worker
    /// runs and whose new schedule is empty first runs its final tick(s) on its old
    /// schedule, as at [`Engine::shutdown`] (ADR-0029, ADR-0041): one tick on a cadence,
    /// so an encoder encodes the last frame its queue holds; on a doorbell, ticks until
    /// no reader has packets left, so an output writes what reached it before it is
    /// released. Partitions are sent in pipeline order over the queues before and after
    /// the edit, so a partition drains after the partitions that fed it.
    ///
    /// This is how a branch of the graph (a recording's encoders and output) is removed
    /// while the rest keeps running. A partition whose earlier swap is still in flight
    /// drains when [`Engine::service`] sends its part, unless an `apply` before that gives
    /// it nodes again. A drain that times out is reported as
    /// [`EngineError::DrainTimeout`]; the worker is then joined and the part swapped on
    /// the control thread.
    pub fn apply_draining(&mut self, logical: &GraphDescription) -> Result<Applied, EngineError> {
        self.apply_inner(logical, true)
    }

    fn apply_inner(
        &mut self,
        logical: &GraphDescription,
        drain: bool,
    ) -> Result<Applied, EngineError> {
        if self.shut_down {
            return Err(EngineError::ShutDown);
        }
        if let Some(n) = logical
            .nodes()
            .iter()
            .find(|n| !self.parts.contains_key(&n.partition))
        {
            return Err(EngineError::UnknownPartition(n.partition));
        }
        let mut lowered = self.lowering.lower(logical);
        let mut out = Applied {
            problems: lowered.problems.clone(),
            moved: lowered.moved.clone(),
            ..Applied::default()
        };

        // Endpoints of new queues, under the reported ids.
        for new in lowered.created.drain(..) {
            let c = new.crossing;
            out.new_queues.push(c.clone());
            let Some(from) = self.parts.get_mut(&c.from) else {
                continue;
            };
            let reader = match from.adopt_writer(c.writer, new.ends) {
                Ok(reader) => reader,
                Err(e) => {
                    out.errors.push(EngineError::Control(c.from, e));
                    continue;
                }
            };
            if let Some(to) = self.parts.get_mut(&c.to) {
                if let Err(e) = to.adopt_reader(c.reader, reader) {
                    out.errors.push(EngineError::Control(c.to, e));
                }
            }
        }
        let order = if drain {
            // Queues being retired still order their ends: the partitions that fed a
            // drained partition are sent (and stop feeding it) first.
            let both: Vec<Crossing> = self
                .crossings
                .iter()
                .chain(&lowered.crossings)
                .cloned()
                .collect();
            pipeline_order(self.parts.keys().copied(), &both)
        } else {
            pipeline_order(self.parts.keys().copied(), &lowered.crossings)
        };
        self.crossings = lowered.crossings.clone();
        // A drain still pending from an earlier call holds only while the partition's
        // newest part stays empty.
        self.draining.retain(|p| {
            lowered
                .parts
                .get(p)
                .is_none_or(|part| part.nodes().is_empty())
        });
        if drain {
            let emptied: Vec<PartitionId> = lowered
                .parts
                .iter()
                .filter(|(p, part)| {
                    part.nodes().is_empty() && self.parts.get(p).is_some_and(|x| x.has_worker())
                })
                .map(|(p, _)| *p)
                .collect();
            self.draining.extend(emptied);
        }
        let leaving: BTreeSet<PartitionId> = lowered.moved.iter().map(|m| m.1).collect();
        for (p, part) in std::mem::take(&mut lowered.parts) {
            self.current.insert(p, part);
            self.dirty.insert(p);
        }
        self.placement = logical
            .nodes()
            .iter()
            .map(|n| (n.id, n.partition))
            .collect();
        self.moves.extend(lowered.moved.iter().copied());

        // Phase 1: the partitions moved nodes leave, so their instances go idle.
        for p in order.iter().filter(|p| leaving.contains(p)) {
            self.send_current(*p, &mut out.reports, &mut out.events, &mut out.errors);
        }
        // Move the idle instances; each partition that gains one is sent again below,
        // also one that was sent in phase 1.
        self.run_moves(&mut out.errors);
        // Phase 2: every partition with a part not sent yet, in pipeline order.
        for p in &order {
            if self.dirty.contains(p) {
                self.send_current(*p, &mut out.reports, &mut out.events, &mut out.errors);
            }
        }

        // Retired queues: their endpoints left with the swaps above.
        for c in &lowered.retired {
            self.retiring.push((c.from, c.writer));
            self.retiring.push((c.to, c.reader));
        }
        self.drop_retired();
        Ok(out)
    }

    /// Sends partition `p`'s current part if it is not sent yet and settles the swap.
    /// A swap still in flight is collected first; if it does not come back in time, the
    /// part stays unsent and [`Engine::service`] sends it later.
    fn send_current(
        &mut self,
        p: PartitionId,
        reports: &mut BTreeMap<PartitionId, ApplyReport>,
        events: &mut Vec<(PartitionId, RuntimeEvent)>,
        errors: &mut Vec<EngineError>,
    ) {
        let timeout = self.config.swap_timeout;
        let empty = GraphDescription::new();
        let Some(handle) = self.parts.get_mut(&p) else {
            return;
        };
        if handle.in_flight() {
            match handle.settle(timeout) {
                Ok(c) => push_collected(p, c, events),
                Err(e) => errors.push(EngineError::settle(p, e)),
            }
            if handle.in_flight() {
                errors.push(EngineError::SwapTimeout(p));
                return;
            }
        }
        self.dirty.remove(&p);
        if self.draining.remove(&p) && !handle.drain(timeout) {
            // As at shutdown: most likely a node hung in `process`. Join the worker; the
            // swap below then runs on this thread.
            errors.push(EngineError::DrainTimeout(p));
            handle.stop_worker();
        }
        let part = self.current.get(&p).unwrap_or(&empty);
        match handle.apply(part) {
            Ok(report) => {
                events.extend(report.events.iter().cloned().map(|e| (p, e)));
                reports.insert(p, report);
            }
            Err(e) => {
                errors.push(EngineError::Apply(p, e));
                return;
            }
        }
        match handle.settle(timeout) {
            Ok(c) => push_collected(p, c, events),
            Err(e) => errors.push(EngineError::settle(p, e)),
        }
        if handle.in_flight() {
            errors.push(EngineError::SwapTimeout(p));
        }
    }

    /// Moves the instances of pending moves whose old partition let them go. A move
    /// whose old partition still has a swap in flight waits for a later call.
    fn run_moves(&mut self, errors: &mut Vec<EngineError>) {
        let pending = std::mem::take(&mut self.moves);
        for (node, from, to) in pending {
            // A node removed from the graph since, or moved on again, is not moved here.
            if self.placement.get(&node) != Some(&to) {
                continue;
            }
            // Already there (a move back after `MoveWrongRuntime`): nothing to move.
            if self.parts.get(&to).is_some_and(|p| p.holds(node)) {
                continue;
            }
            match self.move_instance(node, from, to, errors) {
                Moved::Done => {
                    self.dirty.insert(to);
                }
                Moved::Wait => self.moves.push((node, from, to)),
                Moved::Failed => {}
            }
        }
    }

    fn move_instance(
        &mut self,
        node: NodeId,
        from: PartitionId,
        to: PartitionId,
        errors: &mut Vec<EngineError>,
    ) -> Moved {
        let unsent = self.dirty.contains(&from);
        let Some(source) = self.parts.get_mut(&from) else {
            return Moved::Failed;
        };
        let taken = match source.take_any(node) {
            Ok(Some(instance)) => instance,
            // Still in the old schedule: its swap is in flight, or the part that lets it
            // go is not sent yet. Try again after that.
            Ok(None) | Err(ControlError::Running(_)) if source.in_flight() || unsent => {
                return Moved::Wait;
            }
            Ok(None) | Err(_) => {
                errors.push(EngineError::MoveNotIdle { node, from });
                return Moved::Failed;
            }
        };
        let Some(target) = self.parts.get_mut(&to) else {
            return Moved::Failed;
        };
        match target.adopt_any(node, taken) {
            Ok(()) => Moved::Done,
            Err(AdoptAnyError::WrongRuntime(instance)) => {
                if let Some(source) = self.parts.get_mut(&from) {
                    // It was just taken from there, so the id is free.
                    let _ = source.adopt_any(node, instance);
                }
                errors.push(EngineError::MoveWrongRuntime { node, from, to });
                Moved::Failed
            }
            Err(AdoptAnyError::Control(e)) => {
                errors.push(EngineError::Control(to, e));
                Moved::Failed
            }
        }
    }

    /// Drops retired endpoints that are idle now; keeps the others for later.
    fn drop_retired(&mut self) {
        let parts = &mut self.parts;
        self.retiring.retain(|(p, id)| match parts.get_mut(p) {
            Some(part) => match part.take_any(*id) {
                // Idle: dropped here, on the control thread.
                Ok(Some(_)) => false,
                Err(ControlError::Running(_)) => true,
                // Not here: still inside the executor while a swap is in flight, else gone.
                Ok(None) => part.in_flight(),
                Err(_) => false,
            },
            None => false,
        });
    }

    /// One round of the control loop (ADR-0024, Decision 9): collect every partition's
    /// swap, re-apply the parts whose restarts are due, and say how long to sleep.
    pub fn service(&mut self) -> Serviced {
        let mut out = Serviced::default();
        if self.shut_down {
            return out;
        }
        let order = self.pipeline_order();
        for p in &order {
            let Some(part) = self.parts.get_mut(p) else {
                continue;
            };
            match part.collect() {
                Ok(c) => push_collected(*p, c, &mut out.events),
                Err(e) => out.errors.push(EngineError::settle(*p, e)),
            }
        }
        // Work an earlier timeout left: first the parts of partitions that pending moves
        // leave, then the moves, then every other part not sent yet.
        let mut reports = BTreeMap::new();
        let leaving: BTreeSet<PartitionId> = self.moves.iter().map(|m| m.1).collect();
        for p in order.iter().filter(|p| leaving.contains(p)) {
            let idle = self.parts.get(p).is_some_and(|part| !part.in_flight());
            if idle && self.dirty.contains(p) {
                self.send_current(*p, &mut reports, &mut out.events, &mut out.errors);
            }
        }
        self.run_moves(&mut out.errors);
        for p in &order {
            let Some(part) = self.parts.get(p) else {
                continue;
            };
            if part.in_flight() {
                continue;
            }
            if self.dirty.contains(p) || part.restart_due() {
                self.dirty.insert(*p);
                self.send_current(*p, &mut reports, &mut out.events, &mut out.errors);
            }
        }
        self.drop_retired();

        let mut wait: Option<Duration> = None;
        let mut min = |d: Duration| wait = Some(wait.map_or(d, |w| w.min(d)));
        for part in self.parts.values() {
            if let Some(d) = part.next_due() {
                min(d);
            }
            if part.has_worker() || part.in_flight() || part.has_faults() {
                min(self.config.control_interval);
            }
        }
        if !self.retiring.is_empty() || !self.dirty.is_empty() || !self.moves.is_empty() {
            min(self.config.control_interval);
        }
        out.wait = wait;
        out
    }

    /// Stops the engine: drains the partitions in pipeline order (render, then
    /// encoders, then outputs; ADR-0026 Decision 8), so each partition's inputs stop
    /// before the next one flushes, then stops every worker. Idempotent.
    ///
    /// Each running partition first runs its final tick(s) on its worker: one tick on a
    /// cadence, so an encoder encodes the last frame render wrote; on a doorbell, ticks
    /// until no reader has packets left. Then the empty schedule is swapped in.
    pub fn shutdown(&mut self) -> Shutdown {
        let mut out = Shutdown::default();
        if self.shut_down {
            return out;
        }
        self.shut_down = true;
        let timeout = self.config.swap_timeout;
        let empty = GraphDescription::new();
        out.order = self.pipeline_order();
        for p in out.order.clone() {
            let Some(part) = self.parts.get_mut(&p) else {
                continue;
            };
            // No new worker starts from here on; swaps without one happen inline.
            part.close();
            let settle = |part: &mut Box<dyn Part>, out: &mut Shutdown| {
                if part.in_flight() {
                    match part.settle(timeout) {
                        Ok(c) => push_collected(p, c, &mut out.events),
                        Err(e) => out.errors.push(EngineError::settle(p, e)),
                    }
                }
                if part.in_flight() {
                    // Timed out: join the worker (its current swap finishes first),
                    // then swap on this thread.
                    out.errors.push(EngineError::SwapTimeout(p));
                    part.stop_worker();
                    match part.settle(timeout) {
                        Ok(c) => push_collected(p, c, &mut out.events),
                        Err(e) => out.errors.push(EngineError::settle(p, e)),
                    }
                }
            };
            // Finish a swap still in flight, run the final tick(s) on the old schedule,
            // then swap in the empty one.
            settle(part, &mut out);
            if !part.in_flight() && !part.drain(timeout) {
                // Most likely a node hung in `process`: join the worker (this waits for
                // the node to return, as a timed-out swap does) and drain inline, so the
                // control thread does not wait another timeout for the swap.
                out.errors.push(EngineError::DrainTimeout(p));
                part.stop_worker();
            }
            if !part.in_flight() {
                match part.apply(&empty) {
                    Ok(report) => out.events.extend(report.events.into_iter().map(|e| (p, e))),
                    Err(e) => out.errors.push(EngineError::Apply(p, e)),
                }
                settle(part, &mut out);
            }
            part.stop_worker();
        }
        for c in std::mem::take(&mut self.crossings) {
            self.retiring.push((c.from, c.writer));
            self.retiring.push((c.to, c.reader));
        }
        self.drop_retired();
        self.retiring.clear();
        self.current.clear();
        self.dirty.clear();
        self.moves.clear();
        out
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// What became of one move.
enum Moved {
    Done,
    Wait,
    Failed,
}

fn push_collected(
    p: PartitionId,
    collected: Option<Collected>,
    events: &mut Vec<(PartitionId, RuntimeEvent)>,
) {
    if let Some(c) = collected {
        events.extend(c.events.into_iter().map(|e| (p, e)));
    }
}

/// Topological order of partitions along queue direction; ties and cycles by id.
pub(crate) fn pipeline_order(
    partitions: impl IntoIterator<Item = PartitionId>,
    crossings: &[Crossing],
) -> Vec<PartitionId> {
    let all: BTreeSet<PartitionId> = partitions.into_iter().collect();
    let mut indegree: BTreeMap<PartitionId, usize> = all.iter().map(|p| (*p, 0)).collect();
    let mut edges: BTreeSet<(PartitionId, PartitionId)> = BTreeSet::new();
    for c in crossings {
        if c.from != c.to && all.contains(&c.from) && all.contains(&c.to) {
            edges.insert((c.from, c.to));
        }
    }
    for (_, to) in &edges {
        if let Some(d) = indegree.get_mut(to) {
            *d += 1;
        }
    }
    let mut ready: BTreeSet<PartitionId> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(p, _)| *p)
        .collect();
    let mut order = Vec::with_capacity(all.len());
    let mut done: BTreeSet<PartitionId> = BTreeSet::new();
    loop {
        while let Some(p) = ready.pop_first() {
            order.push(p);
            done.insert(p);
            for (_, to) in edges.range((p, PartitionId(0))..=(p, PartitionId(u32::MAX))) {
                if done.contains(to) {
                    continue;
                }
                if let Some(d) = indegree.get_mut(to) {
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        ready.insert(*to);
                    }
                }
            }
        }
        // A cycle: break it at the lowest id left.
        match all.iter().find(|p| !done.contains(p)) {
            Some(&p) => {
                indegree.insert(p, 0);
                ready.insert(p);
            }
            None => break,
        }
    }
    order
}
