//! The real-time side: runs ticks and swaps schedules between ticks.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};

use crate::description::NodeId;

use super::controller::Retired;
use super::schedule::{Inputs, Outputs, Schedule, Slot};
use super::{NodeError, ProcessCtx, Runtime, Tick, WarmCtx};

/// Consecutive failed ticks after which a node is bypassed, unless changed with
/// [`Executor::set_failure_limit`].
pub const DEFAULT_FAILURE_LIMIT: u32 = 8;

/// Counters of one scheduled node, kept by the executor.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeHealth {
    /// Ticks on which `process` returned `Ok`.
    pub processed: u64,
    /// Ticks on which `process` returned an error.
    pub errors: u64,
    /// Failed ticks in a row.
    pub consecutive_errors: u32,
    /// `process` or `warm` panicked.
    pub panicked: bool,
    /// `warm` failed.
    pub warm_failed: bool,
    /// The node is no longer called: it failed too often, panicked, or failed to warm.
    /// Its video outputs keep their last frame; event outputs stay empty.
    pub bypassed: bool,
}

/// Runs a schedule on its own thread: one call to [`Executor::run_tick`] per tick.
///
/// Created together with its [`Controller`](super::Controller) by
/// [`channel`](super::channel). Neither `run_tick` nor the swap it performs allocates
/// (apart from what `Node::warm` and `Node::cool` choose to do), blocks or takes a
/// lock.
pub struct Executor<X: Runtime> {
    shared: Arc<X::Shared>,
    local: X::Local,
    current: Schedule<X>,
    inbox: Receiver<Schedule<X>>,
    outbox: SyncSender<Retired<X>>,
    /// A retired schedule the controller has not taken yet; no new schedule is accepted
    /// until it is delivered.
    pending: Option<Retired<X>>,
    failure_limit: u32,
}

impl<X: Runtime> Executor<X> {
    pub(crate) fn new(
        shared: Arc<X::Shared>,
        local: X::Local,
        inbox: Receiver<Schedule<X>>,
        outbox: SyncSender<Retired<X>>,
    ) -> Self {
        Self {
            shared,
            local,
            current: Schedule::empty(),
            inbox,
            outbox,
            pending: None,
            failure_limit: DEFAULT_FAILURE_LIMIT,
        }
    }

    /// Sets how many failed ticks in a row bypass a node (at least 1).
    pub fn set_failure_limit(&mut self, ticks: u32) {
        self.failure_limit = ticks.max(1);
    }

    /// Resources owned by this thread.
    pub fn local(&mut self) -> &mut X::Local {
        &mut self.local
    }

    /// Ids of the scheduled nodes, in run order.
    pub fn scheduled(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.current.nodes.iter().map(|e| e.id)
    }

    /// Counters of a scheduled node.
    pub fn health(&self, id: NodeId) -> Option<NodeHealth> {
        self.current
            .nodes
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.health)
    }

    /// Runs one tick, then applies a pending schedule so the next tick runs it.
    ///
    /// Order: `Runtime::begin_tick`, `process` of every scheduled node in dependency
    /// order, `Runtime::end_tick`, then the swap (if one is pending): `cool` for nodes
    /// that leave, `warm` for nodes that join. Warm-up therefore happens in the slack
    /// after a tick, never before the nodes of the current tick ran.
    pub fn run_tick(&mut self, tick: Tick) {
        X::begin_tick(&self.shared, &mut self.local, tick);
        self.process_all(tick);
        X::end_tick(&self.shared, &mut self.local, tick);
        self.maintain();
    }

    fn process_all(&mut self, tick: Tick) {
        let schedule = &mut self.current;
        let shared: &X::Shared = &self.shared;
        for entry in schedule.nodes.iter_mut() {
            let start = entry.outputs.start as usize;
            let (before, rest) = schedule.slots.split_at_mut(start);
            let own = &mut rest[..entry.outputs.len()];
            for slot in own.iter_mut() {
                slot.begin_tick();
            }
            if entry.health.bypassed {
                continue;
            }
            let Some(node) = entry.node.as_mut() else {
                continue;
            };
            let mut ctx = ProcessCtx::<X> {
                tick,
                node: entry.id,
                shared,
                local: &mut self.local,
                inputs: Inputs {
                    slots: before,
                    pins: &schedule.pins[entry.pins.range()],
                    links: &schedule.links,
                },
                outputs: Outputs { slots: own },
            };
            let result = catch_unwind(AssertUnwindSafe(|| node.process(&mut ctx)));
            let health = &mut entry.health;
            match result {
                Ok(Ok(())) => {
                    health.processed += 1;
                    health.consecutive_errors = 0;
                }
                Ok(Err(_)) | Err(_) => {
                    health.errors += 1;
                    health.consecutive_errors = health.consecutive_errors.saturating_add(1);
                    if result.is_err() {
                        health.panicked = true;
                    }
                    if health.panicked || health.consecutive_errors >= self.failure_limit {
                        health.bypassed = true;
                    }
                    // Partial events from a failed tick are not passed on.
                    for slot in rest[..entry.outputs.len()].iter_mut() {
                        slot.begin_tick();
                    }
                }
            }
        }
    }

    /// Delivers a retired schedule, then takes and applies a new one if there is one.
    fn maintain(&mut self) {
        if let Some(retired) = self.pending.take() {
            match self.outbox.try_send(retired) {
                Ok(()) => {}
                Err(TrySendError::Full(r)) => {
                    self.pending = Some(r);
                    return;
                }
                // The controller is gone; dropping here is the only option left.
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
        match self.inbox.try_recv() {
            Ok(next) => self.swap(next),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
    }

    fn swap(&mut self, mut next: Schedule<X>) {
        let old = &mut self.current;
        for &(from, to) in &next.carry_nodes {
            let (from, to) = (from as usize, to as usize);
            next.nodes[to].node = old.nodes[from].node.take();
            next.nodes[to].health = old.nodes[from].health;
        }
        for &(from, to) in &next.carry_slots {
            if let (Some(Slot::Video(a)), Some(Slot::Video(b))) = (
                old.slots.get_mut(from as usize),
                next.slots.get_mut(to as usize),
            ) {
                std::mem::swap(a, b);
            }
        }
        // Nodes still in the old schedule are leaving.
        for entry in old.nodes.iter_mut() {
            if let Some(node) = entry.node.as_mut() {
                let mut ctx = WarmCtx::<X> {
                    node: entry.id,
                    shared: &self.shared,
                    local: &mut self.local,
                };
                let _ = catch_unwind(AssertUnwindSafe(|| node.cool(&mut ctx)));
            }
        }
        for &index in &next.fresh {
            let entry = &mut next.nodes[index as usize];
            let Some(node) = entry.node.as_mut() else {
                continue;
            };
            let mut ctx = WarmCtx::<X> {
                node: entry.id,
                shared: &self.shared,
                local: &mut self.local,
            };
            let result = catch_unwind(AssertUnwindSafe(|| node.warm(&mut ctx)));
            let failed = match result {
                Ok(Ok(())) => false,
                Ok(Err(NodeError::Panicked)) | Err(_) => {
                    entry.health.panicked = true;
                    true
                }
                Ok(Err(_)) => true,
            };
            if failed {
                entry.health.warm_failed = true;
                entry.health.bypassed = true;
                let _ = catch_unwind(AssertUnwindSafe(|| node.cool(&mut ctx)));
            }
        }
        let old = std::mem::replace(&mut self.current, next);
        let retired = Retired::new(old);
        match self.outbox.try_send(retired) {
            Ok(()) => {}
            Err(TrySendError::Full(r)) => self.pending = Some(r),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}
