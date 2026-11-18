//! Scrolling layers (ADR-0150; spec `docs/specs/gpu-compositor.md`, "Scrolling layers").
//!
//! A scroll moves what a layer shows through its box, the window. Its phase is closed
//! form: a [`ScrollRun`] holds an anchor time, the distance travelled at the anchor and
//! a speed change that eases in from the anchor, so any compositor evaluating it at a
//! tick's time gets the same distance and nothing accumulates (ADR-0150 decision 3).
//! The control side makes a new run for a pause, a resume or a speed change
//! ([`ScrollRun::change_speed`]); the node only evaluates. Everything here is pure
//! arithmetic: no allocation, no I/O.
//!
//! Lengths are in the units of the layer's transform (canvas pixels of the canvas the
//! layer is drawn on), speeds in those units per second.

use std::time::Duration;

use bytemuck::{Pod, Zeroable};
use rbs_core::time::Timestamp;

use crate::math::{Affine2, UvRect};

/// The way the picture moves through the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollDirection {
    /// Towards the box's left edge (a ticker).
    #[default]
    Left,
    /// Towards the box's right edge.
    Right,
    /// Towards the box's top edge (end credits).
    Up,
    /// Towards the box's bottom edge.
    Down,
}

impl ScrollDirection {
    fn vertical(self) -> bool {
        matches!(self, Self::Up | Self::Down)
    }

    /// `+1` when the window's start moves forward along the picture.
    fn sign(self) -> f64 {
        match self {
            Self::Left | Self::Up => 1.0,
            Self::Right | Self::Down => -1.0,
        }
    }
}

/// Whether copies follow each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollRepeat {
    /// Copies follow each other, `gap` apart, forever.
    #[default]
    Loop,
    /// The picture crosses the window once; then the layer draws nothing.
    Once,
}

/// Where the picture is when a run starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollStart {
    /// Where the layer places it without a scroll.
    #[default]
    Inside,
    /// Just outside the edge the motion comes from: it enters the window.
    Outside,
}

/// One scroll's motion over time (ADR-0150 decision 3). Kept by the control side and
/// sent with each layer list; the node evaluates it and keeps no state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollRun {
    /// Engine time the run is anchored at.
    pub anchor: Timestamp,
    /// Distance travelled at the anchor.
    pub position: f64,
    /// Speed at the anchor, and before it (the run this one continues).
    pub from: f64,
    /// Speed the run eases to, and keeps after `ramp`.
    pub to: f64,
    /// How long the speed eases from `from` to `to`, starting at the anchor
    /// (smoothstep, ADR-0100 `EaseInOut`).
    pub ramp: Duration,
    /// A run that continues no earlier run: before its anchor it holds still at its
    /// start instead of extrapolating.
    pub fresh: bool,
}

impl ScrollRun {
    /// A new run at `speed` from its start, anchored at `now`.
    pub fn start(now: Timestamp, speed: f64) -> Self {
        Self {
            anchor: now,
            position: 0.0,
            from: speed,
            to: speed,
            ramp: Duration::ZERO,
            fresh: true,
        }
    }

    /// Seconds from the anchor to `now`, negative before it. Exact for any span below
    /// about 104 days (f64 integers up to 2^53 nanoseconds).
    fn seconds(&self, now: Timestamp) -> f64 {
        let d = i128::from(now.as_nanos()) - i128::from(self.anchor.as_nanos());
        d as f64 / 1e9
    }

    /// Distance travelled at `now`, in f64.
    pub fn position_at(&self, now: Timestamp) -> f64 {
        let dt = self.seconds(now);
        if dt < 0.0 {
            return if self.fresh {
                self.position
            } else {
                self.position + self.from * dt
            };
        }
        let r = self.ramp.as_secs_f64();
        let delta = self.to - self.from;
        if r > 0.0 && dt < r {
            // ∫ from + delta * (3u² − 2u³) dt over [0, dt], u = t / r.
            let u = dt / r;
            let eased = u * u * u - u * u * u * u / 2.0;
            return self.position + self.from * dt + delta * r * eased;
        }
        // The whole ramp covers r * (from + delta / 2); constant speed after it.
        self.position + r * (self.from + delta / 2.0) + self.to * (dt - r)
    }

    /// Speed at `now`.
    pub fn speed_at(&self, now: Timestamp) -> f64 {
        let dt = self.seconds(now);
        if dt < 0.0 {
            return if self.fresh { 0.0 } else { self.from };
        }
        let r = self.ramp.as_secs_f64();
        if r > 0.0 && dt < r {
            let u = dt / r;
            return self.from + (self.to - self.from) * u * u * (3.0 - 2.0 * u);
        }
        self.to
    }

    /// End of the ramp (the anchor when there is none).
    pub fn ramp_end(&self) -> Timestamp {
        self.anchor + self.ramp
    }

    /// The run that continues this one from `now` and eases to `speed` over `ramp`:
    /// continuous in position and speed (pause is `speed` 0).
    pub fn change_speed(&self, now: Timestamp, speed: f64, ramp: Duration) -> Self {
        Self {
            anchor: now,
            position: self.position_at(now),
            from: self.speed_at(now),
            to: speed,
            ramp,
            fresh: false,
        }
    }

    /// This run moved to another clock: `now_old` on the old clock is `now_new` on the
    /// new one (a studio restart; ADR-0150 decision 5). Signed, as the new clock may be
    /// behind the old one.
    pub fn moved(&self, now_old: Timestamp, now_new: Timestamp) -> Self {
        let shift = i128::from(now_new.as_nanos()) - i128::from(now_old.as_nanos());
        let anchor = i128::from(self.anchor.as_nanos()) + shift;
        let anchor = u64::try_from(anchor.max(0)).unwrap_or(u64::MAX);
        Self {
            anchor: Timestamp::from_nanos(anchor),
            ..*self
        }
    }
}

/// A layer's scroll as the node receives it (ADR-0150 decision 3).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerScroll {
    /// The way the picture moves.
    pub direction: ScrollDirection,
    /// Copies forever or once.
    pub repeat: ScrollRepeat,
    /// Where a run starts.
    pub start: ScrollStart,
    /// Empty length between copies, `>= 0`.
    pub gap: f32,
    /// Soft edge length at both ends of the window, `>= 0`; clamped to half the window.
    pub fade: f32,
    /// The motion.
    pub run: ScrollRun,
}

/// The shader's record of one scrolling layer (WGSL `ScrollGpu`, bound with a dynamic
/// offset on group 2): `a` = window start on the strip (wrapped for `Loop`), window
/// length, picture length, period; `b` = axis (0 horizontal, 1 vertical), once (1),
/// fade length, unused. Lengths in the transform's units.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct ScrollGpu {
    a: [f32; 4],
    b: [f32; 4],
}

/// Where a scroll's window is at one tick, before it becomes a [`ScrollGpu`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollFrame {
    /// Start of the window on the strip (copy `k` covers `[k * period, k * period +
    /// picture)`), in f64.
    pub start: f64,
    /// Window length along the direction.
    pub window: f64,
    /// Picture length along the direction, scaled to fill the box across it.
    pub picture: f64,
    /// Picture plus gap.
    pub period: f64,
    /// The picture has fully left the window and will not come back (`Once` only).
    pub ended: bool,
}

impl LayerScroll {
    /// The window at `now` for a box `transform` showing `crop` of a `width x height`
    /// texture. `None` when the box or the picture has no length.
    pub fn frame(
        &self,
        transform: &Affine2,
        crop: &UvRect,
        width: u32,
        height: u32,
        now: Timestamp,
    ) -> Option<ScrollFrame> {
        let x_len = f64::from(transform.a).hypot(f64::from(transform.b));
        let y_len = f64::from(transform.c).hypot(f64::from(transform.d));
        let pw = f64::from(crop.u1 - crop.u0) * f64::from(width);
        let ph = f64::from(crop.v1 - crop.v0) * f64::from(height);
        let (window, cross, along, across) = if self.direction.vertical() {
            (y_len, x_len, ph, pw)
        } else {
            (x_len, y_len, pw, ph)
        };
        let picture = along * cross / across;
        if !(window > 0.0 && picture > 0.0 && picture.is_finite() && window.is_finite()) {
            return None;
        }
        let gap = f64::from(self.gap).max(0.0);
        let period = picture + gap;
        let sign = self.direction.sign();
        let origin = match (self.start, sign > 0.0) {
            (ScrollStart::Inside, _) => 0.0,
            // Forward: the picture's start sits at the window's far end.
            (ScrollStart::Outside, true) => -window,
            // Backward: the picture's end sits at the window's near end.
            (ScrollStart::Outside, false) => picture,
        };
        let raw = origin + sign * self.run.position_at(now);
        let (start, ended) = match self.repeat {
            ScrollRepeat::Loop => (raw.rem_euclid(period), false),
            ScrollRepeat::Once => {
                let ended = if sign > 0.0 {
                    raw >= picture
                } else {
                    raw + window <= 0.0
                };
                // Bounded so the f32 record keeps its precision; outside the picture
                // either way.
                (raw.clamp(-window - 1.0, picture + 1.0), ended)
            }
        };
        Some(ScrollFrame {
            start,
            window,
            picture,
            period,
            ended,
        })
    }

    /// Whether this scroll still moves the picture at `at` (ADR-0150 decision 8): the
    /// speed is above 0 and a `Once` picture has not left, a ramp has not ended, or a
    /// fresh run has not started yet but will move.
    pub fn moving_at(&self, at: Timestamp, ended: bool) -> bool {
        let run = &self.run;
        let once_done = self.repeat == ScrollRepeat::Once && ended;
        (run.speed_at(at) > 0.0 && !once_done)
            || (run.ramp > Duration::ZERO && at <= run.ramp_end())
            || (run.fresh && at < run.anchor && run.to > 0.0)
    }
}

impl ScrollFrame {
    /// The shader record for a scroll in `direction` with `repeat` and `fade`.
    pub fn gpu(&self, direction: ScrollDirection, repeat: ScrollRepeat, fade: f32) -> ScrollGpu {
        let fade = f64::from(fade).clamp(0.0, self.window / 2.0);
        ScrollGpu {
            a: [
                self.start as f32,
                self.window as f32,
                self.picture as f32,
                self.period as f32,
            ],
            b: [
                if direction.vertical() { 1.0 } else { 0.0 },
                if repeat == ScrollRepeat::Once {
                    1.0
                } else {
                    0.0
                },
                fade as f32,
                0.0,
            ],
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn s(secs: f64) -> Timestamp {
        Timestamp::from_nanos((secs * 1e9) as u64)
    }

    fn layer(direction: ScrollDirection, repeat: ScrollRepeat, start: ScrollStart) -> LayerScroll {
        LayerScroll {
            direction,
            repeat,
            start,
            gap: 100.0,
            fade: 0.0,
            run: ScrollRun::start(s(10.0), 200.0),
        }
    }

    /// A 1000 x 100 box showing a whole 4000 x 200 texture: the picture is 2000 long.
    fn frame(l: &LayerScroll, now: Timestamp) -> ScrollFrame {
        let t = Affine2::rect(0.0, 0.0, 1000.0, 100.0);
        l.frame(&t, &UvRect::FULL, 4000, 200, now).expect("frame")
    }

    #[test]
    fn constant_speed_is_exact_after_thirty_days() {
        let run = ScrollRun::start(s(5.0), 123.25);
        let days30 = 30 * 86_400;
        let now = Timestamp::from_nanos(5_000_000_000 + days30 * 1_000_000_000);
        assert_eq!(run.position_at(now), 123.25 * days30 as f64);
        let day = Timestamp::from_nanos(5_000_000_000 + 86_400_000_000_000);
        assert_eq!(run.position_at(day), 123.25 * 86_400.0);
    }

    #[test]
    fn a_fresh_run_holds_before_its_anchor_and_a_continued_one_extrapolates() {
        let fresh = ScrollRun::start(s(10.0), 100.0);
        assert_eq!(fresh.position_at(s(9.5)), 0.0);
        assert_eq!(fresh.speed_at(s(9.5)), 0.0);
        let changed = fresh.change_speed(s(20.0), 300.0, Duration::from_millis(500));
        assert!((changed.position - 1000.0).abs() < 1e-9);
        // Before the new anchor: the old speed, exactly as the old run.
        assert!((changed.position_at(s(19.9)) - fresh.position_at(s(19.9))).abs() < 1e-9);
    }

    #[test]
    fn a_speed_change_is_continuous_in_position_and_speed() {
        let run = ScrollRun::start(s(0.0), 100.0);
        let ramp = Duration::from_millis(400);
        let next = run.change_speed(s(2.0), 0.0, ramp);
        let eps = 1e-6;
        for t in [2.0, 2.0 + eps, 2.2, 2.4 - eps, 2.4, 2.4 + eps] {
            let a = next.position_at(s(t));
            let b = next.position_at(s(t + 1e-6));
            assert!((b - a).abs() < 1e-3, "jump at {t}");
        }
        assert_eq!(next.speed_at(s(2.0)), 100.0);
        assert!((next.speed_at(s(2.2)) - 50.0).abs() < 1e-9);
        assert_eq!(next.speed_at(s(2.4)), 0.0);
        // Ramp distance is half of from * ramp; then it stays.
        assert!((next.position_at(s(3.0)) - (200.0 + 20.0)).abs() < 1e-9);
        assert_eq!(next.position_at(s(100.0)), next.position_at(s(3.0)));
    }

    #[test]
    fn a_change_during_a_ramp_continues_smoothly_with_a_tick_before_the_anchor() {
        let run = ScrollRun::start(s(0.0), 100.0);
        let ramp = Duration::from_millis(1000);
        let slowing = run.change_speed(s(1.0), 0.0, ramp);
        let back = slowing.change_speed(s(1.5), 200.0, ramp);
        assert!((back.speed_at(s(1.5)) - slowing.speed_at(s(1.5))).abs() < 1e-9);
        // A tick 5 ms before the new anchor: extrapolated at the speed then, within a
        // fraction of a pixel of the run it replaces.
        let tick = s(1.495);
        assert!((back.position_at(tick) - slowing.position_at(tick)).abs() < 0.1);
    }

    #[test]
    fn a_restart_moves_the_anchor_by_the_signed_clock_difference() {
        let run = ScrollRun::start(s(100.0), 50.0);
        // The new clock is behind: 100 s on the old clock is -30 s, saturated at 0.
        let moved = run.moved(s(150.0), s(20.0));
        assert_eq!(moved.anchor, Timestamp::ZERO);
        let ahead = run.moved(s(150.0), s(400.0));
        assert_eq!(ahead.position_at(s(450.0)), run.position_at(s(200.0)));
    }

    #[test]
    fn the_picture_fills_the_box_across_the_direction() {
        let l = layer(
            ScrollDirection::Left,
            ScrollRepeat::Loop,
            ScrollStart::Inside,
        );
        let f = frame(&l, s(10.0));
        assert_eq!(f.window, 1000.0);
        assert_eq!(f.picture, 2000.0);
        assert_eq!(f.period, 2100.0);
        assert_eq!(f.start, 0.0);
    }

    #[test]
    fn loop_wraps_and_outside_starts_beyond_the_edge() {
        let l = layer(
            ScrollDirection::Left,
            ScrollRepeat::Loop,
            ScrollStart::Inside,
        );
        // 11.5 s at 200/s past the anchor at 10 s: 300 along.
        assert!((frame(&l, s(11.5)).start - 300.0).abs() < 1e-6);
        // 2100 per period: 12 s later is 2400, wrapped 300.
        assert!((frame(&l, s(22.0)).start - 300.0).abs() < 1e-6);
        let out = layer(
            ScrollDirection::Left,
            ScrollRepeat::Loop,
            ScrollStart::Outside,
        );
        assert!((frame(&out, s(10.0)).start - (2100.0 - 1000.0)).abs() < 1e-6);
        let right = layer(
            ScrollDirection::Right,
            ScrollRepeat::Loop,
            ScrollStart::Outside,
        );
        assert!((frame(&right, s(10.0)).start - 2000.0).abs() < 1e-6);
    }

    #[test]
    fn once_ends_when_the_picture_has_left() {
        let l = layer(
            ScrollDirection::Left,
            ScrollRepeat::Once,
            ScrollStart::Outside,
        );
        // From -1000 to 2000: 3000 at 200/s is 15 s.
        assert!(!frame(&l, s(24.9)).ended);
        assert!(frame(&l, s(25.0)).ended);
        assert!(l.moving_at(s(24.9), false));
        assert!(!l.moving_at(s(25.0), true));
        let r = layer(
            ScrollDirection::Right,
            ScrollRepeat::Once,
            ScrollStart::Inside,
        );
        // From 0 back to -1000: 5 s.
        assert!(!frame(&r, s(14.9)).ended);
        assert!(frame(&r, s(15.0)).ended);
    }

    #[test]
    fn paused_runs_do_not_move_and_fresh_runs_do_before_their_anchor() {
        let mut l = layer(ScrollDirection::Up, ScrollRepeat::Once, ScrollStart::Inside);
        assert!(l.moving_at(s(9.9), false), "fresh run about to start");
        l.run = l.run.change_speed(s(12.0), 0.0, Duration::from_millis(300));
        assert!(
            l.moving_at(s(12.3), false),
            "the last tick of the ramp draws"
        );
        assert!(
            !l.moving_at(s(12.31), false),
            "a paused Once does not keep drawing"
        );
        let mut lp = layer(
            ScrollDirection::Left,
            ScrollRepeat::Loop,
            ScrollStart::Inside,
        );
        lp.run = lp.run.change_speed(s(12.0), 0.0, Duration::ZERO);
        assert!(!lp.moving_at(s(12.0), false));
    }

    #[test]
    fn vertical_uses_the_box_width_across() {
        let l = layer(ScrollDirection::Up, ScrollRepeat::Loop, ScrollStart::Inside);
        let t = Affine2::rect(0.0, 0.0, 400.0, 300.0);
        let f = l
            .frame(&t, &UvRect::FULL, 200, 1000, s(10.0))
            .expect("frame");
        assert_eq!(f.window, 300.0);
        assert_eq!(f.picture, 2000.0);
        let g = f.gpu(ScrollDirection::Up, ScrollRepeat::Loop, 1000.0);
        assert_eq!(g.b, [1.0, 0.0, 150.0, 0.0]);
    }

    #[test]
    fn an_empty_box_or_picture_has_no_frame() {
        let l = layer(
            ScrollDirection::Left,
            ScrollRepeat::Loop,
            ScrollStart::Inside,
        );
        let t = Affine2::rect(0.0, 0.0, 0.0, 100.0);
        assert!(l.frame(&t, &UvRect::FULL, 10, 10, s(10.0)).is_none());
        let t = Affine2::rect(0.0, 0.0, 100.0, 100.0);
        assert!(l.frame(&t, &UvRect::FULL, 0, 10, s(10.0)).is_none());
    }
}
