//! Linger in the running studio (ADR-0152 decisions 3 and 8): the binder counts it on
//! the engine clock; the host's control loop arms one wake for the soonest end and
//! updates the studio when it is due.

use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_scene::binder::SceneBinder;
use rbs_scene::model::SourceId;
use rbs_sources::SourceTypes;

use super::{Engine, ScrollHandover, Studio};

/// The studio's binder on the engine clock, with the lingers of the studio it
/// replaces moved to that clock by the signed difference (ADR-0152 decision 8).
pub(super) fn binder(
    sources: &SourceTypes,
    engine: &Engine,
    handover: Option<&ScrollHandover>,
) -> SceneBinder {
    let clock = engine.clock();
    let now = clock.now();
    let binder = SceneBinder::new(sources.node_types()).with_clock(clock);
    let Some(h) = handover.filter(|h| !h.lingers.is_empty()) else {
        return binder;
    };
    let shift = i128::from(now.as_nanos()) - i128::from(h.at.as_nanos());
    let moved = h.lingers.iter().map(|(id, end)| {
        let end = i128::from(end.as_nanos()) + shift;
        (
            *id,
            Timestamp::from_nanos(u64::try_from(end.max(0)).unwrap_or(u64::MAX)),
        )
    });
    binder.with_lingers(moved.collect())
}

impl Studio {
    /// How long the control thread may sleep before the soonest linger ends; zero
    /// when it is due. `None` while no source lingers, so nothing is armed (P13).
    pub fn linger_wait(&self, now: Timestamp) -> Option<Duration> {
        self.binder.linger_wait(now)
    }

    /// True when a linger has ended by `now`: the host updates the studio, and that
    /// sync puts the source to Standby.
    pub fn linger_due(&self, now: Timestamp) -> bool {
        self.binder.linger_end().is_some_and(|end| end <= now)
    }

    /// Whether `source` is Active only because it lingers.
    pub fn is_lingering(&self, source: SourceId) -> bool {
        self.binder.is_lingering(source)
    }
}
