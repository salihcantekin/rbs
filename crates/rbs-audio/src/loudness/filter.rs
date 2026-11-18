//! The K-weighting pre-filter of ITU-R BS.1770-4, for any sample rate.
//!
//! BS.1770 gives the two biquads for 48 kHz only. Both stages here come from their
//! analogue prototypes by the bilinear transform with prewarping, so they match the
//! published 48 kHz coefficients and keep the same response at other rates.

/// Stage 1 (head shelf): centre frequency in Hz.
const SHELF_F0: f64 = 1_681.974_450_955_533;
/// Stage 1: gain in dB.
const SHELF_GAIN_DB: f64 = 3.999_843_853_973_347;
/// Stage 1: quality factor.
const SHELF_Q: f64 = 0.707_175_236_955_419_6;
/// Stage 2 (RLB high-pass): corner frequency in Hz.
const HIGH_PASS_F0: f64 = 38.135_470_876_024_44;
/// Stage 2: quality factor.
const HIGH_PASS_Q: f64 = 0.500_327_037_323_877_3;

/// One second-order section, normalised so `a0` is 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Biquad {
    pub(crate) b0: f64,
    pub(crate) b1: f64,
    pub(crate) b2: f64,
    pub(crate) a1: f64,
    pub(crate) a2: f64,
}

/// The two stages of the K-weighting filter for `rate` Hz.
pub(crate) fn k_weighting(rate: f64) -> [Biquad; 2] {
    let k = (std::f64::consts::PI * SHELF_F0 / rate).tan();
    let vh = 10f64.powf(SHELF_GAIN_DB / 20.0);
    let vb = vh.powf(0.499_666_774_154_541_6);
    let a0 = 1.0 + k / SHELF_Q + k * k;
    let shelf = Biquad {
        b0: (vh + vb * k / SHELF_Q + k * k) / a0,
        b1: 2.0 * (k * k - vh) / a0,
        b2: (vh - vb * k / SHELF_Q + k * k) / a0,
        a1: 2.0 * (k * k - 1.0) / a0,
        a2: (1.0 - k / SHELF_Q + k * k) / a0,
    };
    let k = (std::f64::consts::PI * HIGH_PASS_F0 / rate).tan();
    let a0 = 1.0 + k / HIGH_PASS_Q + k * k;
    let high_pass = Biquad {
        b0: 1.0,
        b1: -2.0,
        b2: 1.0,
        a1: 2.0 * (k * k - 1.0) / a0,
        a2: (1.0 - k / HIGH_PASS_Q + k * k) / a0,
    };
    [shelf, high_pass]
}

/// The state of both stages for one channel (transposed direct form II).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct KState {
    s: [[f64; 2]; 2],
}

impl KState {
    /// Filters `x` and returns the sum of squares of the K-weighted signal.
    /// Real-time safe: no allocation, no branch per sample.
    #[inline]
    pub(crate) fn sum_squares(&mut self, f: &[Biquad; 2], x: &[f32]) -> f64 {
        let [p, q] = f;
        let [mut s0, mut s1] = self.s[0];
        let [mut t0, mut t1] = self.s[1];
        let mut acc = 0.0;
        for &v in x {
            let v = f64::from(v);
            let y = p.b0 * v + s0;
            s0 = p.b1 * v - p.a1 * y + s1;
            s1 = p.b2 * v - p.a2 * y;
            let z = q.b0 * y + t0;
            t0 = q.b1 * y - q.a1 * z + t1;
            t1 = q.b2 * y - q.a2 * z;
            acc += z * z;
        }
        // Denormals would slow the filter down after long silence.
        self.s = [[flush(s0), flush(s1)], [flush(t0), flush(t1)]];
        acc
    }
}

/// Filters two channels together and returns the sum of squares of each. The two
/// recursions are independent, so they overlap in the pipeline (or share one vector
/// register); each lane does exactly what [`KState::sum_squares`] does, so the results
/// are bit-exact. `x` and `y` must be the same length. Real-time safe.
#[inline]
pub(crate) fn sum_squares_pair(
    states: &mut [KState],
    f: &[Biquad; 2],
    x: &[f32],
    y: &[f32],
) -> (f64, f64) {
    let [p, q] = f;
    let (Some(a), Some(b)) = (states.first(), states.get(1)) else {
        return (0.0, 0.0);
    };
    // Lane 0 is `x`, lane 1 is `y`.
    let mut s0 = [a.s[0][0], b.s[0][0]];
    let mut s1 = [a.s[0][1], b.s[0][1]];
    let mut t0 = [a.s[1][0], b.s[1][0]];
    let mut t1 = [a.s[1][1], b.s[1][1]];
    let mut acc = [0.0f64; 2];
    for (&u, &w) in x.iter().zip(y) {
        let v = [f64::from(u), f64::from(w)];
        for l in 0..2 {
            let y = p.b0 * v[l] + s0[l];
            s0[l] = p.b1 * v[l] - p.a1 * y + s1[l];
            s1[l] = p.b2 * v[l] - p.a2 * y;
            let z = q.b0 * y + t0[l];
            t0[l] = q.b1 * y - q.a1 * z + t1[l];
            t1[l] = q.b2 * y - q.a2 * z;
            acc[l] += z * z;
        }
    }
    for (l, st) in states.iter_mut().take(2).enumerate() {
        st.s = [[flush(s0[l]), flush(s1[l])], [flush(t0[l]), flush(t1[l])]];
    }
    (acc[0], acc[1])
}

#[inline]
fn flush(v: f64) -> f64 {
    if v.abs() < 1e-30 { 0.0 } else { v }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_channels_together_match_one_at_a_time() {
        let f = k_weighting(48_000.0);
        let x: Vec<f32> = (0..480).map(|i| (i as f32 * 0.05).sin()).collect();
        let y: Vec<f32> = (0..480).map(|i| 0.3 * (i as f32 * 0.31).cos()).collect();
        let mut single = [KState::default(); 2];
        let mut pair = [KState::default(); 2];
        for _ in 0..3 {
            let want = (single[0].sum_squares(&f, &x), single[1].sum_squares(&f, &y));
            assert_eq!(sum_squares_pair(&mut pair, &f, &x, &y), want);
        }
        assert_eq!(single[0].s, pair[0].s);
        assert_eq!(single[1].s, pair[1].s);
    }

    #[test]
    fn the_48_khz_coefficients_match_bs_1770() {
        // ITU-R BS.1770-4, Annex 1, tables 1 and 2.
        let [s, h] = k_weighting(48_000.0);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
        assert!(close(s.b0, 1.535_124_859_586_97), "{s:?}");
        assert!(close(s.b1, -2.691_696_189_406_38), "{s:?}");
        assert!(close(s.b2, 1.198_392_810_852_85), "{s:?}");
        assert!(close(s.a1, -1.690_659_293_182_41), "{s:?}");
        assert!(close(s.a2, 0.732_480_774_215_85), "{s:?}");
        assert!(close(h.a1, -1.990_047_454_833_98), "{h:?}");
        assert!(close(h.a2, 0.990_072_250_366_21), "{h:?}");
        assert_eq!((h.b0, h.b1, h.b2), (1.0, -2.0, 1.0));
    }
}
