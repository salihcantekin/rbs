//! The control side: owns idle node instances, prepares joining nodes and builds
//! schedules for the executor.

use std::collections::HashMap;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::time::Duration;

use rbs_core::lifecycle::Lifecycle;

use crate::description::{GraphDescription, Link, NodeId, SchedulePlan};
use crate::node::LinkKind;

use super::executor::{Executor, NodeHealth};
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
    let controller = Controller {
        shared: Arc::clone(&shared),
        idle: HashMap::new(),
        running: HashMap::new(),
        outbox: to_exec,
        inbox: from_exec,
        in_flight: false,
    };
    (controller, Executor::new(shared, local, from_ctrl, to_ctrl))
}

/// A schedule the executor swapped out, on its way back to the controller.
pub struct Retired<X: Runtime> {
    schedule: Schedule<X>,
}

impl<X: Runtime> Retired<X> {
    pub(crate) fn new(schedule: Schedule<X>) -> Self {
        Self { schedule }
    }
}

/// What the controller did with a description.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// The plan that was sent. Nodes whose `prepare` failed were left out and the plan
    /// recomputed without them, so this can differ from `description.plan()`.
    pub plan: SchedulePlan,
    /// Nodes the description wants Active that could not join, and why.
    pub failed: Vec<(NodeId, NodeError)>,
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
        let mut working: Option<GraphDescription> = None;
        let mut failed: Vec<(NodeId, NodeError)> = Vec::new();
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
                    None => newly_failed.push((id, NodeError::NoInstance)),
                    Some(mut node) => {
                        let wanted = graph.node(id).map(|e| &*e.descriptor);
                        if wanted != Some(node.descriptor()) {
                            self.idle.insert(id, node);
                            newly_failed.push((id, NodeError::DescriptorMismatch));
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
                            Ok(Err(e)) => {
                                let _ = catch_unwind(AssertUnwindSafe(|| node.release(&mut ctx)));
                                self.idle.insert(id, node);
                                newly_failed.push((id, e));
                            }
                            Err(_) => {
                                self.idle.insert(id, node);
                                newly_failed.push((id, NodeError::Panicked));
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
        failed.sort_by_key(|(id, _)| *id);
        Ok(ApplyReport { plan, failed })
    }

    /// Takes back the schedule the executor swapped out, if it has; calls `release` on
    /// nodes that left the schedule and keeps them idle. Returns the ids that left,
    /// with their last counters. `Ok(None)` means the executor has not swapped yet.
    pub fn collect(&mut self) -> Result<Option<Vec<(NodeId, NodeHealth)>>, ControlError> {
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
    ) -> Result<Option<Vec<(NodeId, NodeHealth)>>, ControlError> {
        match self.inbox.recv_timeout(timeout) {
            Ok(retired) => Ok(Some(self.absorb(retired))),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(ControlError::ExecutorGone),
        }
    }

    fn absorb(&mut self, retired: Retired<X>) -> Vec<(NodeId, NodeHealth)> {
        self.in_flight = false;
        let mut left = Vec::new();
        for entry in retired.schedule.nodes {
            if let Some(node) = entry.node {
                left.push((entry.id, entry.health));
                self.release_one(entry.id, node);
            }
        }
        // The old slots (and any frames still in them) are dropped here, off the
        // real-time thread.
        left
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
                for link in plan
                    .links
                    .iter()
                    .filter(|l| l.to.node == id && l.to.pin == pin && l.kind.is_media())
                {
                    // Producers precede consumers in `plan.order`, so the producer's
                    // slots are already laid out and have lower indices.
                    if let Some(&producer) = first_output.get(&link.from.node) {
                        schedule.links.push(producer + link.from.pin as u32);
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
            });
        }
        schedule
    }
}
