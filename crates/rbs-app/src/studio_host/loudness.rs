//! The loudness meter of the main canvas's mix (ADR-0092 stage 1; control API
//! `loudness.*`, protocol 1.51).
//!
//! Whether it runs is session state: off when the app starts, on after
//! `loudness.set_enabled`, and started again in a new studio (a collection or canvas
//! change) with a fresh measurement. While it is off the studio has no loudness node.

use rbs_api::types as api;

use super::{Host, Refusal};

/// A value the API can carry: `None` for "no data" (negative infinity or NaN).
fn value(v: f32) -> Option<f32> {
    v.is_finite().then_some(v)
}

impl Host {
    /// Starts or stops the loudness meter. Starting a running meter keeps its
    /// measurement.
    pub(super) fn set_loudness_enabled(&mut self, on: bool) -> Result<(), Refusal> {
        if on {
            self.studio
                .start_loudness()
                .map_err(|e| Refusal::Failed(format!("Cannot measure loudness: {e}")))?;
        } else {
            self.studio.stop_loudness();
        }
        self.loudness_on = on;
        Ok(())
    }

    /// Restarts the measurement (integrated loudness, range, maxima).
    pub(super) fn reset_loudness(&mut self) -> Result<(), Refusal> {
        if self.studio.reset_loudness() {
            Ok(())
        } else {
            Err(Refusal::Conflict(
                "The loudness meter is off; turn it on first.".to_owned(),
            ))
        }
    }

    /// Starts the meter in a new studio when it was on.
    pub(super) fn restore_loudness(&mut self) {
        if self.loudness_on {
            // A studio without audio has nothing to measure: `loudness.status` stays
            // enabled with no values, and the next studio with a mixer measures again.
            let _ = self.studio.start_loudness();
        }
    }

    /// `loudness.status`.
    pub(super) fn api_loudness(&self) -> api::LoudnessStatus {
        match self.studio.loudness() {
            Some(l) => api::LoudnessStatus {
                enabled: self.loudness_on,
                momentary: value(l.momentary),
                short_term: value(l.short_term),
                integrated: value(l.integrated),
                range: value(l.range),
                true_peak_max: value(l.true_peak),
                momentary_max: value(l.max_momentary),
                short_term_max: value(l.max_short_term),
            },
            None => api::LoudnessStatus {
                enabled: self.loudness_on,
                ..api::LoudnessStatus::default()
            },
        }
    }
}
