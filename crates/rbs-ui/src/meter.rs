//! Audio meter scale and ballistics (AudioMeter, rule U16).
//!
//! Spec: `docs/specs/ui-design-system.md`, "AudioMeter". Toolkit-neutral: whoever draws
//! the meter (the toolkit, or a native layer per ADR-0035) gets a [`MeterView`] of
//! positions on the scale and only maps them to pixels.

use std::time::Duration;

use crate::layout::Rect;

/// Bottom of the scale in dBFS. Lower levels show as an empty meter.
pub const FLOOR_DB: f32 = -60.0;
/// Top of the scale in dBFS.
pub const TOP_DB: f32 = 0.0;
/// The `color.meter.mid` zone starts here (dBFS).
pub const MID_FROM_DB: f32 = -18.0;
/// The `color.meter.high` zone starts here (dBFS).
pub const HIGH_FROM_DB: f32 = -6.0;
/// A level at or above this (dBFS) latches the clip indicator.
pub const CLIP_DB: f32 = 0.0;
/// The peak-hold line stays at a new peak this long before it falls.
pub const PEAK_HOLD: Duration = Duration::from_millis(1500);
/// How fast the peak-hold line falls after [`PEAK_HOLD`], in dB per second.
pub const PEAK_FALL_DB_PER_S: f32 = 20.0;
/// Thickness of the peak-hold line in logical pixels.
pub const PEAK_LINE: f32 = 2.0;
/// Labelled scale marks in dBFS, top to bottom.
pub const SCALE_MARKS_DB: [f32; 7] = [0.0, -6.0, -12.0, -18.0, -24.0, -36.0, -48.0];

/// Position of `db` on the scale: 0 at [`FLOOR_DB`] and below, 1 at [`TOP_DB`] and
/// above, linear in dB between. Not a number maps to 0.
pub fn position(db: f32) -> f32 {
    let p = (db - FLOOR_DB) / (TOP_DB - FLOOR_DB);
    if p.is_nan() { 0.0 } else { p.clamp(0.0, 1.0) }
}

/// `v` clamped to 0..=1; not a number is 0.
fn unit(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

/// What one meter shows at one moment, as positions on the scale (see [`position`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterView {
    /// Top of the bar.
    pub level: f32,
    /// The peak-hold line; never below `level`.
    pub peak: f32,
    /// The clip indicator is lit (latched until [`Meter::reset_clip`]).
    pub clipped: bool,
}

impl MeterView {
    /// An empty meter.
    pub const SILENT: MeterView = MeterView {
        level: 0.0,
        peak: 0.0,
        clipped: false,
    };

    /// The lit part of the bar in each colour zone (`color.meter.low`, `.mid`, `.high`),
    /// as `(from, to)` positions with `from <= to`; an unlit zone is empty (`from == to`).
    pub fn zones(&self) -> [(f32, f32); 3] {
        let mid = position(MID_FROM_DB);
        let high = position(HIGH_FROM_DB);
        let level = unit(self.level);
        let band = |from: f32, to: f32| (from, level.clamp(from, to));
        [band(0.0, mid), band(mid, high), band(high, 1.0)]
    }

    /// Where the lit zones and the peak-hold line go inside `bar` (any coordinate space
    /// with the origin at the top left). Levels that are not a number count as 0; a `bar`
    /// that is not finite gives rectangles that are not finite, which drawing code must
    /// skip ([`Rect`] values come from the toolkit's layout).
    pub fn frames(&self, bar: Rect, orientation: Orientation) -> MeterFrames {
        let along = |from: f32, to: f32| match orientation {
            Orientation::Vertical => Rect {
                x: bar.x,
                y: bar.y + bar.h * (1.0 - to),
                w: bar.w,
                h: bar.h * (to - from),
            },
            Orientation::Horizontal => Rect {
                x: bar.x + bar.w * from,
                y: bar.y,
                w: bar.w * (to - from),
                h: bar.h,
            },
        };
        let zones = self.zones().map(|(from, to)| along(from, to));
        let peak = unit(self.peak);
        let peak_line = (peak > 0.0).then(|| match orientation {
            Orientation::Vertical => Rect {
                x: bar.x,
                y: (bar.y + bar.h * (1.0 - peak))
                    .min(bar.y + bar.h - PEAK_LINE)
                    .max(bar.y),
                w: bar.w,
                h: PEAK_LINE.min(bar.h),
            },
            Orientation::Horizontal => Rect {
                x: (bar.x + bar.w * peak - PEAK_LINE).max(bar.x),
                y: bar.y,
                w: PEAK_LINE.min(bar.w),
                h: bar.h,
            },
        });
        let peak_zone = if peak >= position(HIGH_FROM_DB) {
            2
        } else if peak >= position(MID_FROM_DB) {
            1
        } else {
            0
        };
        MeterFrames {
            zones,
            peak: peak_line,
            peak_zone,
        }
    }
}

/// Which way a meter's bar grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// From the bottom up.
    Vertical,
    /// From the left to the right.
    Horizontal,
}

/// Rectangles of one meter's moving parts, from [`MeterView::frames`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterFrames {
    /// Lit part of each zone (`color.meter.low`, `.mid`, `.high`); empty when unlit.
    pub zones: [Rect; 3],
    /// The peak-hold line, if the peak is above the floor.
    pub peak: Option<Rect>,
    /// Zone of the peak (0 low, 1 mid, 2 high): the line takes that zone's colour.
    pub peak_zone: usize,
}

/// A meter's accessible value (spec "AudioMeter", Accessibility). Its text comes from
/// the Fluent messages `meter-value-silent`, `meter-value-db` (variable `$db`) and
/// `meter-value-clipped`; [`AccessibleLevel::english`] is the built-in English text until
/// the UI has Fluent strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessibleLevel {
    /// Highest level of the period in whole dBFS; `None` below the scale's floor.
    pub level: Option<i32>,
    /// The clip indicator is lit.
    pub clipped: bool,
}

impl AccessibleLevel {
    /// English text: "silent", "-12 dBFS", "-12 dBFS, clipped".
    pub fn english(&self) -> String {
        let level = match self.level {
            None => "silent".to_owned(),
            Some(db) => format!("{db} dBFS"),
        };
        if self.clipped {
            format!("{level}, clipped")
        } else {
            level
        }
    }
}

/// Ballistics of one meter: the level, the peak-hold line and the clip latch.
///
/// Time is passed in by the caller (time since any fixed start, the same for every
/// call), so tests drive it without sleeping. No allocation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Meter {
    /// Highest level since the last [`Meter::take_report`].
    report_db: f32,
    level_db: f32,
    peak_db: f32,
    peak_at: Duration,
    clipped: bool,
}

impl Default for Meter {
    fn default() -> Self {
        Self::new()
    }
}

impl Meter {
    /// A silent meter.
    pub const fn new() -> Self {
        Meter {
            report_db: f32::NEG_INFINITY,
            level_db: f32::NEG_INFINITY,
            peak_db: f32::NEG_INFINITY,
            peak_at: Duration::ZERO,
            clipped: false,
        }
    }

    /// Takes the newest level (dBFS, the peak of the samples since the last update).
    /// Not a number counts as silence.
    pub fn update(&mut self, level_db: f32, now: Duration) {
        let level_db = if level_db.is_nan() {
            f32::NEG_INFINITY
        } else {
            level_db
        };
        self.level_db = level_db;
        if level_db > self.report_db {
            self.report_db = level_db;
        }
        if level_db >= self.peak_at_time(now) {
            self.peak_db = level_db;
            self.peak_at = now;
        }
        if level_db >= CLIP_DB {
            self.clipped = true;
        }
    }

    /// Turns the clip indicator off (the operator acknowledged it).
    pub fn reset_clip(&mut self) {
        self.clipped = false;
    }

    /// The peak-hold value in dBFS at `now`: held for [`PEAK_HOLD`], then falling.
    pub fn peak_at_time(&self, now: Duration) -> f32 {
        let since = now.saturating_sub(self.peak_at);
        match since.checked_sub(PEAK_HOLD) {
            None => self.peak_db,
            Some(falling) => self.peak_db - PEAK_FALL_DB_PER_S * falling.as_secs_f32(),
        }
    }

    /// The level last given to [`Meter::update`], in dBFS.
    pub fn level_db(&self) -> f32 {
        self.level_db
    }

    /// The accessible value since the last call (the highest level since then, and the
    /// clip latch); starts a new report period. Call it at most once per second.
    pub fn take_report(&mut self) -> AccessibleLevel {
        let db = std::mem::replace(&mut self.report_db, f32::NEG_INFINITY);
        let level = if db < FLOOR_DB {
            None
        } else {
            // Whole dB; the scale tops out at 0 dBFS but a level above it is reported.
            Some(db.round() as i32)
        };
        AccessibleLevel {
            level,
            clipped: self.clipped,
        }
    }

    /// What the meter shows at `now`.
    pub fn view(&self, now: Duration) -> MeterView {
        let level = position(self.level_db);
        MeterView {
            level,
            peak: position(self.peak_at_time(now)).max(level),
            clipped: self.clipped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn position_is_clamped_and_linear() {
        assert_eq!(position(-120.0), 0.0);
        assert_eq!(position(f32::NEG_INFINITY), 0.0);
        assert_eq!(position(f32::NAN), 0.0);
        assert_eq!(position(6.0), 1.0);
        assert_eq!(position(0.0), 1.0);
        assert!((position(-30.0) - 0.5).abs() < 1e-6);
        assert!((position(MID_FROM_DB) - 0.7).abs() < 1e-6);
        assert!((position(HIGH_FROM_DB) - 0.9).abs() < 1e-6);
    }

    #[test]
    fn position_is_monotonic() {
        let mut last = 0.0;
        for i in 0..=1400 {
            let db = -70.0 + i as f32 * 0.05;
            let p = position(db);
            assert!(p >= last, "{db} dB went down");
            assert!((0.0..=1.0).contains(&p));
            last = p;
        }
    }

    #[test]
    fn scale_marks_are_on_the_scale_and_ordered() {
        for pair in SCALE_MARKS_DB.windows(2) {
            assert!(pair[0] > pair[1]);
        }
        for db in SCALE_MARKS_DB {
            assert!((FLOOR_DB..=TOP_DB).contains(&db));
        }
    }

    #[test]
    fn zones_split_the_bar_at_the_zone_marks() {
        let mid = position(MID_FROM_DB);
        let high = position(HIGH_FROM_DB);
        let empty = MeterView::SILENT.zones();
        assert!(empty.iter().all(|(a, b)| a == b));
        let low_only = MeterView {
            level: 0.5,
            ..MeterView::SILENT
        }
        .zones();
        assert_eq!(low_only, [(0.0, 0.5), (mid, mid), (high, high)]);
        let full = MeterView {
            level: 1.0,
            ..MeterView::SILENT
        }
        .zones();
        assert_eq!(full, [(0.0, mid), (mid, high), (high, 1.0)]);
        for i in 0..=100 {
            let level = i as f32 / 100.0;
            let z = MeterView {
                level,
                ..MeterView::SILENT
            }
            .zones();
            let lit: f32 = z.iter().map(|(a, b)| b - a).sum();
            assert!((lit - level).abs() < 1e-5, "{level}");
        }
    }

    #[test]
    fn vertical_frames_grow_from_the_bottom() {
        let bar = Rect {
            x: 10.0,
            y: 20.0,
            w: 8.0,
            h: 100.0,
        };
        let v = MeterView {
            level: 0.8,
            peak: 0.95,
            clipped: false,
        };
        let f = v.frames(bar, Orientation::Vertical);
        // Low zone: 0..0.7 -> y 50..120.
        assert_eq!(
            f.zones[0],
            Rect {
                x: 10.0,
                y: 50.0,
                w: 8.0,
                h: 70.0
            }
        );
        // Mid zone: 0.7..0.8 -> y 40..50.
        assert!((f.zones[1].y - 40.0).abs() < 1e-4 && (f.zones[1].h - 10.0).abs() < 1e-4);
        assert_eq!(f.zones[2].h, 0.0);
        let peak = f.peak.unwrap_or_default();
        assert!((peak.y - 25.0).abs() < 1e-4 && peak.h == PEAK_LINE);
        assert_eq!(f.peak_zone, 2);
    }

    #[test]
    fn horizontal_frames_grow_from_the_left() {
        let bar = Rect {
            x: 0.0,
            y: 0.0,
            w: 200.0,
            h: 6.0,
        };
        let v = MeterView {
            level: 0.5,
            peak: 0.5,
            clipped: false,
        };
        let f = v.frames(bar, Orientation::Horizontal);
        assert_eq!(
            f.zones[0],
            Rect {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 6.0
            }
        );
        assert_eq!(f.zones[1].w, 0.0);
        assert_eq!(
            f.peak,
            Some(Rect {
                x: 98.0,
                y: 0.0,
                w: PEAK_LINE,
                h: 6.0
            })
        );
        assert_eq!(f.peak_zone, 0);
    }

    #[test]
    fn peak_line_stays_inside_the_bar() {
        let bar = Rect {
            x: 0.0,
            y: 0.0,
            w: 4.0,
            h: 50.0,
        };
        for i in 0..=100 {
            let p = i as f32 / 100.0;
            let v = MeterView {
                level: 0.0,
                peak: p,
                clipped: false,
            };
            let f = v.frames(bar, Orientation::Vertical);
            match f.peak {
                None => assert_eq!(p, 0.0),
                Some(r) => assert!(r.y >= 0.0 && r.y + r.h <= 50.0 + 1e-4, "{p}"),
            }
        }
    }

    #[test]
    fn silent_meter_shows_nothing() {
        assert_eq!(Meter::new().view(ms(0)), MeterView::SILENT);
    }

    #[test]
    fn peak_holds_then_falls() {
        let mut m = Meter::new();
        m.update(-12.0, ms(0));
        m.update(-40.0, ms(100));
        // Held.
        assert_eq!(m.peak_at_time(ms(1500)), -12.0);
        // Falling at 20 dB/s after the hold: 0.5 s later, 10 dB lower.
        assert!((m.peak_at_time(ms(2000)) - -22.0).abs() < 1e-3);
        let v = m.view(ms(2000));
        assert!((v.peak - position(-22.0)).abs() < 1e-6);
        assert!((v.level - position(-40.0)).abs() < 1e-6);
    }

    #[test]
    fn peak_never_below_level() {
        let mut m = Meter::new();
        m.update(-6.0, ms(0));
        // Long after the hold the peak has fallen below the new level.
        m.update(-30.0, ms(10_000));
        let v = m.view(ms(10_000));
        assert!(v.peak >= v.level);
    }

    #[test]
    fn a_higher_level_restarts_the_hold() {
        let mut m = Meter::new();
        m.update(-20.0, ms(0));
        m.update(-10.0, ms(1000));
        assert_eq!(m.peak_at_time(ms(2400)), -10.0);
    }

    #[test]
    fn a_level_above_the_falling_peak_takes_over() {
        let mut m = Meter::new();
        m.update(-10.0, ms(0));
        // At 2.0 s the held peak has fallen to -20 dB; -15 dB is above it.
        m.update(-15.0, ms(2000));
        assert_eq!(m.peak_at_time(ms(2000)), -15.0);
        assert_eq!(m.peak_at_time(ms(3000)), -15.0);
    }

    #[test]
    fn clip_latches_until_reset() {
        let mut m = Meter::new();
        m.update(-1.0, ms(0));
        assert!(!m.view(ms(0)).clipped);
        m.update(0.0, ms(33));
        m.update(-50.0, ms(66));
        assert!(m.view(ms(5000)).clipped);
        m.reset_clip();
        assert!(!m.view(ms(5000)).clipped);
    }

    #[test]
    fn nan_views_draw_nothing() {
        let bar = Rect {
            x: 0.0,
            y: 0.0,
            w: 8.0,
            h: 40.0,
        };
        let v = MeterView {
            level: f32::NAN,
            peak: f32::NAN,
            clipped: false,
        };
        let f = v.frames(bar, Orientation::Vertical);
        assert!(f.zones.iter().all(|r| r.h == 0.0));
        assert_eq!(f.peak, None);
    }

    #[test]
    fn peak_line_stays_inside_a_bar_shorter_than_the_line() {
        let bar = Rect {
            x: 0.0,
            y: 10.0,
            w: 8.0,
            h: 1.0,
        };
        let v = MeterView {
            level: 0.0,
            peak: 1.0,
            clipped: false,
        };
        let r = v
            .frames(bar, Orientation::Vertical)
            .peak
            .unwrap_or_default();
        assert!(r.y >= 10.0 && r.y + r.h <= 11.0 + 1e-6, "{r:?}");
    }

    #[test]
    fn accessible_value_reports_the_period_peak() {
        let mut m = Meter::new();
        assert_eq!(m.take_report().english(), "silent");
        m.update(-30.4, ms(0));
        m.update(-12.2, ms(100));
        m.update(-40.0, ms(200));
        assert_eq!(
            m.take_report(),
            AccessibleLevel {
                level: Some(-12),
                clipped: false
            }
        );
        // A new period starts empty.
        assert_eq!(m.take_report().level, None);
        m.update(-70.0, ms(300));
        assert_eq!(m.take_report().english(), "silent");
        m.update(0.4, ms(400));
        assert_eq!(m.take_report().english(), "0 dBFS, clipped");
        m.update(f32::NEG_INFINITY, ms(500));
        assert_eq!(m.take_report().english(), "silent, clipped");
    }

    #[test]
    fn nan_counts_as_silence() {
        let mut m = Meter::new();
        m.update(f32::NAN, ms(0));
        assert_eq!(m.view(ms(0)), MeterView::SILENT);
        assert_eq!(m.level_db(), f32::NEG_INFINITY);
    }
}
