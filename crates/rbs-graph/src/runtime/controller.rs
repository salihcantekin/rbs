//! The control side: owns idle node instances, prepares joining nodes and builds
//! schedules for the executor.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::time::Duration;

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::DEFAULT_BLOCK_FRAMES;
use rbs_core::time::{Clock, MonotonicClock, Timestamp};

use crate::description::{GraphDescription, Link, NodeId, SchedulePlan};
use crate::node::LinkKind;

use super::executor::{Executor, NodeHealth};
use super::restart::{FaultCause, FaultStatus, Faults, Gate, RestartPolicy, RuntimeEvent};
use super::schedule::{Entry, MAX_BLOCK_FRAMES, Schedule, Slot, Span};
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
        audio_frames: DEFAULT_BLOCK_FRAMES,
        shared: Arc::clone(&shared),
        idle: HashMap::new(),
        placed: HashMap::new(),
        epochs: HashMap::new(),
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
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyError::Busy => f.write_str("previous schedule not taken up yet"),
            ApplyError::ExecutorGone => f.write_str("executor is gone"),
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
    /// The scheduled instance's epoch; `None` for a node held without an instance.
    epoch: Option<u64>,
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
    /// tick), including nodes held there without an instance.
    placed: HashMap<NodeId, Placement>,
    outbox: SyncSender<Schedule<X>>,
    inbox: Receiver<Retired<X>>,
    in_flight: bool,
    /// Time for back-off; the production master clock unless replaced.
    clock: Arc<dyn Clock>,
    faults: Faults,
    /// Instance epoch per node: bumped by every successful `prepare` and by clearing a
    /// failure record, so health of an older instance is recognised as stale.
    epochs: HashMap<NodeId, u64>,
    /// Bumped by the executor whenever it bypasses a node, so the control side learns
    /// about it without waiting for an edit.
    bypasses: Arc<AtomicU64>,
    /// The counter value whose bypasses have been collected.
    bypasses_seen: u64,
    /// Frames per channel of each audio output pin's buffer in schedules built next.
    audio_frames: u32,
}

impl<X: Runtime> Controller<X> {
    /// Hands over the instance for node `id`, idle (Standby) until a plan schedules it.
    pub fn adopt(&mut self, id: NodeId, node: Box<dyn Node<X>>) -> Result<(), ControlError> {
        if self.idle.contains_key(&id) || self.is_running(id) {
            return Err(ControlError::AlreadyAdopted(id));
        }
        self.idle.insert(id, node);
        Ok(())
    }

    /// Takes back an idle instance (for example to destroy it: Standby to Installed).
    pub fn take(&mut self, id: NodeId) -> Result<Option<Box<dyn Node<X>>>, ControlError> {
        if self.is_running(id) {
            return Err(ControlError::Running(id));
        }
        Ok(self.idle.remove(&id))
    }

    /// True if node `id` is in the schedule last sent.
    pub fn is_running(&self, id: NodeId) -> bool {
        self.running_epoch(id).is_some()
    }

    /// The epoch of the instance of `id` in the schedule last sent, if it has one.
    fn running_epoch(&self, id: NodeId) -> Option<u64> {
        self.placed.get(&id).and_then(|p| p.epoch)
    }

    fn epoch(&self, id: NodeId) -> u64 {
        self.epochs.get(&id).copied().unwrap_or(0)
    }

    fn bump_epoch(&mut self, id: NodeId) -> u64 {
        let e = self.epochs.entry(id).or_insert(0);
        *e += 1;
        *e
    }

    /// True if the scheduled instance of `id` failed, or was superseded by a retry, and
    /// must leave the schedule before it can be restarted.
    fn must_leave(&self, id: NodeId) -> bool {
        self.running_epoch(id)
            .is_some_and(|e| e != self.epoch(id) || self.faults.waiting(id))
    }

    /// True if node `id` has an idle instance here.
    pub fn is_idle(&self, id: NodeId) -> bool {
        self.idle.contains_key(&id)
    }

    /// Sets the buffer size of every audio output pin in schedules built from now on,
    /// in frames per channel (clamped to `1..=MAX_BLOCK_FRAMES`; default
    /// `DEFAULT_BLOCK_FRAMES`). An audio partition sets its graph's block size here.
    pub fn set_audio_block_frames(&mut self, frames: u32) {
        self.audio_frames = frames.clamp(1, MAX_BLOCK_FRAMES);
    }

    /// Frames per channel of each audio output pin's buffer.
    pub fn audio_block_frames(&self) -> u32 {
        self.audio_frames
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
    ///
    /// Health still reported for the failed instance afterwards is ignored. If that
    /// instance is still scheduled, the next `apply` takes it out and the one after
    /// prepares it again.
    pub fn retry(&mut self, id: NodeId) -> bool {
        let had = self.faults.clear(id);
        if had {
            self.bump_epoch(id);
        }
        had
    }

    /// True if calling `apply` again with the same description would change the
    /// schedule because of restarts: the executor bypassed a node since the last
    /// `collect`, a scheduled node failed and must be taken out, a waiting node's
    /// back-off is over, or a restarted node is due to be forgiven.
    ///
    /// The engine's control loop, on its timer: `collect` (an `apply` is refused while
    /// a schedule is in flight), then if this is true, `apply` the current
    /// description; sleep for at most [`Controller::next_due`].
    pub fn restart_due(&self) -> bool {
        if self.bypasses.load(Ordering::Relaxed) != self.bypasses_seen {
            return true;
        }
        let now = self.clock.now();
        self.placed.iter().any(|(&id, p)| match p.epoch {
            Some(_) => self.must_leave(id),
            // Held without an instance: due once its gate opens.
            None => matches!(self.faults.gate(id, now), Gate::Open),
        }) || self.faults.next_deadline().is_some_and(|at| at <= now)
    }

    /// How long until [`Controller::restart_due`] may turn true without a new bypass:
    /// zero if it is true now, `None` if nothing is scheduled to happen. Measured on
    /// the controller's clock, so the engine can sleep for it directly.
    pub fn next_due(&self) -> Option<Duration> {
        if self.restart_due() {
            return Some(Duration::ZERO);
        }
        let now = self.clock.now();
        self.faults
            .next_deadline()
            .map(|at| at.saturating_since(now))
    }

    /// True while a sent schedule has not come back through [`Controller::collect`].
    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// Plans `description`, prepares the nodes that join, and sends the schedule. The
    /// executor swaps to it after its next tick.
    ///
    /// A node whose `prepare` fails, or that waits for a restart (ADR-0024), stays in
    /// the plan without an instance: its outputs keep their last video frame and its
    /// dependants keep running, so a failure stays contained to that node (P7). A node
    /// with no instance or a different descriptor is a configuration problem: the plan
    /// is recomputed as if it were Standby, so nodes that depended on it are skipped
    /// too, and nodes prepared in a round that this made unschedulable are released
    /// again before returning.
    pub fn apply(&mut self, description: &GraphDescription) -> Result<ApplyReport, ApplyError> {
        if self.in_flight {
            return Err(ApplyError::Busy);
        }
        let now = self.clock.now();
        let mut events: Vec<RuntimeEvent> = Vec::new();
        let mut working: Option<GraphDescription> = None;
        let mut failed: Vec<(NodeId, JoinError)> = Vec::new();

        // A node the scene no longer wants (switched off or removed) is no longer
        // failing: switching it on again prepares it at once.
        let mut unwanted: Vec<NodeId> = self
            .faults
            .nodes()
            .filter(|&id| description.node(id).map(|e| e.state) != Some(Lifecycle::Active))
            .collect();
        unwanted.sort();
        for id in unwanted {
            self.faults.clear(id);
            self.bump_epoch(id);
            events.push(RuntimeEvent::Cleared { node: id });
        }

        // Nodes held in the plan without an instance: waiting for their back-off, or
        // scheduled but failed (or retried) and to be released before a restart.
        let mut hollow: HashSet<NodeId> = HashSet::new();
        for entry in description.nodes() {
            if entry.state != Lifecycle::Active {
                continue;
            }
            let id = entry.id;
            let retry_at = self.faults.status(id).and_then(|f| f.retry_at);
            if self.is_running(id) {
                if self.must_leave(id) {
                    hollow.insert(id);
                    failed.push((id, JoinError::BackingOff { retry_at }));
                }
            } else if let Gate::Closed(retry_at) = self.faults.gate(id, now) {
                hollow.insert(id);
                failed.push((id, JoinError::BackingOff { retry_at }));
            }
        }

        let mut prepared: HashMap<NodeId, Box<dyn Node<X>>> = HashMap::new();
        let plan = loop {
            let plan = working.as_ref().unwrap_or(description).plan();
            let mut newly_failed = Vec::new();
            for &id in &plan.order {
                if hollow.contains(&id) || self.is_running(id) || prepared.contains_key(&id) {
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
                                // a partial prepare took, keep the instance idle, and
                                // hold the node in the plan without it.
                                let _ = catch_unwind(AssertUnwindSafe(|| node.release(&mut ctx)));
                                self.idle.insert(id, node);
                                let error = match result {
                                    Ok(Err(e)) => JoinError::Prepare(e),
                                    _ => JoinError::Panicked,
                                };
                                let permanent = matches!(
                                    &error,
                                    JoinError::Prepare(e) if e.is_permanent()
                                );
                                events.push(self.faults.fail(
                                    id,
                                    FaultCause::Join(error.clone()),
                                    permanent,
                                    None,
                                    now,
                                ));
                                hollow.insert(id);
                                failed.push((id, error));
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

        let scheduled: HashSet<NodeId> = plan.order.iter().copied().collect();
        // A node whose wait is over but that the plan skipped for another reason (no
        // sink, starved) is not due until an edit schedules it.
        let parked: Vec<NodeId> = self
            .faults
            .nodes()
            .filter(|id| !scheduled.contains(id) && self.faults.waiting(*id))
            .collect();
        for id in parked {
            if matches!(self.faults.gate(id, now), Gate::Open) {
                self.faults.park(id);
            }
        }
        // Prepared nodes a later round made unschedulable go back to idle.
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
        let mut fresh_epochs: HashMap<NodeId, u64> = HashMap::with_capacity(joined.len());
        for &id in &joined {
            fresh_epochs.insert(id, self.bump_epoch(id));
        }
        let graph = working.as_ref().unwrap_or(description);
        let schedule = self.build(graph, &plan, &mut prepared, &hollow, &fresh_epochs);
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
                        epoch: e.epoch,
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
        self.placed = placements;
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
            // Health of an older instance (replaced, retried or switched off since) is
            // stale and never recorded.
            let current = entry.epoch.filter(|&e| e == self.epoch(entry.id));
            if let Some(epoch) = current {
                if entry.health.bypassed {
                    // Count a bypass once per instance.
                    if !self.faults.counted(entry.id, epoch) {
                        let permanent = entry.health.permanent;
                        collected.events.push(self.faults.fail(
                            entry.id,
                            FaultCause::Bypassed(entry.health),
                            permanent,
                            Some(epoch),
                            now,
                        ));
                    }
                } else if let Some(event) = self.faults.forgive_if_stable(entry.id, now) {
                    // Ran cleanly up to this swap for `stable_after` since its restart.
                    collected.events.push(event);
                }
            }
            if let Some(node) = entry.node {
                collected.left.push((entry.id, entry.health));
                self.release_one(entry.id, node);
            }
        }
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
        hollow: &HashSet<NodeId>,
        fresh_epochs: &HashMap<NodeId, u64>,
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
                schedule
                    .slots
                    .push(Slot::for_kind(pin.kind, self.audio_frames));
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
            let mut epoch = None;
            if node.is_some() {
                schedule.fresh.push(index as u32);
                epoch = fresh_epochs.get(&id).copied();
            } else if let Some(old) = self.placed.get(&id) {
                // A node held without an instance keeps the last frame of the instance
                // that left (P7), but the instance itself stays behind to be released.
                if !hollow.contains(&id) && old.epoch.is_some() {
                    schedule.carry_nodes.push((old.index, index as u32));
                    epoch = old.epoch;
                }
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
                epoch,
            });
        }
        schedule
    }
}
