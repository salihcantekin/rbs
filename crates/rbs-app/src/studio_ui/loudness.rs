//! The loudness meter in the UI (ADR-0092 decision 7): the Master strip's loudness bar,
//! its number, the true-peak dot and the details, and Settings › Audio › Loudness.
//! Both read one [`LoudnessUi`], filled from the host's [`Update::Loudness`]; the
//! values come from the running meter's lock-free readout at the Mixer's refresh rate,
//! and only while Show loudness is on (P13).

use std::rc::Rc;

use rbs_app::loudness_settings::{self, PRESETS};
use rbs_app::studio_host::{Command, LoudnessView, Update};
use rbs_audio::Loudness;
use rbs_ui::meter;
use slint::{Model, VecModel};

use super::{StudioUi, strings};
use crate::ui::{LoudnessData, Studio};

/// What the UI keeps of the loudness meter.
#[derive(Default)]
pub struct LoudnessUi {
    /// The host's last [`Update::Loudness`].
    view: Option<LoudnessView>,
    /// The Mixer's one row while Show loudness is on, else none.
    pub(super) rows: Rc<VecModel<LoudnessData>>,
    /// Custom was picked in Settings while the target is a preset's value: the
    /// Custom target row stays shown until another preset is picked.
    custom: bool,
}

/// `v` with one decimal and a real minus sign ("−14.2").
fn number(v: f32) -> String {
    let t = format!("{v:.1}");
    match t.strip_prefix('-') {
        Some(rest) if rest != "0.0" => format!("\u{2212}{rest}"),
        Some(rest) => rest.to_owned(),
        None => t,
    }
}

/// A value with its unit, or empty while it has none.
fn with_unit(v: f32, unit: &str) -> String {
    if v.is_finite() {
        format!("{} {unit}", number(v))
    } else {
        String::new()
    }
}

/// The Mixer's row for these values.
pub fn row(l: &Loudness, target: f32, limit: f32) -> LoudnessData {
    let has = l.short_term.is_finite();
    let preset = loudness_settings::preset_of(target).map(|i| {
        PRESETS[i]
            .label
            .split(" (")
            .next()
            .unwrap_or_default()
            .to_owned()
    });
    LoudnessData {
        on: true,
        has,
        short_text: if has {
            number(l.short_term)
        } else {
            String::new()
        }
        .into(),
        at: if has {
            meter::position(l.short_term)
        } else {
            0.0
        },
        target_at: meter::position(target),
        target_text: match preset {
            Some(name) => format!("{} ({name})", with_unit(target, "LUFS")),
            None => with_unit(target, "LUFS"),
        }
        .into(),
        over: l.true_peak.is_finite() && l.true_peak > limit,
        momentary: with_unit(l.momentary, "LUFS").into(),
        integrated: with_unit(l.integrated, "LUFS").into(),
        range: with_unit(l.range, "LU").into(),
        true_peak: with_unit(l.true_peak, "dBTP").into(),
        limit_text: with_unit(limit, "dBTP").into(),
    }
}

impl StudioUi {
    /// Takes the host's loudness update. Returns true when it was one.
    pub(super) fn loudness_update(&mut self, u: &Update) -> bool {
        let Update::Loudness(v) = u else {
            return false;
        };
        self.loudness.view = Some(v.clone());
        if !v.shown {
            self.loudness.rows.set_vec(Vec::new());
        } else {
            self.tick_loudness();
        }
        true
    }

    /// One refresh of the Mixer's loudness row (with the meters, while the Mixer
    /// shows): reads the meter's values; the row changes only when what it shows does.
    pub(super) fn tick_loudness(&mut self) {
        let Some(v) = &self.loudness.view else {
            return;
        };
        if !v.shown {
            return;
        }
        let values = v.readout.as_ref().map_or(Loudness::NONE, |r| r.get());
        let row = row(&values, v.target, v.true_peak_limit);
        match self.loudness.rows.row_data(0) {
            Some(old) if old == row => {}
            Some(_) => self.loudness.rows.set_row_data(0, row),
            None => self.loudness.rows.set_vec(vec![row]),
        }
    }

    /// Pushes the Settings › Audio › Loudness values into a window's `Studio` global.
    pub(super) fn push_loudness(&self, g: &Studio<'_>) {
        let (target, limit) = self
            .loudness
            .view
            .as_ref()
            .map_or((PRESETS[0].lufs, loudness_settings::DEFAULT_LIMIT), |v| {
                (v.target, v.true_peak_limit)
            });
        g.set_loudness_targets(strings(
            PRESETS
                .iter()
                .map(|p| p.label.to_owned())
                .chain(std::iter::once("Custom".to_owned())),
        ));
        let index = match loudness_settings::preset_of(target) {
            Some(i) if !self.loudness.custom => i,
            _ => PRESETS.len(),
        };
        g.set_loudness_target(index as i32);
        g.set_loudness_target_lufs(target);
        g.set_loudness_limit(limit);
    }

    /// Show loudness is on (the host's last word).
    pub fn loudness_shown(&self) -> bool {
        self.loudness.view.as_ref().is_some_and(|v| v.shown)
    }

    /// Show loudness on or off (saved by the host).
    pub fn set_loudness_shown(&mut self, on: bool) {
        self.send(Command::SetLoudnessShown(on));
    }

    /// Restarts the measurement.
    pub fn reset_loudness(&mut self) {
        self.send(Command::ResetLoudness);
    }

    fn send_loudness(&mut self, target: f32, limit: f32) {
        if loudness_settings::problem(target, limit).is_none() {
            self.send(Command::SetLoudnessSettings {
                target,
                true_peak_limit: limit,
            });
        }
    }

    fn loudness_now(&self) -> (f32, f32) {
        self.loudness
            .view
            .as_ref()
            .map_or((PRESETS[0].lufs, loudness_settings::DEFAULT_LIMIT), |v| {
                (v.target, v.true_peak_limit)
            })
    }

    /// Settings' Target choice: a preset, or Custom (the last), which keeps the value.
    pub fn set_loudness_target(&mut self, index: i32) {
        let (_, limit) = self.loudness_now();
        match usize::try_from(index).ok().and_then(|i| PRESETS.get(i)) {
            Some(p) => {
                self.loudness.custom = false;
                self.send_loudness(p.lufs, limit);
            }
            None => self.loudness.custom = true,
        }
    }

    /// Settings' Custom target, LUFS (clamped to the range the settings take).
    pub fn set_loudness_target_lufs(&mut self, lufs: f32) {
        let (lo, hi) = loudness_settings::TARGET_RANGE;
        let (_, limit) = self.loudness_now();
        self.send_loudness(lufs.clamp(lo, hi), limit);
    }

    /// Settings' True-peak limit, dBTP (clamped to the range the settings take).
    pub fn set_loudness_limit(&mut self, dbtp: f32) {
        let (lo, hi) = loudness_settings::LIMIT_RANGE;
        let (target, _) = self.loudness_now();
        self.send_loudness(target, dbtp.clamp(lo, hi));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_use_a_real_minus_and_no_value_stays_empty() {
        assert_eq!(number(-14.24), "\u{2212}14.2");
        assert_eq!(number(-0.01), "0.0");
        assert_eq!(number(2.0), "2.0");
        assert_eq!(with_unit(f32::NEG_INFINITY, "LUFS"), "");
    }

    #[test]
    fn the_row_shows_the_short_term_value_the_target_and_an_over() {
        let mut l = Loudness::NONE;
        let empty = row(&l, -14.0, -1.0);
        assert!(empty.on && !empty.has && !empty.over);
        assert_eq!(empty.short_text, "");
        assert_eq!(empty.target_text, "\u{2212}14.0 LUFS (Streaming)");
        l.short_term = -15.2;
        l.true_peak = -0.6;
        let r = row(&l, -14.0, -1.0);
        assert!(r.has && r.over);
        assert_eq!(r.short_text, "\u{2212}15.2");
        assert!((r.at - meter::position(-15.2)).abs() < 1e-6);
        assert!((r.target_at - meter::position(-14.0)).abs() < 1e-6);
        assert_eq!(r.true_peak, "\u{2212}0.6 dBTP");
        assert!(!row(&l, -14.0, 0.0).over);
        assert_eq!(row(&l, -18.0, -1.0).target_text, "\u{2212}18.0 LUFS");
    }
}
