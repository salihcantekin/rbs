//! Linger in the running studio (ADR-0152 decisions 3 and 8): the binder counts it on
//! the engine clock; the host's control loop arms one wake for the soonest end and
//! updates the studio when it is due.

use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_scene::model::SourceId;

use super::Studio;

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
