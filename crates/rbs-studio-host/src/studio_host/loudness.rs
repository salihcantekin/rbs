//! The loudness meter of the main canvas's mix (ADR-0092 stage 1; control API
//! `loudness.*`, protocol 1.47).
//!
//! Whether it runs is saved (Show loudness, `loudness.toml`, protocol 1.59): it starts
//! with the app when it was on, after `loudness.set_enabled`, and again in a new studio
//! (a collection or canvas change), each time with a fresh measurement. While it is off
//! the studio has no loudness node. The target and the true-peak limit are saved in the
//! same file (`settings.set` `loudness`).

use rbs_api::types as api;

use super::{Host, LoudnessView, Refusal};
use crate::loudness_settings::{self, LoudnessSettings};

/// A value the API can carry: `None` for "no data" (negative infinity or NaN).
fn value(v: f32) -> Option<f32> {
    v.is_finite().then_some(v)
}

impl Host {
    /// Starts or stops the loudness meter and saves the choice. Starting a running
    /// meter keeps its measurement. The choice is saved first: a failed save changes
    /// nothing, and a meter that cannot start puts the old file back.
    pub(super) fn set_loudness_enabled(&mut self, on: bool) -> Result<(), Refusal> {
        let changed = self.loudness_state.loudness_on != on;
        if changed {
            self.save_loudness(&LoudnessSettings {
                shown: on,
                ..self.loudness_state.loudness_settings
            })?;
        }
        if on {
            if let Err(e) = self.studio.start_loudness() {
                if changed {
                    self.undo_loudness_save();
                }
                return Err(Refusal::Failed(format!("Cannot measure loudness: {e}")));
            }
        } else {
            self.studio.stop_loudness();
        }
        self.loudness_state.loudness_on = on;
        Ok(())
    }

    /// Saves a new target and true-peak limit (rounded to a tenth) without applying
    /// them; the caller checked them (`loudness_settings::problem`). `None` when they
    /// do not change. A failed save changes nothing.
    pub(super) fn save_loudness_settings(
        &self,
        target: f32,
        true_peak_limit: f32,
    ) -> Result<Option<LoudnessSettings>, Refusal> {
        let new = LoudnessSettings {
            target: loudness_settings::tenth(target),
            true_peak_limit: loudness_settings::tenth(true_peak_limit),
            shown: self.loudness_state.loudness_on,
            ..self.loudness_state.loudness_settings
        };
        if new.target == self.loudness_state.loudness_settings.target
            && new.true_peak_limit == self.loudness_state.loudness_settings.true_peak_limit
        {
            return Ok(None);
        }
        self.save_loudness(&new)?;
        Ok(Some(new))
    }

    /// Applies settings that [`Self::save_loudness_settings`] saved.
    pub(super) fn apply_loudness_settings(&mut self, saved: LoudnessSettings) {
        self.loudness_state.loudness_settings.target = saved.target;
        self.loudness_state.loudness_settings.true_peak_limit = saved.true_peak_limit;
        self.settings_changed();
    }

    /// Puts back the file of the settings in effect, after a save whose change could
    /// not be applied. Best effort: the earlier error is the one reported.
    pub(super) fn undo_loudness_save(&self) {
        let _ = self.save_loudness(&LoudnessSettings {
            shown: self.loudness_state.loudness_on,
            ..self.loudness_state.loudness_settings
        });
    }

    /// Writes `loudness.toml`; without a config folder (tests, a headless host) the
    /// settings are kept in memory only.
    fn save_loudness(&self, settings: &LoudnessSettings) -> Result<(), Refusal> {
        let Some(dir) = &self.config_dir else {
            return Ok(());
        };
        loudness_settings::save(dir, settings)
            .map_err(|e| Refusal::Failed(format!("The loudness settings were not saved: {e}")))
    }

    /// What the Mixer and Settings show ([`super::Update::Loudness`]).
    pub(super) fn loudness_view(&self) -> LoudnessView {
        LoudnessView {
            shown: self.loudness_state.loudness_on,
            readout: self.studio.loudness_readout().cloned(),
            target: self.loudness_state.loudness_settings.target,
            true_peak_limit: self.loudness_state.loudness_settings.true_peak_limit,
        }
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
        if self.loudness_state.loudness_on {
            // A studio without audio has nothing to measure: `loudness.status` stays
            // enabled with no values, and the next studio with a mixer measures again.
            let _ = self.studio.start_loudness();
        }
    }

    /// `loudness.status`.
    pub(super) fn api_loudness(&self) -> api::LoudnessStatus {
        match self.studio.loudness() {
            Some(l) => api::LoudnessStatus {
                enabled: self.loudness_state.loudness_on,
                momentary: value(l.momentary),
                short_term: value(l.short_term),
                integrated: value(l.integrated),
                range: value(l.range),
                true_peak_max: value(l.true_peak),
                momentary_max: value(l.max_momentary),
                short_term_max: value(l.max_short_term),
                target: Some(self.loudness_state.loudness_settings.target),
                true_peak_limit: Some(self.loudness_state.loudness_settings.true_peak_limit),
            },
            None => api::LoudnessStatus {
                enabled: self.loudness_state.loudness_on,
                target: Some(self.loudness_state.loudness_settings.target),
                true_peak_limit: Some(self.loudness_state.loudness_settings.true_peak_limit),
                ..api::LoudnessStatus::default()
            },
        }
    }
}

/// The loudness meter: shown or not, its settings and what was sent.
pub(super) struct LoudnessState {
    /// The main canvas's loudness meter runs (`loudness.set_enabled`), with its saved
    /// target and limit (ADR-0092; `loudness.toml`, where `shown` is `loudness_on`).
    pub(super) loudness_on: bool,
    pub(super) loudness_settings: crate::loudness_settings::LoudnessSettings,
    /// What the UI was last told of the loudness meter ([`Update::Loudness`]).
    pub(super) loudness_sent: Option<LoudnessView>,
}

impl LoudnessState {
    pub(super) fn new(loudness_settings: crate::loudness_settings::LoudnessSettings) -> Self {
        Self {
            loudness_on: loudness_settings.shown,
            loudness_settings,
            loudness_sent: None,
        }
    }
}
