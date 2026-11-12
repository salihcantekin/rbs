//! Lock-free level meters: the engine records peaks and RMS, the UI takes them at its own rate.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use rbs_core::media::MAX_CHANNELS;

/// Peak levels per channel since the UI last looked.
///
/// The audio thread calls [`record`](LevelMeter::record) with each block's absolute
/// peak per channel; it keeps the maximum with `fetch_max` on the `f32` bits, which
/// orders like the values for non-negative floats. The UI calls
/// [`take`](LevelMeter::take) at its redraw rate and gets the highest peak since its
/// last take, so a clip between two redraws is never missed. Neither side allocates or
/// waits.
#[derive(Debug)]
pub struct LevelMeter {
    /// Highest peak since the UI last called [`take`](Self::take).
    peaks: [AtomicU32; MAX_CHANNELS],
    /// Highest peak since a control-API sample last called [`observe`](Self::observe).
    /// Written beside `peaks` so a subscriber does not steal the UI's peaks, and the
    /// UI does not steal a subscriber's. One extra atomic per channel per block, with
    /// no branch for "is anyone listening" (P13).
    shared: [AtomicU32; MAX_CHANNELS],
    /// Sum of squares and sample count since [`take`](Self::take).
    energy: [AtomicU64; MAX_CHANNELS],
    energy_n: [AtomicU32; MAX_CHANNELS],
    /// The same, since [`observe`](Self::observe). Independent of the UI bank.
    shared_energy: [AtomicU64; MAX_CHANNELS],
    shared_energy_n: [AtomicU32; MAX_CHANNELS],
    channels: usize,
}

impl LevelMeter {
    /// A meter for `channels` channels (clamped to `1..=MAX_CHANNELS`).
    pub fn new(channels: usize) -> Self {
        Self {
            peaks: Default::default(),
            shared: Default::default(),
            energy: Default::default(),
            energy_n: Default::default(),
            shared_energy: Default::default(),
            shared_energy_n: Default::default(),
            channels: channels.clamp(1, MAX_CHANNELS),
        }
    }

    /// The number of channels.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Records an absolute peak for `channel`. NaN, negative values and channels out of
    /// range are ignored. Real-time safe.
    #[inline]
    pub fn record(&self, channel: usize, peak: f32) {
        if channel >= self.channels || peak.is_nan() || peak <= 0.0 {
            return;
        }
        let peak = if peak.is_infinite() { f32::MAX } else { peak };
        let bits = peak.to_bits();
        self.peaks[channel].fetch_max(bits, Ordering::Relaxed);
        self.shared[channel].fetch_max(bits, Ordering::Relaxed);
    }

    /// Records one block: its absolute `peak`, and `sum_sq` over `frames` samples
    /// (the square of each sample, already scaled as the peak is). Real-time safe:
    /// one max and one add per bank, no allocation. A torn add against a reader
    /// loses at most this block.
    #[inline]
    pub fn record_block(&self, channel: usize, peak: f32, sum_sq: f32, frames: u32) {
        self.record(channel, peak);
        if channel >= self.channels || frames == 0 || sum_sq.is_nan() || sum_sq < 0.0 {
            return;
        }
        Self::add(
            &self.energy[channel],
            &self.energy_n[channel],
            sum_sq,
            frames,
        );
        Self::add(
            &self.shared_energy[channel],
            &self.shared_energy_n[channel],
            sum_sq,
            frames,
        );
    }

    fn add(sum: &AtomicU64, count: &AtomicU32, sum_sq: f32, frames: u32) {
        let mut cur = sum.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(cur) + f64::from(sum_sq)).to_bits();
            match sum.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(seen) => cur = seen,
            }
        }
        count.fetch_add(frames, Ordering::Relaxed);
    }

    /// The peaks recorded since the last take, and resets them to 0.
    pub fn take(&self) -> Levels {
        Self::swap(&self.peaks, &self.energy, &self.energy_n, self.channels)
    }

    /// The peaks recorded since the last observe, and resets them to 0.
    ///
    /// For a reader other than the UI (the control API's `audio.levels` sampler).
    /// Independent of [`take`](Self::take): each keeps the highest peak since its own
    /// last read.
    pub fn observe(&self) -> Levels {
        Self::swap(
            &self.shared,
            &self.shared_energy,
            &self.shared_energy_n,
            self.channels,
        )
    }

    fn swap(
        slots: &[AtomicU32; MAX_CHANNELS],
        energy: &[AtomicU64; MAX_CHANNELS],
        energy_n: &[AtomicU32; MAX_CHANNELS],
        channels: usize,
    ) -> Levels {
        let mut peaks = [0.0; MAX_CHANNELS];
        let mut rms = [0.0; MAX_CHANNELS];
        for i in 0..channels {
            peaks[i] = f32::from_bits(slots[i].swap(0, Ordering::Relaxed));
            let sum = f64::from_bits(energy[i].swap(0, Ordering::Relaxed));
            let n = energy_n[i].swap(0, Ordering::Relaxed);
            if n > 0 && sum > 0.0 {
                rms[i] = (sum / f64::from(n)).sqrt() as f32;
            }
        }
        Levels {
            peaks,
            rms,
            channels,
        }
    }
}

/// Peaks per channel, linear amplitude (1.0 is 0 dBFS).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Levels {
    peaks: [f32; MAX_CHANNELS],
    /// Root mean square since the last read, linear amplitude.
    rms: [f32; MAX_CHANNELS],
    channels: usize,
}

impl Levels {
    /// The number of channels.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The peak of `channel` (0 when out of range).
    pub fn peak(&self, channel: usize) -> f32 {
        if channel < self.channels {
            self.peaks[channel]
        } else {
            0.0
        }
    }

    /// The RMS of `channel` (0 when out of range, or when no block was recorded).
    pub fn rms(&self, channel: usize) -> f32 {
        if channel < self.channels {
            self.rms[channel]
        } else {
            0.0
        }
    }

    /// The peak of `channel` in dBFS; negative infinity for silence.
    pub fn peak_db(&self, channel: usize) -> f32 {
        Self::db(self.peak(channel))
    }

    /// The RMS of `channel` in dBFS; negative infinity for silence.
    pub fn rms_db(&self, channel: usize) -> f32 {
        Self::db(self.rms(channel))
    }

    fn db(amplitude: f32) -> f32 {
        if amplitude > 0.0 {
            20.0 * amplitude.log10()
        } else {
            f32::NEG_INFINITY
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Arc;

    #[test]
    fn take_returns_the_highest_peak_since_the_last_take_and_resets() {
        let m = LevelMeter::new(2);
        m.record(0, 0.25);
        m.record(0, 0.5);
        m.record(0, 0.1);
        m.record(1, 1.5);
        m.record(1, f32::NAN);
        m.record(1, -3.0);
        m.record(5, 1.0);
        let l = m.take();
        assert_eq!((l.peak(0), l.peak(1), l.peak(2)), (0.5, 1.5, 0.0));
        assert!((l.peak_db(0) + 6.0206).abs() < 1e-3);
        let l = m.take();
        assert_eq!(l.peak(0), 0.0);
        assert_eq!(l.peak_db(0), f32::NEG_INFINITY);
    }

    #[test]
    fn a_peak_recorded_during_takes_is_seen_by_exactly_one_take() {
        let m = Arc::new(LevelMeter::new(1));
        let writer = {
            let m = Arc::clone(&m);
            std::thread::spawn(move || {
                for i in 1..=10_000u32 {
                    m.record(0, i as f32);
                }
            })
        };
        let mut seen_max = 0.0f32;
        let mut takes = 0;
        // At least one take, also when the writer finishes before this thread runs
        // (it did under load: `takes` was 0).
        loop {
            let finished = writer.is_finished();
            seen_max = seen_max.max(m.take().peak(0));
            takes += 1;
            if finished {
                break;
            }
        }
        writer.join().unwrap();
        seen_max = seen_max.max(m.take().peak(0));
        assert_eq!(seen_max, 10_000.0);
        assert!(takes > 0);
    }

    #[test]
    fn take_and_observe_keep_their_own_peaks() {
        let m = LevelMeter::new(2);
        m.record(0, 0.5);
        m.record(1, 0.25);
        let ui = m.take();
        let api = m.observe();
        assert_eq!(ui.peak(0), 0.5);
        assert_eq!(api.peak(0), 0.5);
        assert_eq!(api.peak(1), 0.25);
        m.record(0, 0.1);
        assert_eq!(m.take().peak(0), 0.1);
        assert_eq!(m.observe().peak(0), 0.1);
        assert_eq!(m.take().peak(0), 0.0);
        assert_eq!(m.observe().peak(0), 0.0);
    }

    #[test]
    fn a_block_rms_is_the_root_mean_square_and_stays_below_the_peak() {
        let m = LevelMeter::new(1);
        // Two samples: 0 and 1. Peak 1, RMS sqrt(0.5).
        m.record_block(0, 1.0, 1.0, 2);
        let api = m.observe();
        assert_eq!(api.peak(0), 1.0);
        assert!((api.rms(0) - 0.5f32.sqrt()).abs() < 1e-5);
        assert!(api.rms(0) < api.peak(0));
        // The UI bank saw the same block and does not clear the API's next one.
        assert!((m.take().rms(0) - 0.5f32.sqrt()).abs() < 1e-5);
        m.record_block(0, 0.5, 0.25, 1);
        assert!((m.observe().rms(0) - 0.5).abs() < 1e-5);
        assert_eq!(m.observe().rms(0), 0.0);
    }

    #[test]
    fn record_and_take_do_not_allocate() {
        let m = LevelMeter::new(2);
        let c = allocation_counter::measure(|| {
            for i in 0..1000 {
                m.record_block(i % 2, i as f32, (i as f32) * (i as f32), 8);
                let _ = m.take();
                let _ = m.observe();
            }
        });
        assert_eq!(c.count_total, 0);
    }
}
