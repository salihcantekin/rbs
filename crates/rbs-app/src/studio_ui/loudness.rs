//! The loudness meter in the UI (ADR-0092 decision 7): the Master strip's loudness bar,
//! its number, the true-peak dot and the details, and Settings › Audio › Loudness.
//! Both read one [`LoudnessUi`], filled from the host's [`Update::Loudness`]; the
//! values come from the running meter's lock-free readout, and only while Show loudness
//! is on (P13). The bar follows the readout at the Mixer's refresh rate, drawn by the
//! native meter layer where one shows (ADR-0035), so it redraws nothing; the numbers
//! change at most once a second (owner, 2026-11-20), since every changed string redraws
//! the whole window. A string is formatted again only when the tenth it shows changes.

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
    /// The strings last formatted, and the tenths they show.
    paint: LoudnessPaint,
}

/// One string kept until the tenth it shows changes.
#[derive(Default)]
struct Held<K: Default> {
    key: K,
    ready: bool,
    text: slint::SharedString,
}

impl<K: Copy + PartialEq + Default> Held<K> {
    /// Formats only when `key` is not the one already shown. The new string replaces
    /// the old one; a repeat keeps the same allocation.
    fn update(&mut self, key: K, format_text: impl FnOnce() -> String) -> bool {
        if self.ready && self.key == key {
            return false;
        }
        self.text = format_text().into();
        self.key = key;
        self.ready = true;
        true
    }
}

/// The tenths the Mixer's loudness strings were formatted from, plus the bar and the
/// true-peak dot, which follow the raw reading and are not tenths.
#[derive(Default)]
struct LoudnessPaint {
    short_text: Held<Option<i32>>,
    momentary: Held<Option<i32>>,
    integrated: Held<Option<i32>>,
    range: Held<Option<i32>>,
    true_peak: Held<Option<i32>>,
    /// The target's tenth and, when it is a preset, that preset's index. The name is
    /// part of the string.
    target_text: Held<(Option<i32>, Option<usize>)>,
    limit_text: Held<Option<i32>>,
    at: f32,
    target_at: f32,
    over: bool,
    has: bool,
}

impl LoudnessPaint {
    /// The row for this reading, or `None` when every shown tenth, the bar, the target
    /// mark and the true-peak dot are unchanged. A steady tenth keeps its string. The
    /// strings are looked at only when `texts` (once a second, or on a settings change)
    /// or before the first ones; the bar is left as it is while `native` (the layer
    /// draws it).
    fn refresh(
        &mut self,
        l: &Loudness,
        target: f32,
        limit: f32,
        texts: bool,
        native: bool,
    ) -> Option<LoudnessData> {
        let (has, at, target_at) = bar(l, target);
        let over = l.true_peak.is_finite() && l.true_peak > limit;
        let preset = loudness_settings::preset_of(target);
        // A value that comes or goes changes what is shown at once ("—" or a number).
        let due = texts || !self.short_text.ready || has != self.has;
        let formatted = due && self.texts(l, target, limit, preset);
        let at = if native { self.at } else { at };
        let same_place = self.at.to_bits() == at.to_bits()
            && self.target_at.to_bits() == target_at.to_bits()
            && self.over == over
            && self.has == has;
        if !formatted && same_place {
            return None;
        }
        self.at = at;
        self.target_at = target_at;
        self.over = over;
        self.has = has;
        Some(LoudnessData {
            on: true,
            has,
            short_text: self.short_text.text.clone(),
            at,
            target_at,
            target_text: self.target_text.text.clone(),
            over,
            momentary: self.momentary.text.clone(),
            integrated: self.integrated.text.clone(),
            range: self.range.text.clone(),
            true_peak: self.true_peak.text.clone(),
            limit_text: self.limit_text.text.clone(),
        })
    }

    /// Formats the strings whose tenth changed; true when any did.
    fn texts(&mut self, l: &Loudness, target: f32, limit: f32, preset: Option<usize>) -> bool {
        let has = l.short_term.is_finite();
        let mut formatted = false;
        formatted |= self.short_text.update(shown_tenth(l.short_term), || {
            if has {
                number(l.short_term)
            } else {
                String::new()
            }
        });
        formatted |= self
            .momentary
            .update(shown_tenth(l.momentary), || with_unit(l.momentary, "LUFS"));
        formatted |= self.integrated.update(shown_tenth(l.integrated), || {
            with_unit(l.integrated, "LUFS")
        });
        formatted |= self
            .range
            .update(shown_tenth(l.range), || with_unit(l.range, "LU"));
        formatted |= self
            .true_peak
            .update(shown_tenth(l.true_peak), || with_unit(l.true_peak, "dBTP"));
        formatted |= self.target_text.update((shown_tenth(target), preset), || {
            target_line(target, preset)
        });
        formatted |= self
            .limit_text
            .update(shown_tenth(limit), || with_unit(limit, "dBTP"));
        formatted
    }
}

/// Whether the reading has a short-term value, the bar's top and the target mark, as
/// positions on the meter scale.
fn bar(l: &Loudness, target: f32) -> (bool, f32, f32) {
    let has = l.short_term.is_finite();
    let at = if has {
        meter::position(l.short_term)
    } else {
        0.0
    };
    (has, at, meter::position(target))
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

/// The tenth [`number`] shows, or `None` when `v` has no number.
///
/// The product is in `f64` so an `f32` multiply does not invent a halfway, and a real
/// halfway rounds to the even tenth, which is what `{v:.1}` does. The meter tick
/// remembers this and formats again only when it changes.
fn shown_tenth(v: f32) -> Option<i32> {
    if !v.is_finite() {
        return None;
    }
    let scaled = f64::from(v) * 10.0;
    let floor = scaled.floor();
    if !(floor >= f64::from(i32::MIN) && floor <= f64::from(i32::MAX)) {
        return Some(if scaled.is_sign_negative() {
            i32::MIN
        } else {
            i32::MAX
        });
    }
    // `floor` is inside `i32`, so the cast keeps it.
    let base = floor as i32;
    let frac = scaled - floor;
    let tenth = if frac > 0.5 || (frac == 0.5 && base.rem_euclid(2) != 0) {
        base.saturating_add(1)
    } else {
        base
    };
    Some(tenth)
}

/// A value with its unit, or empty while it has none.
fn with_unit(v: f32, unit: &str) -> String {
    if v.is_finite() {
        format!("{} {unit}", number(v))
    } else {
        String::new()
    }
}

/// The target as shown: "−14.0 LUFS", or "−14.0 LUFS (Streaming)" for a preset.
fn target_line(target: f32, preset: Option<usize>) -> String {
    let value = with_unit(target, "LUFS");
    match preset.and_then(|i| PRESETS.get(i)) {
        Some(p) => {
            let name = p.label.split(" (").next().unwrap_or_default();
            format!("{value} ({name})")
        }
        None => value,
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
            self.loudness.paint = LoudnessPaint::default();
        } else {
            self.tick_loudness(true);
        }
        true
    }

    /// One refresh of the Mixer's loudness row (with the meters, while the Mixer
    /// shows). The strings are looked at only when `texts` (once a second) and formatted
    /// only when the tenth they show changes; the bar, the target mark and the
    /// true-peak dot follow the raw reading, the bar on the native meter layer where it
    /// shows. The row is replaced only when what it shows changes.
    pub(super) fn tick_loudness(&mut self, texts: bool) {
        let (values, target, limit) = {
            let Some(v) = &self.loudness.view else {
                return;
            };
            if !v.shown {
                return;
            }
            (
                v.readout.as_ref().map_or(Loudness::NONE, |r| r.get()),
                v.target,
                v.true_peak_limit,
            )
        };
        let (_, at, target_at) = bar(&values, target);
        let native = crate::meter_bridge::set_view(
            crate::meter_bridge::LOUDNESS_KEY,
            &meter::MeterView {
                level: at,
                peak: target_at,
                clipped: false,
            },
        );
        let paint = &mut self.loudness.paint;
        let Some(row) = paint.refresh(&values, target, limit, texts, native) else {
            return;
        };
        match self.loudness.rows.row_data(0) {
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
#[allow(clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    /// A freshly formatted row, the same strings the meter shows on a first reading.
    fn row(l: &Loudness, target: f32, limit: f32) -> LoudnessData {
        LoudnessPaint::default()
            .refresh(l, target, limit, true, false)
            .expect("a fresh reading builds a row")
    }

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

    /// The integer `shown_tenth` is the tenth of the string `number` builds.
    fn tenth_in(text: &str) -> i32 {
        let neg = text.starts_with('\u{2212}');
        let text = text.trim_start_matches('\u{2212}');
        let (whole, frac) = text.split_once('.').expect("one decimal");
        let whole: i32 = whole.parse().expect("whole");
        let frac: i32 = frac.parse().expect("tenth");
        let n = whole * 10 + frac;
        if neg { -n } else { n }
    }

    #[test]
    fn the_remembered_tenth_is_the_one_the_string_shows() {
        for v in [
            -14.25f32, -14.24, -14.26, -0.05, -0.04, -0.01, 0.0, -0.0, 1.25, 2.0, -1.25,
            -30.849998, -70.0, 30.0,
        ] {
            assert_eq!(shown_tenth(v), Some(tenth_in(&number(v))), "{v}");
        }
        assert_eq!(shown_tenth(f32::NEG_INFINITY), None);
    }

    #[test]
    fn a_steady_tenth_keeps_the_string_and_a_new_tenth_rebuilds_it() {
        let mut paint = LoudnessPaint::default();
        let mut l = Loudness::NONE;
        l.short_term = -15.24;
        l.momentary = -16.21;
        l.integrated = -14.04;
        l.range = 8.01;
        l.true_peak = -1.04;
        let a = paint
            .refresh(&l, -14.0, -1.0, true, false)
            .expect("the first reading builds a row");
        assert_eq!(a, row(&l, -14.0, -1.0));
        let short = a.short_text.as_str().as_ptr();
        let peak = a.true_peak.as_str().as_ptr();
        let target = a.target_text.as_str().as_ptr();

        // Still −15.2. The bar moves; the strings stay the same allocation.
        l.short_term = -15.21;
        l.momentary = -16.24;
        let b = paint
            .refresh(&l, -14.0, -1.0, true, false)
            .expect("the bar still moves");
        assert_eq!(b, row(&l, -14.0, -1.0));
        assert_eq!(b.short_text.as_str().as_ptr(), short);
        assert_eq!(b.true_peak.as_str().as_ptr(), peak);
        assert_eq!(b.target_text.as_str().as_ptr(), target);
        assert_ne!(b.at.to_bits(), a.at.to_bits());

        // Nothing shown changed: no new row, so nothing is formatted.
        assert!(paint.refresh(&l, -14.0, -1.0, true, false).is_none());

        // −15.3. Short-term is rebuilt. A true peak still on −1.0 is not.
        l.short_term = -15.26;
        let c = paint
            .refresh(&l, -14.0, -1.0, true, false)
            .expect("a new tenth rebuilds");
        assert_eq!(c.short_text.as_str(), "\u{2212}15.3");
        assert_eq!(c, row(&l, -14.0, -1.0));
        assert_ne!(c.short_text.as_str().as_ptr(), short);
        assert_eq!(c.true_peak.as_str().as_ptr(), peak);

        // The dot lights inside the same true-peak tenth (−1.0), without a new string.
        l.true_peak = -0.96;
        let d = paint
            .refresh(&l, -14.0, -1.0, true, false)
            .expect("the dot still lights");
        assert!(d.over);
        assert_eq!(d.true_peak.as_str(), "\u{2212}1.0 dBTP");
        assert_eq!(d.true_peak.as_str().as_ptr(), peak);
        assert_eq!(d, row(&l, -14.0, -1.0));
    }

    #[test]
    fn the_numbers_wait_for_their_tick_and_the_native_layer_keeps_the_bar() {
        let mut paint = LoudnessPaint::default();
        let mut l = Loudness::NONE;
        l.short_term = -15.2;
        let first = paint
            .refresh(&l, -14.0, -1.0, false, false)
            .expect("the first reading is formatted at once");
        assert_eq!(first.short_text, "\u{2212}15.2");

        l.short_term = -20.0;
        let moved = paint
            .refresh(&l, -14.0, -1.0, false, false)
            .expect("the bar Slint draws moves");
        assert_eq!(
            moved.short_text, "\u{2212}15.2",
            "the number waits for its tick"
        );
        assert!(
            paint.refresh(&l, -14.0, -1.0, false, false).is_none(),
            "nothing else changed"
        );

        l.short_term = -25.0;
        assert!(
            paint.refresh(&l, -14.0, -1.0, false, true).is_none(),
            "a moving bar on the native layer writes nothing (ADR-0035)"
        );
        let tick = paint
            .refresh(&l, -14.0, -1.0, true, true)
            .expect("the once-a-second tick shows the number");
        assert_eq!(tick.short_text, "\u{2212}25.0");

        l.short_term = f32::NEG_INFINITY;
        let gone = paint
            .refresh(&l, -14.0, -1.0, false, true)
            .expect("a value that goes shows at once");
        assert!(!gone.has);
    }
}
