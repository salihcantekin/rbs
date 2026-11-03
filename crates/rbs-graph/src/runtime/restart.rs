//! Restart with back-off for nodes that failed, and the events that report it
//! (principle P7, ADR-0024).
//!
//! The controller keeps one [`Fault`] record per node that failed: a `prepare` that
//! returned an error or panicked, or a node the executor bypassed (too many failed
//! ticks, a panic, a failed `warm`). While a record waits, the node is kept out of
//! every plan, so a `prepare` that blocks is not retried on every edit. When the wait
//! is over, the next `apply` prepares the node again. A node that then runs without
//! being bypassed for [`RestartPolicy::stable_after`] is forgiven: its record is
//! removed and the next failure starts the back-off from the beginning.

use std::collections::HashMap;
use std::time::Duration;

use rbs_core::time::Timestamp;

use crate::description::NodeId;

use super::controller::JoinError;
use super::executor::NodeHealth;

/// How long to wait before restarting a failed node, and when to give up.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Wait after the first failure.
    pub initial: Duration,
    /// Upper bound for the wait; it doubles after each failure until it gets here.
    pub max: Duration,
    /// Failures in a row after which the node is not restarted automatically, or
    /// `None` to keep trying at `max` intervals.
    pub max_attempts: Option<u32>,
    /// Running this long after a restart without being bypassed forgives earlier
    /// failures.
    pub stable_after: Duration,
}

impl Default for RestartPolicy {
    /// 0.5 s, doubling up to 30 s, never giving up, forgiven after 10 s of clean running.
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(500),
            max: Duration::from_secs(30),
            max_attempts: None,
            stable_after: Duration::from_secs(10),
        }
    }
}

impl RestartPolicy {
    /// A policy with the given first wait and upper bound, and the defaults otherwise.
    pub fn backoff(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max: max.max(initial),
            ..Self::default()
        }
    }

    /// Gives up after `attempts` failures in a row (at least 1).
    pub fn give_up_after(mut self, attempts: u32) -> Self {
        self.max_attempts = Some(attempts.max(1));
        self
    }

    /// Forgives failures after running cleanly for `period`.
    pub fn stable_after(mut self, period: Duration) -> Self {
        self.stable_after = period;
        self
    }

    /// The wait after failure number `attempt` (1-based).
    pub fn delay(&self, attempt: u32) -> Duration {
        let doublings = attempt.saturating_sub(1).min(32);
        self.initial
            .checked_mul(1_u32 << doublings.min(31))
            .unwrap_or(self.max)
            .min(self.max)
    }
}

/// Why a node failed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FaultCause {
    /// It could not join: `prepare` failed or panicked.
    Join(JoinError),
    /// The executor bypassed it; the counters say why (errors in a row, a panic, a
    /// failed `warm`, a permanent error).
    Bypassed(NodeHealth),
}

/// Something the control side reports about node failures and restarts. Collected in
/// [`ApplyReport::events`](super::ApplyReport::events) and
/// [`Collected::events`](super::Collected::events) for the engine's event stream.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    /// A node failed. It is kept out of the schedule until `retry_at`, or, when
    /// `retry_at` is `None`, until the user retries it (the policy gave up or the
    /// error was permanent).
    Failed {
        /// The node.
        node: NodeId,
        /// Failures in a row, including this one.
        attempt: u32,
        /// What happened.
        cause: FaultCause,
        /// When the next `apply` may prepare it again.
        retry_at: Option<Timestamp>,
    },
    /// A failed node was prepared again and is in the schedule just sent.
    Restarted {
        /// The node.
        node: NodeId,
        /// Failures in a row before this restart.
        attempt: u32,
    },
    /// A restarted node ran cleanly for `stable_after`; earlier failures are forgiven.
    Recovered {
        /// The node.
        node: NodeId,
    },
}

/// The failure record of one node.
#[derive(Debug, Clone)]
pub(crate) struct Fault {
    attempts: u32,
    /// `None`: not restarted automatically.
    retry_at: Option<Timestamp>,
    /// Set when the node joined again after the failure; cleared by a new failure.
    rejoined_at: Option<Timestamp>,
}

/// All failure records of a controller.
#[derive(Debug, Default)]
pub(crate) struct Faults {
    policy: RestartPolicy,
    records: HashMap<NodeId, Fault>,
}

/// Whether a node with a record may be prepared now.
pub(crate) enum Gate {
    Open,
    /// Waiting until the given time, or for the user (`None`).
    Closed(Option<Timestamp>),
}

impl Faults {
    pub(crate) fn policy(&self) -> RestartPolicy {
        self.policy
    }

    pub(crate) fn set_policy(&mut self, policy: RestartPolicy) {
        self.policy = policy;
    }

    /// Records a failure; `permanent` skips the back-off and waits for the user.
    pub(crate) fn fail(
        &mut self,
        node: NodeId,
        cause: FaultCause,
        permanent: bool,
        now: Timestamp,
    ) -> RuntimeEvent {
        let policy = self.policy;
        let record = self.records.entry(node).or_insert(Fault {
            attempts: 0,
            retry_at: None,
            rejoined_at: None,
        });
        record.attempts = record.attempts.saturating_add(1);
        record.rejoined_at = None;
        let gave_up = permanent
            || policy
                .max_attempts
                .is_some_and(|limit| record.attempts >= limit);
        record.retry_at = if gave_up {
            None
        } else {
            Some(now + policy.delay(record.attempts))
        };
        RuntimeEvent::Failed {
            node,
            attempt: record.attempts,
            cause,
            retry_at: record.retry_at,
        }
    }

    /// Whether `node` may be prepared at `now`. A node without a record, or whose
    /// record has been restarted already, is open.
    pub(crate) fn gate(&self, node: NodeId, now: Timestamp) -> Gate {
        match self.records.get(&node) {
            None => Gate::Open,
            Some(r) if r.rejoined_at.is_some() => Gate::Open,
            Some(r) => match r.retry_at {
                Some(at) if at <= now => Gate::Open,
                other => Gate::Closed(other),
            },
        }
    }

    /// True if `node` failed and has not been prepared again since.
    pub(crate) fn waiting(&self, node: NodeId) -> bool {
        self.records
            .get(&node)
            .is_some_and(|r| r.rejoined_at.is_none())
    }

    /// Marks that a failed node joined again; returns the restart event.
    pub(crate) fn rejoined(&mut self, node: NodeId, now: Timestamp) -> Option<RuntimeEvent> {
        let record = self.records.get_mut(&node)?;
        if record.rejoined_at.is_some() {
            return None;
        }
        record.rejoined_at = Some(now);
        Some(RuntimeEvent::Restarted {
            node,
            attempt: record.attempts,
        })
    }

    /// Forgives nodes that have run cleanly long enough; `running` says whether a node
    /// is in the schedule.
    pub(crate) fn settle(
        &mut self,
        now: Timestamp,
        running: impl Fn(NodeId) -> bool,
        events: &mut Vec<RuntimeEvent>,
    ) {
        let stable = self.policy.stable_after;
        let mut forgiven: Vec<NodeId> = self
            .records
            .iter()
            .filter(|(id, r)| {
                r.rejoined_at
                    .is_some_and(|at| now.saturating_since(at) >= stable)
                    && running(**id)
            })
            .map(|(id, _)| *id)
            .collect();
        forgiven.sort();
        for node in forgiven {
            self.records.remove(&node);
            events.push(RuntimeEvent::Recovered { node });
        }
    }

    /// Forgets the record of `node`; returns whether there was one.
    pub(crate) fn clear(&mut self, node: NodeId) -> bool {
        self.records.remove(&node).is_some()
    }

    /// The earliest time a waiting node may be retried.
    pub(crate) fn next_retry(&self) -> Option<Timestamp> {
        self.records
            .values()
            .filter(|r| r.rejoined_at.is_none())
            .filter_map(|r| r.retry_at)
            .min()
    }

    /// Records, for inspection.
    pub(crate) fn status(&self, node: NodeId) -> Option<FaultStatus> {
        self.records.get(&node).map(|r| FaultStatus {
            attempts: r.attempts,
            retry_at: r.retry_at,
            waiting: r.rejoined_at.is_none(),
        })
    }

    pub(crate) fn nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.records.keys().copied()
    }
}

/// The failure state of one node, from [`Controller::fault`](super::Controller::fault).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultStatus {
    /// Failures in a row.
    pub attempts: u32,
    /// When it may be prepared again; `None` if it is not restarted automatically.
    pub retry_at: Option<Timestamp>,
    /// True while it is kept out of the schedule; false once it was restarted and is
    /// waiting to be forgiven.
    pub waiting: bool,
}
