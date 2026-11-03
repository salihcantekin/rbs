//! The control side: owns idle node instances, prepares joining nodes and builds
//! schedules for the executor.

use std::collections::HashMap;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::time::Duration;

use rbs_core::lifecycle::Lifecycle;
use rbs_core::time::{Clock, MonotonicClock, Timestamp};

use crate::description::{GraphDescription, Link, NodeId, SchedulePlan};
use crate::node::LinkKind;

use super::executor::{Executor, NodeHealth};
use super::restart::{FaultCause, FaultStatus, Faults, Gate, RestartPolicy, RuntimeEvent};
use super::schedule::{Entry, Schedule, Slot, Span};
use super::{Node, NodeError, PrepareCtx, Runtime};

/// Creates a connected controller (for the control thread) and executor (for the
/// thread that runs ticks). Both start with an empty schedule.
pub fn channel<X: Runtime>(
    shared: Arc<X::Shared>,
    local: X::Local,
) -> (Controller<X>, Executor<X>) {
    let (to_exec, from_ctrl) = sync_channel(1);
    let (to_ctrl, from_exec) = sync_channel(1);
    let bypasses = Arc::new(AtomicU64::new(0));
    let controller = Controller {
        bypasses: Arc::clone(&bypasses),
        bypasses_seen: 0,
        shared: Arc::clone(&shared),
        idle: HashMap::new(),
        running: HashMap::new(),
        outbox: to_exec,
        inbox: from_exec,
        in_flight: false,
        clock: Arc::new(MonotonicClock::new()),
        faults: Faults::default(),
    };
    (
        controller,
        Executor::new(shared, local, from_ctrl, to_ctrl, bypasses),
    )
}

/// A schedule the executor swapped out, on its way back to the controller.
pub(crate) struct Retired<X: Runtime> {
    schedule: Schedule<X>,
    /// The executor's bypass counter when it swapped; every bypass counted up to here
    /// shows in this schedule's health.
    bypasses: u64,
}

impl<X: Runtime> Retired<X> {
    pub(crate) fn new(schedule: Schedule<X>, bypasses: u64) -> Self {
        Self { schedule, bypasses }
    }
}

/// What the controller did with a description.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// The plan that was sent. Nodes whose `prepare` failed were left out and the plan
    /// recomputed without them, so this can differ from `description.plan()`.
    pub plan: SchedulePlan,
    /// Nodes the description wants Active that could not join, and why. Includes
    /// nodes kept out because they are waiting for a restart
    /// ([`JoinError::BackingOff`]).
    pub failed: Vec<(NodeId, JoinError)>,
    /// Failures, restarts and recoveries noticed by this call, for the event stream.
    pub events: Vec<RuntimeEvent>,
}

/// Why a node the description wants Active could not join the schedule.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinError {
    /// The controller has no instance for the node.
    NoInstance,
    /// The instance's descriptor does not match the node's descriptor in the graph.
    DescriptorMismatch,
    /// `prepare` returned an error (`release` was called).
    Prepare(NodeError),
    /// `prepare` panicked (`release` was called).
    Panicked,
    /// The node failed earlier and waits for a restart: until `retry_at`, or for the
    /// user ([`Controller::retry`]) when `retry_at` is `None` (ADR-0024).
    BackingOff {
        /// When the next `apply` may prepare it again.
        retry_at: Option<Timestamp>,
    },
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JoinError::NoInstance => f.write_str("no node instance"),
            JoinError::DescriptorMismatch => f.write_str("instance descriptor does not match"),
            JoinError::Prepare(e) => write!(f, "prepare failed: {e}"),
            JoinError::Panicked => f.write_str("prepare panicked"),
            JoinError::BackingOff { retry_at: Some(t) } => {
                write!(f, "waiting to restart at {} ns", t.as_nanos())
            }
            JoinError::BackingOff { retry_at: None } => {
                f.write_str("not restarted automatically; retry it")
            }
        }
    }
}

impl std::error::Error for JoinError {}

/// What [`Controller::collect`] took back from the executor.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collected {
    /// Nodes that left the schedule (now idle and released), with their last counters.
    pub left: Vec<(NodeId, NodeHealth)>,
    /// Counters of every node of the swapped-out schedule as of the swap, including
    /// nodes that stayed; this is how the control side sees bypassed nodes.
    pub health: Vec<(NodeId, NodeHealth)>,
    /// Failures (bypassed nodes) and recoveries noticed by this call, for the event
    /// stream.
    pub events: Vec<RuntimeEvent>,
}

/// Why a description could not be applied.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// The previous schedule has not been taken up yet; call
    /// [`Controller::collect`] after the executor's next tick.
    Busy,
    /// The executor is gone.
    ExecutorGone,
    /// The plan links a kind this executor does not carry (audio runs on the audio
    /// executor). Nothing was sent and no node was prepared.
    UnsupportedLink(Link),
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyError::Busy => f.write_str("previous schedule not taken up yet"),
            ApplyError::ExecutorGone => f.write_str("executor is gone"),
            ApplyError::UnsupportedLink(l) => {
                write!(f, "{} link not carried by this executor", l.kind)
            }
        }
    }
}

impl std::error::Error for ApplyError {}

/// Why a node instance could not be handed over or taken back.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    /// The controller already has an instance for this id.
    AlreadyAdopted(NodeId),
    /// The node is scheduled; remove it from the schedule first.
    Running(NodeId),
    /// The executor is gone.
    ExecutorGone,
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlError::AlreadyAdopted(id) => write!(f, "{id} already has an instance"),
            ControlError::Running(id) => write!(f, "{id} is scheduled"),
            ControlError::ExecutorGone => f.write_str("executor is gone"),
        }
    }
}

impl std::error::Error for ControlError {}

/// Where a running node sits in the schedule last sent.
#[derive(Debug, Clone, Copy)]
struct Placement {
    index: u32,
    outputs: Span,
}

/// The control-thread side of a running graph.
///
/// Node instances are either idle here (Standby: no resources) or in the executor's
/// schedule (Active). The description's lifecycle states are what the scene wants;
/// whether a node actually runs is decided by the plan, and a node the plan skips is
/// kept idle with its resources released, so it can join again later (ADR-0020).
pub struct Controller<X: Runtime> {
    shared: Arc<X::Shared>,
    idle: HashMap<NodeId, Box<dyn Node<X>>>,
    /// Layout of the schedule last sent (the executor runs it, or will after its next
    /// tick).
    running: HashMap<NodeId, Placement>,
    outbox: SyncSender<Schedule<X>>,
    inbox: Receiver<Retired<X>>,
    in_flight: bool,
    /// Time for back-off; the production master clock unless replaced.
    clock: Arc<dyn Clock>,
    faults: Faults,
    /// Bumped by the executor whenever it bypasses a node, so the control side learns
    /// about it without waiting for an edit.
    bypasses: Arc<AtomicU64>,
    /// The counter value whose bypasses have been collected.
    bypasses_seen: u64,
}

impl<X: Runtime> Controller<X> {
    /// Hands over the instance for node `id`, idle (Standby) until a plan schedules it.
    pub fn adopt(&mut self, id: NodeId, node: Box<dyn Node<X>>) -> Result<(), ControlError> {
        if self.idle.contains_key(&id) || self.running.contains_key(&id) {
            return Err(ControlError::AlreadyAdopted(id));
        }
        self.idle.insert(id, node);
        Ok(())
    }

    /// Takes back an idle instance (for example to destroy it: Standby to Installed).
    pub fn take(&mut self, id: NodeId) -> Result<Option<Box<dyn Node<X>>>, ControlError> {
        if self.running.contains_key(&id) {
            return Err(ControlError::Running(id));
        }
        Ok(self.idle.remove(&id))
    }

    /// True if node `id` is in the schedule last sent.
    pub fn is_running(&self, id: NodeId) -> bool {
        self.running.contains_key(&id)
    }

    /// True if node `id` has an idle instance here.
    pub fn is_idle(&self, id: NodeId) -> bool {
        self.idle.contains_key(&id)
    }

    /// Replaces the clock used for restart back-off (tests use `TestClock`).
    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }

    /// Sets when failed nodes are restarted (ADR-0024). Applies to the next failure;
    /// waits already scheduled keep their time.
    pub fn set_restart_policy(&mut self, policy: RestartPolicy) {
        self.faults.set_policy(policy);
    }

    /// The restart policy in use.
    pub fn restart_policy(&self) -> RestartPolicy {
        self.faults.policy()
    }

    /// The failure state of node `id`, if it failed and has not been forgiven.
    pub fn fault(&self, id: NodeId) -> Option<FaultStatus> {
        self.faults.status(id)
    }

    /// Nodes with a failure record, in id order.
    pub fn faulted(&self) -> Vec<NodeId> {
        let mut ids: Vec<NodeId> = self.faults.nodes().collect();
        ids.sort();
        ids
    }

    /// Forgets the failures of node `id`, so the next `apply` prepares it at once
    /// (the user's "retry"). Returns whether there was a record.
    pub fn retry(&mut self, id: NodeId) -> bool {
        self.faults.clear(id)
    }

    /// The earliest time a waiting node may be restarted.
    pub fn next_retry(&self) -> Option<Timestamp> {
        self.faults.next_retry()
    }

    /// True if calling `apply` again with the same description would change the
    /// schedule because of restarts: a scheduled node failed and must be taken out, or
    /// a waiting node's back-off is over. The engine's control loop checks this on its
    /// timer and re-applies the current description.
    pub fn restart_due(&self) -> bool {
        self.bypasses.load(Ordering::Relaxed) != self.bypasses_seen
            || self.running.keys().any(|&id| self.faults.waiting(id))
            || self.next_retry().is_some_and(|at| at <= self.clock.now())
    }

    /// True while a sent schedule has not come back through [`Controller::collect`].
    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// Plans `description`, prepares the nodes that join, and sends the schedule. The
    /// executor swaps to it after its next tick.
    ///
    /// A node whose `prepare` fails (or that has no instance) is left out: the plan is
    /// recomputed as if it were Standby, so nodes that depended on it are skipped too.
    /// Nodes prepared in a round that a later failure made unschedulable are released
    /// again before returning.
    pub fn apply(&mut self, description: &GraphDescription) -> Result<ApplyReport, ApplyError> {
        if self.in_flight {
            return Err(ApplyError::Busy);
        }
        let now = self.clock.now();
        let mut events: Vec<RuntimeEvent> = Vec::new();
        let mut working: Option<GraphDescription> = None;
        let mut failed: Vec<(NodeId, JoinError)> = Vec::new();

        // Restart bookkeeping. A node the scene no longer wants is no longer failing:
        // switching it off and on again is a retry.
        let unwanted: Vec<NodeId> = self
            .faults
            .nodes()
            .filter(|&id| {
                !self.running.contains_key(&id)
                    && description.node(id).map(|e| e.state) != Some(Lifecycle::Active)
            })
            .collect();
        for id in unwanted {
            self.faults.clear(id);
        }
        let running = &self.running;
        self.faults
            .settle(now, |id| running.contains_key(&id), &mut events);
        // Keep out nodes that wait for a restart, and take out scheduled nodes that
        // failed (bypassed) so they are released before they are restarted.
        for entry in description.nodes() {
            if entry.state != Lifecycle::Active {
                continue;
            }
            let closed = match self.faults.gate(entry.id, now) {
                Gate::Closed(retry_at) => Some(retry_at),
                Gate::Open if self.running.contains_key(&entry.id) => self
                    .faults
                    .waiting(entry.id)
                    .then(|| self.faults.status(entry.id).and_then(|s| s.retry_at)),
                Gate::Open => None,
            };
            if let Some(retry_at) = closed {
                let graph = working.get_or_insert_with(|| description.clone());
                let _ = graph.set_state(entry.id, Lifecycle::Standby);
                failed.push((entry.id, JoinError::BackingOff { retry_at }));
            }
        }

        let mut prepared: HashMap<NodeId, Box<dyn Node<X>>> = HashMap::new();
        let plan = loop {
            let plan = working.as_ref().unwrap_or(description).plan();
            if let Some(link) = plan.links.iter().find(|l| l.kind == LinkKind::Audio) {
                let link = *link;
                self.release_all(prepared);
                return Err(ApplyError::UnsupportedLink(link));
            }
            let mut newly_failed = Vec::new();
            for &id in &plan.order {
                if self.running.contains_key(&id) || prepared.contains_key(&id) {
                    continue;
                }
                let graph = working.as_ref().unwrap_or(description);
                match self.idle.remove(&id) {
                    None => newly_failed.push((id, JoinError::NoInstance)),
                    Some(mut node) => {
                        let wanted = graph.node(id).map(|e| &*e.descriptor);
                        if wanted != Some(node.descriptor()) {
                            self.idle.insert(id, node);
                            newly_failed.push((id, JoinError::DescriptorMismatch));
                            continue;
                        }
                        let mut ctx = PrepareCtx::<X> {
                            node: id,
                            shared: &self.shared,
                        };
                        let result = catch_unwind(AssertUnwindSafe(|| node.prepare(&mut ctx)));
                        match result {
                            Ok(Ok(())) => {
                                prepared.insert(id, node);
                            }
                            Ok(Err(_)) | Err(_) => {
                                // Same handling for an error and a panic: return what
                                // a partial prepare took, keep the instance idle.
                                let _ = catch_unwind(AssertUnwindSafe(|| node.release(&mut ctx)));
                                self.idle.insert(id, node);
                                let error = match result {
                                    Ok(Err(e)) => JoinError::Prepare(e),
                                    _ => JoinError::Panicked,
                                };
                                let permanent =
                                    matches!(error, JoinError::Prepare(NodeError::Permanent(_)));
                                events.push(self.faults.fail(
                                    id,
                                    FaultCause::Join(error.clone()),
                                    permanent,
                                    now,
                                ));
                                newly_failed.push((id, error));
                            }
                        }
                    }
                }
            }
            if newly_failed.is_empty() {
                break plan;
            }
            let graph = working.get_or_insert_with(|| description.clone());
            for (id, _) in &newly_failed {
                // Active -> Standby is always allowed; the node exists (it is in the plan).
                let _ = graph.set_state(*id, Lifecycle::Standby);
            }
            failed.extend(newly_failed);
        };

        // Prepared nodes a later round made unschedulable go back to idle.
        let scheduled: std::collections::HashSet<NodeId> = plan.order.iter().copied().collect();
        let unused: Vec<NodeId> = prepared
            .keys()
            .copied()
            .filter(|id| !scheduled.contains(id))
            .collect();
        for id in unused {
            if let Some(node) = prepared.remove(&id) {
                self.release_one(id, node);
            }
        }

        let mut joined: Vec<NodeId> = prepared.keys().copied().collect();
        joined.sort();
        let graph = working.as_ref().unwrap_or(description);
        let schedule = self.build(graph, &plan, &mut prepared);
        let placements = schedule
            .nodes
            .iter()
            .enumerate()
            .map(|(i, e)| {
                (
                    e.id,
                    Placement {
                        index: i as u32,
                        outputs: e.outputs,
                    },
                )
            })
            .collect();
        match self.outbox.try_send(schedule) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(s))
            | Err(std::sync::mpsc::TrySendError::Disconnected(s)) => {
                // Full cannot happen while not in flight; treat both as a lost executor
                // and give the prepared nodes back.
                self.reclaim(s);
                return Err(ApplyError::ExecutorGone);
            }
        }
        self.running = placements;
        self.in_flight = true;
        for id in joined {
            if let Some(event) = self.faults.rejoined(id, now) {
                events.push(event);
            }
        }
        failed.sort_by_key(|(id, _)| *id);
        Ok(ApplyReport {
            plan,
            failed,
            events,
        })
    }

    /// Takes back the schedule the executor swapped out, if it has; calls `release` on
    /// nodes that left the schedule and keeps them idle. `Ok(None)` means the executor
    /// has not swapped yet.
    pub fn collect(&mut self) -> Result<Option<Collected>, ControlError> {
        match self.inbox.try_recv() {
            Ok(retired) => Ok(Some(self.absorb(retired))),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(ControlError::ExecutorGone),
        }
    }

    /// Like [`Controller::collect`], waiting up to `timeout` for the executor's tick.
    pub fn collect_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<Collected>, ControlError> {
        match self.inbox.recv_timeout(timeout) {
            Ok(retired) => Ok(Some(self.absorb(retired))),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(ControlError::ExecutorGone),
        }
    }

    fn absorb(&mut self, retired: Retired<X>) -> Collected {
        self.in_flight = false;
        self.bypasses_seen = retired.bypasses;
        let now = self.clock.now();
        let mut collected = Collected::default();
        for entry in retired.schedule.nodes {
            collected.health.push((entry.id, entry.health));
            // Count a bypass once: the record waits until the node is restarted.
            if entry.health.bypassed && !self.faults.waiting(entry.id) {
                let permanent = entry.health.permanent;
                collected.events.push(self.faults.fail(
                    entry.id,
                    FaultCause::Bypassed(entry.health),
                    permanent,
                    now,
                ));
            }
            if let Some(node) = entry.node {
                collected.left.push((entry.id, entry.health));
                self.release_one(entry.id, node);
            }
        }
        let running = &self.running;
        self.faults
            .settle(now, |id| running.contains_key(&id), &mut collected.events);
        // The old slots (and any frames still in them) are dropped here, off the
        // real-time thread.
        collected
    }

    /// Sends an empty schedule, so that after the executor's next tick every node is
    /// cooled and handed back; [`Controller::collect`] then releases them. Call this
    /// (and collect) before dropping the executor: dropping a running executor cools
    /// its nodes but never releases them.
    pub fn drain(&mut self) -> Result<ApplyReport, ApplyError> {
        self.apply(&GraphDescription::new())
    }

    fn release_one(&mut self, id: NodeId, mut node: Box<dyn Node<X>>) {
        let mut ctx = PrepareCtx::<X> {
            node: id,
            shared: &self.shared,
        };
        let _ = catch_unwind(AssertUnwindSafe(|| node.release(&mut ctx)));
        self.idle.insert(id, node);
    }

    fn release_all(&mut self, prepared: HashMap<NodeId, Box<dyn Node<X>>>) {
        for (id, node) in prepared {
            self.release_one(id, node);
        }
    }

    fn reclaim(&mut self, schedule: Schedule<X>) {
        for entry in schedule.nodes {
            if let Some(node) = entry.node {
                self.release_one(entry.id, node);
            }
        }
    }

    fn build(
        &self,
        graph: &GraphDescription,
        plan: &SchedulePlan,
        prepared: &mut HashMap<NodeId, Box<dyn Node<X>>>,
    ) -> Schedule<X> {
        let mut schedule = Schedule::empty();
        let mut first_output: HashMap<NodeId, u32> = HashMap::with_capacity(plan.order.len());
        // Media links per input pin, in description order; O(links) once.
        let mut into: HashMap<(NodeId, usize), Vec<&Link>> = HashMap::new();
        for link in plan.links.iter().filter(|l| l.kind.is_media()) {
            into.entry((link.to.node, link.to.pin))
                .or_default()
                .push(link);
        }
        for (index, &id) in plan.order.iter().enumerate() {
            let Some(entry) = graph.node(id) else {
                continue;
            };
            let descriptor = &entry.descriptor;
            let start = schedule.slots.len() as u32;
            for pin in &descriptor.outputs {
                schedule.slots.push(Slot::for_kind(pin.kind));
            }
            let outputs = Span {
                start,
                end: schedule.slots.len() as u32,
            };
            first_output.insert(id, start);

            let pins_start = schedule.pins.len() as u32;
            for pin in 0..descriptor.inputs.len() {
                let links_start = schedule.links.len() as u32;
                for link in into.get(&(id, pin)).into_iter().flatten() {
                    // Producers precede consumers in `plan.order`, so the producer's
                    // slots are already laid out and have lower indices.
                    if let Some(&producer) = first_output.get(&link.from.node) {
                        schedule.links.push(producer + link.from.pin as u32);
                        schedule.link_sources.push(link.from.node);
                    }
                }
                schedule.pins.push(Span {
                    start: links_start,
                    end: schedule.links.len() as u32,
                });
            }
            let pins = Span {
                start: pins_start,
                end: schedule.pins.len() as u32,
            };

            let node = prepared.remove(&id);
            if node.is_some() {
                schedule.fresh.push(index as u32);
            } else if let Some(old) = self.running.get(&id) {
                schedule.carry_nodes.push((old.index, index as u32));
                for (pin, p) in descriptor.outputs.iter().enumerate() {
                    if p.kind == LinkKind::Video {
                        schedule
                            .carry_slots
                            .push((old.outputs.start + pin as u32, start + pin as u32));
                    }
                }
            }
            schedule.nodes.push(Entry {
                id,
                node,
                outputs,
                pins,
                health: NodeHealth::default(),
                warmed: false,
            });
        }
        schedule
    }
}
