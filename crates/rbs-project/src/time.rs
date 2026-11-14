//! Exact rational times for project files.
//!
//! Positions on the sequence are whole frames of the sequence's rate; a sound clip's
//! in-point and an asset's length are whole samples or frames of the file's own rate.
//! Text is FCPXML's rational seconds: `0s`, `5s` or `1001/30000s`.

use rbs_core::timing::FrameRate;

const NANOS: u128 = 1_000_000_000;

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// `num/den` seconds as FCPXML text, reduced.
pub(crate) fn seconds(num: u128, den: u128) -> String {
    if num == 0 {
        return "0s".to_owned();
    }
    let g = gcd(num, den);
    let (n, d) = (num / g, den / g);
    if d == 1 {
        format!("{n}s")
    } else {
        format!("{n}/{d}s")
    }
}

/// A rate as `(num, den)` frames per second.
fn parts(rate: FrameRate) -> (u128, u128) {
    let r = rate.per_second();
    (u128::from(r.num()), u128::from(r.den()))
}

/// `frames` frames of `rate` as text.
pub(crate) fn frames_text(frames: u64, rate: FrameRate) -> String {
    let (num, den) = parts(rate);
    seconds(u128::from(frames) * den, num)
}

/// `samples` at `sample_rate` as text.
pub(crate) fn samples_text(samples: u64, sample_rate: u32) -> String {
    seconds(u128::from(samples), u128::from(sample_rate))
}

/// `ns` in frames of `rate`, as the exact fraction `(numerator, denominator)`.
fn frame_fraction(ns: u64, rate: FrameRate) -> (u128, u128) {
    let (num, den) = parts(rate);
    (u128::from(ns) * num, den * NANOS)
}

fn to_u64(v: u128) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

/// The nearest frame; exactly half a frame rounds up.
pub(crate) fn nearest_frame(ns: u64, rate: FrameRate) -> u64 {
    let (n, d) = frame_fraction(ns, rate);
    to_u64((2 * n + d) / (2 * d))
}

/// The frame at or before `ns`.
pub(crate) fn floor_frame(ns: u64, rate: FrameRate) -> u64 {
    let (n, d) = frame_fraction(ns, rate);
    to_u64(n / d)
}

/// The first frame at or after `ns`.
pub(crate) fn ceil_frame(ns: u64, rate: FrameRate) -> u64 {
    let (n, d) = frame_fraction(ns, rate);
    to_u64(n.div_ceil(d))
}

/// Samples from `ns` to the start of frame `frame` (which is at or after `ns`),
/// rounded to the nearest sample (half up).
pub(crate) fn samples_to_frame(ns: u64, frame: u64, rate: FrameRate, sample_rate: u32) -> u64 {
    let (num, den) = parts(rate);
    let sr = u128::from(sample_rate);
    // frame * den / num seconds minus ns / 1e9 seconds, times sr.
    let a = u128::from(frame) * den * sr * NANOS;
    let b = u128::from(ns) * sr * num;
    let diff = a.saturating_sub(b);
    let d = num * NANOS;
    to_u64((2 * diff + d) / (2 * d))
}

/// `ns` in whole samples, nearest (half up).
pub(crate) fn nearest_sample(ns: u64, sample_rate: u32) -> u64 {
    let n = u128::from(ns) * u128::from(sample_rate);
    to_u64((2 * n + NANOS) / (2 * NANOS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_text_is_reduced() {
        assert_eq!(seconds(0, 7), "0s");
        assert_eq!(seconds(10, 2), "5s");
        assert_eq!(frames_text(1, FrameRate::FPS_59_94), "1001/60000s");
        assert_eq!(frames_text(60, FrameRate::FPS_30), "2s");
        assert_eq!(samples_text(24_000, 48_000), "1/2s");
    }

    #[test]
    fn frames_round_as_the_spec_says() {
        let r = FrameRate::FPS_25; // 40 ms frames
        assert_eq!(nearest_frame(19_999_999, r), 0);
        assert_eq!(nearest_frame(20_000_000, r), 1, "half rounds up");
        assert_eq!(floor_frame(79_999_999, r), 1);
        assert_eq!(ceil_frame(40_000_001, r), 2);
        assert_eq!(ceil_frame(40_000_000, r), 1);
        // 59.94: frame 1 starts at 16 683 333.3 ns.
        assert_eq!(ceil_frame(2_500_000, FrameRate::FPS_59_94), 1);
        // From 2.5 ms to 16.6833 ms at 48 kHz is 680.8 samples.
        assert_eq!(
            samples_to_frame(2_500_000, 1, FrameRate::FPS_59_94, 48_000),
            681
        );
        assert_eq!(nearest_sample(1_000_000_000, 48_000), 48_000);
    }
}
