//! Sync offset measurement by claps (ADR-0126 decisions 4 to 6).
//!
//! Pure logic: no I/O, no clock, no allocation beyond the returned vectors. The
//! studio feeds it what its probes collected during one measurement:
//!
//! - [`AudioEnergy`]: the mean square of every 1 ms sub-block of the measured
//!   source's sound, at the block time the program hears it (after its offset);
//! - [`PictureMotion`]: one motion value per **new** frame of the reference picture,
//!   at the canvas tick time that frame first reached the tap.
//!
//! [`estimate`] finds clap onsets in the sound and contact frames in the picture,
//! pairs them and returns how late the sound is ([`Estimate::residual_ns`]).
//! [`suggest`] turns that into the offset to offer. Times are master-clock
//! nanoseconds.

use std::time::Duration;

/// Rise in 1 ms energy over the mean of the previous [`ONSET_HISTORY`] that marks a
/// clap in the sound, in dB.
pub const ONSET_DB: f32 = 12.0;
/// A sound onset must also reach this level (dBFS of the 1 ms mean square), so
/// background noise rising from near silence is not taken as a clap.
pub const ONSET_FLOOR_DB: f32 = -50.0;
/// History an onset is compared with.
pub const ONSET_HISTORY: Duration = Duration::from_millis(50);
/// After an event (sound or picture), no other event of the same kind is taken
/// within this time.
pub const REFRACTORY: Duration = Duration::from_millis(300);
/// A sound event is paired with the nearest picture event within this distance.
pub const SEARCH: Duration = Duration::from_millis(500);
/// Fewest pairs a result needs.
pub const MIN_PAIRS: usize = 3;
/// A picture peak must be at least this many times the median motion of the
/// window, so a still scene's noise is never a clap.
pub const PEAK_OVER_MEDIAN: f32 = 4.0;
/// A picture peak must also be at least this mean absolute luma difference
/// (luma in `0..=1`).
pub const MIN_PEAK_MOTION: f32 = 0.004;

/// The mean square of one 1 ms sub-block of sound, over all channels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioEnergy {
    /// Start of the sub-block on the master clock (ns).
    pub time_ns: i64,
    /// Mean square of the samples, full scale 1.0.
    pub energy: f32,
}

/// One new frame of the reference picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PictureMotion {
    /// Canvas tick time at which this frame first reached the tap (ns).
    pub time_ns: i64,
    /// Mean absolute luma difference against the previous new frame (`0..=1`).
    pub motion: f32,
}

/// What a successful measurement found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    /// How late the sound is against the picture (ns): sound time minus picture
    /// time. Positive: the sound is late.
    pub residual_ns: i64,
    /// Pairs of a sound and a picture event used.
    pub pairs: usize,
    /// Interquartile range of the pairs' residuals (ns).
    pub spread_ns: i64,
    /// Median interval between new frames in the window (ns).
    pub frame_interval_ns: i64,
}

/// Why a measurement gave no result (ADR-0126 decision 5; the studio adds the
/// reasons that end a measurement early).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EstimateError {
    /// Fewer than [`MIN_PAIRS`] sound events with a picture event near them.
    TooFewClaps,
    /// The picture had fewer than two new frames or no motion peak.
    NoPictureMotion,
    /// The pairs disagree by more than one frame interval.
    Inconsistent,
}

impl std::fmt::Display for EstimateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TooFewClaps => "too few claps were heard near a movement in the picture",
            Self::NoPictureMotion => "no clap was seen in the picture",
            Self::Inconsistent => "the claps disagree by more than one frame",
        })
    }
}

impl std::error::Error for EstimateError {}

/// The offset to offer the user (ADR-0126 decisions 4 and 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Suggestion {
    /// `effective offset - residual`, rounded to whole milliseconds, not clamped.
    pub measured_ms: i32,
    /// [`Self::measured_ms`] clamped to what the input can apply.
    pub offset_ms: i32,
    /// True when [`Self::offset_ms`] differs from [`Self::measured_ms`].
    pub clamped: bool,
}

fn ns(d: Duration) -> i64 {
    i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
}

fn db(energy: f32) -> f32 {
    10.0 * energy.max(1e-12).log10()
}

/// Sound events: the start time of every 1 ms sub-block whose energy is at least
/// [`ONSET_FLOOR_DB`] and at least [`ONSET_DB`] above the mean energy of the
/// previous [`ONSET_HISTORY`], with [`REFRACTORY`] after each. `energy` must be in
/// time order.
pub fn audio_onsets(energy: &[AudioEnergy]) -> Vec<i64> {
    let history = ns(ONSET_HISTORY);
    let refractory = ns(REFRACTORY);
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut sum = 0.0f64;
    let mut last: Option<i64> = None;
    for (i, e) in energy.iter().enumerate() {
        while start < i && energy[start].time_ns < e.time_ns - history {
            sum -= f64::from(energy[start].energy);
            start += 1;
        }
        let count = i - start;
        let ready = count > 0 && energy[start].time_ns <= e.time_ns - history / 2;
        if ready && last.is_none_or(|t| e.time_ns - t >= refractory) {
            #[allow(clippy::cast_possible_truncation)] // a mean of f32 values
            let mean = (sum / count as f64) as f32;
            let level = db(e.energy);
            if level >= ONSET_FLOOR_DB && level - db(mean) >= ONSET_DB {
                out.push(e.time_ns);
                last = Some(e.time_ns);
            }
        }
        sum += f64::from(e.energy);
    }
    out
}

fn median(values: &mut [i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

/// Picture events and the median interval between new frames.
///
/// A clap is a reversal of the hands: its frame `k`, the first that shows the hands
/// together, is a motion peak (above the previous frame, at least the next, at
/// least [`PEAK_OVER_MEDIAN`] times the window's median and [`MIN_PEAK_MOTION`]),
/// followed within two new frames by a fall to at most half of it or by a local
/// minimum. The contact happened between frames `k - 1` and `k`, so the event time
/// is `T_k - (T_k - T_(k-1)) / 2`. Within [`REFRACTORY`] only the highest peak is
/// kept. `None` with fewer than two frames.
pub fn picture_events(frames: &[PictureMotion]) -> Option<(Vec<i64>, i64)> {
    if frames.len() < 2 {
        return None;
    }
    let mut intervals: Vec<i64> = frames
        .windows(2)
        .map(|w| w[1].time_ns - w[0].time_ns)
        .collect();
    let interval = median(&mut intervals)?;
    let mut motions: Vec<f32> = frames.iter().map(|f| f.motion).collect();
    motions.sort_unstable_by(f32::total_cmp);
    let floor = (motions[motions.len() / 2] * PEAK_OVER_MEDIAN).max(MIN_PEAK_MOTION);
    let m = |i: usize| frames.get(i).map_or(0.0, |f| f.motion);
    let refractory = ns(REFRACTORY);
    // (time, peak motion) of the kept events.
    let mut events: Vec<(i64, f32)> = Vec::new();
    for k in 1..frames.len() {
        let peak = m(k);
        if peak < floor || peak <= m(k - 1) || peak < m(k + 1) {
            continue;
        }
        let settles = (k + 1..=k + 2).filter(|&j| j < frames.len()).any(|j| {
            let v = m(j);
            v <= peak / 2.0 || (v < m(j - 1) && v <= m(j + 1))
        });
        if !settles {
            continue;
        }
        let t = frames[k].time_ns - (frames[k].time_ns - frames[k - 1].time_ns) / 2;
        match events.last_mut() {
            Some(last) if t - last.0 < refractory => {
                if peak > last.1 {
                    *last = (t, peak);
                }
            }
            _ => events.push((t, peak)),
        }
    }
    Some((events.into_iter().map(|(t, _)| t).collect(), interval))
}

/// Pairs sound and picture events and estimates how late the sound is
/// (ADR-0126 decisions 4 and 5).
///
/// Each sound event is paired with the nearest picture event within [`SEARCH`]; a
/// picture event serves at most one sound event (the nearest). The residual of a
/// pair is `sound - picture`. The result is the mean of the inner half
/// (interquartile) of the residuals, and needs at least [`MIN_PAIRS`] pairs whose
/// interquartile range is at most the median frame interval.
pub fn estimate(
    energy: &[AudioEnergy],
    frames: &[PictureMotion],
) -> Result<Estimate, EstimateError> {
    let (pictures, frame_interval_ns) =
        picture_events(frames).ok_or(EstimateError::NoPictureMotion)?;
    if pictures.is_empty() {
        return Err(EstimateError::NoPictureMotion);
    }
    let search = ns(SEARCH);
    // Best (distance, residual) per picture event.
    let mut best: Vec<Option<i64>> = vec![None; pictures.len()];
    for a in audio_onsets(energy) {
        let nearest = pictures
            .iter()
            .enumerate()
            .min_by_key(|(_, p)| (a - **p).abs());
        if let Some((i, p)) = nearest {
            let r = a - p;
            if r.abs() <= search && best[i].is_none_or(|old| r.abs() < old.abs()) {
                best[i] = Some(r);
            }
        }
    }
    let mut residuals: Vec<i64> = best.into_iter().flatten().collect();
    if residuals.len() < MIN_PAIRS {
        return Err(EstimateError::TooFewClaps);
    }
    residuals.sort_unstable();
    let n = residuals.len();
    let (lo, hi) = (n / 4, n - n / 4);
    let spread_ns = residuals[hi - 1] - residuals[lo];
    if spread_ns > frame_interval_ns {
        return Err(EstimateError::Inconsistent);
    }
    let inner = &residuals[lo..hi];
    let sum: i128 = inner.iter().map(|&r| i128::from(r)).sum();
    let count = i128::try_from(inner.len()).unwrap_or(1);
    let residual_ns = i64::try_from(sum / count).unwrap_or(0);
    Ok(Estimate {
        residual_ns,
        pairs: n,
        spread_ns,
        frame_interval_ns,
    })
}

/// The offset to offer: `effective_offset_ms - residual`, rounded to whole
/// milliseconds, clamped to `-max_advance_ms..=max_delay_ms` (the scene's sync
/// offset limit and the input's advance limit). `effective_offset_ms` must be the
/// offset the input applied during the measurement, after its own clamp.
pub fn suggest(
    effective_offset_ms: i32,
    estimate: &Estimate,
    max_advance_ms: i32,
    max_delay_ms: i32,
) -> Suggestion {
    let residual_ms = (estimate.residual_ns as f64 / 1e6).round();
    #[allow(clippy::cast_possible_truncation)] // clamped into i32 range first
    let measured = (f64::from(effective_offset_ms) - residual_ms)
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
    let offset = measured.clamp(-max_advance_ms.abs(), max_delay_ms.abs());
    Suggestion {
        measured_ms: measured,
        offset_ms: offset,
        clamped: offset != measured,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    /// A deterministic generator for synthetic sessions (no flakiness, ADR-0032).
    struct Lcg(u64);
    impl Lcg {
        fn unit(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Synthetic session: claps whose hands touch at `contacts` (real time), a
    /// sound that the program hears `sound_late` after each contact, a camera with
    /// new frames every `interval` starting at `phase`, its picture shown at the
    /// tick of each frame. `rebound`: hands bounce apart after the contact.
    fn session(
        contacts: &[i64],
        sound_late: i64,
        interval: i64,
        phase: i64,
        rebound: bool,
        len: i64,
    ) -> (Vec<AudioEnergy>, Vec<PictureMotion>) {
        let mut energy = Vec::new();
        let mut t = 0;
        while t < len {
            let mut e = 1e-7f32; // room noise, -70 dBFS
            for &c in contacts {
                let d = t - (c + sound_late);
                if (0..20 * MS).contains(&d) {
                    e += 0.05 * (-(d as f32) / (5 * MS) as f32).exp();
                }
            }
            energy.push(AudioEnergy {
                time_ns: t,
                energy: e,
            });
            t += MS;
        }
        let mut frames = Vec::new();
        let mut ft = phase;
        while ft < len {
            // Frame captured at ft shows the hands together if a contact happened in
            // (ft - interval, ft]; earlier frames show approaching hands.
            let mut m = 0.001f32;
            for &c in contacts {
                let since = ft - c; // >= 0: this frame is after the contact
                if since >= 0 && since < interval {
                    m = m.max(0.08); // hands meet: biggest change
                } else if since >= interval && since < 2 * interval {
                    m = m.max(if rebound { 0.05 } else { 0.01 });
                } else if since >= 2 * interval && since < 3 * interval && rebound {
                    m = m.max(0.02);
                } else if since < 0 && since >= -3 * interval {
                    m = m.max(0.04 + 0.01 * ((since + 3 * interval) / interval) as f32);
                }
            }
            frames.push(PictureMotion {
                time_ns: ft,
                motion: m,
            });
            ft += interval;
        }
        (energy, frames)
    }

    fn claps(first: i64, gap: i64, n: usize) -> Vec<i64> {
        (0..n as i64).map(|i| first + i * gap).collect()
    }

    #[test]
    fn a_late_sound_gives_a_positive_residual_and_an_advance() {
        let interval = 33_333_333;
        // Contacts spread over the frame grid's phases.
        let contacts: Vec<i64> = (0..5)
            .map(|i| 500 * MS + i * 700 * MS + i * interval / 5)
            .collect();
        let (e, f) = session(&contacts, 20 * MS, interval, 0, false, 5_000 * MS);
        let est = estimate(&e, &f).unwrap();
        assert_eq!(est.pairs, 5);
        assert!((est.residual_ns - 20 * MS).abs() <= interval / 4, "{est:?}");
        let s = suggest(0, &est, 35, 200);
        assert!((-28..=-12).contains(&s.offset_ms) && !s.clamped, "{s:?}");
    }

    #[test]
    fn an_early_sound_gives_a_negative_residual_and_a_delay() {
        let interval = 33_333_333;
        let contacts: Vec<i64> = (0..5)
            .map(|i| 500 * MS + i * 700 * MS + i * interval / 5)
            .collect();
        let (e, f) = session(&contacts, -60 * MS, interval, 0, false, 5_000 * MS);
        let est = estimate(&e, &f).unwrap();
        assert!((est.residual_ns + 60 * MS).abs() <= interval / 4, "{est:?}");
        let s = suggest(0, &est, 35, 200);
        assert!((55..=65).contains(&s.offset_ms), "{s:?}");
    }

    #[test]
    fn the_suggestion_starts_from_the_effective_offset() {
        let est = Estimate {
            residual_ns: -30 * MS,
            pairs: 5,
            spread_ns: 0,
            frame_interval_ns: 33 * MS,
        };
        // Already delayed by 50 and still 30 early: 80.
        assert_eq!(suggest(50, &est, 35, 200).offset_ms, 80);
    }

    #[test]
    fn a_suggestion_beyond_the_limits_is_clamped_and_says_so() {
        let late = Estimate {
            residual_ns: 90 * MS,
            pairs: 5,
            spread_ns: 0,
            frame_interval_ns: 33 * MS,
        };
        let s = suggest(0, &late, 35, 200);
        assert_eq!((s.measured_ms, s.offset_ms, s.clamped), (-90, -35, true));
        let early = Estimate {
            residual_ns: -250 * MS,
            ..late
        };
        let s = suggest(0, &early, 35, 200);
        assert_eq!((s.measured_ms, s.offset_ms, s.clamped), (250, 200, true));
    }

    /// Each clap's contact is somewhere inside a frame interval, so one pair is off
    /// by up to half an interval; the inner mean of six claps at random phases is
    /// within half an interval in every session and within a quarter in about nine
    /// sessions of ten (35 to 40 of 40 for these seeds; the bound is 34).
    #[test]
    fn random_phases_stay_within_a_quarter_frame() {
        for (fps_interval, seed) in [(33_333_333i64, 1u64), (16_666_667, 2), (41_708_333, 3)] {
            let mut rng = Lcg(seed);
            let mut within_quarter = 0;
            for _ in 0..40 {
                let late = ((rng.unit() - 0.5) * 300.0) as i64 * MS;
                let phase = (rng.unit() * fps_interval as f64) as i64;
                let contacts: Vec<i64> = (0..6)
                    .map(|i| 400 * MS + i * 650 * MS + (rng.unit() * 200.0) as i64 * MS)
                    .collect();
                let (e, f) = session(&contacts, late, fps_interval, phase, false, 5_000 * MS);
                let est = estimate(&e, &f).unwrap();
                let err = (est.residual_ns - late).abs();
                assert!(
                    err <= fps_interval / 2,
                    "late {late} interval {fps_interval}: {est:?}"
                );
                within_quarter += usize::from(err <= fps_interval / 4);
            }
            assert!(
                within_quarter >= 34,
                "{within_quarter} of 40 at {fps_interval}"
            );
        }
    }

    #[test]
    fn hands_that_rebound_still_give_the_contact_frame() {
        let interval = 33_333_333;
        let contacts: Vec<i64> = (0..5)
            .map(|i| 500 * MS + i * 700 * MS + i * interval / 5)
            .collect();
        let (e, f) = session(&contacts, 40 * MS, interval, 7 * MS, true, 5_000 * MS);
        let est = estimate(&e, &f).unwrap();
        assert!((est.residual_ns - 40 * MS).abs() <= interval / 4, "{est:?}");
    }

    #[test]
    fn a_camera_at_15_fps_in_low_light_still_measures() {
        // The studio feeds new frames only, so a slow camera gives one value per
        // 67 ms and the measured interval (not a nominal rate) sets the bias.
        let interval = 66_666_667;
        let contacts: Vec<i64> = (0..6)
            .map(|i| 400 * MS + i * 700 * MS + i * interval / 6)
            .collect();
        let (e, f) = session(&contacts, 10 * MS, interval, 0, false, 5_000 * MS);
        let est = estimate(&e, &f).unwrap();
        assert_eq!(est.frame_interval_ns, interval);
        assert!((est.residual_ns - 10 * MS).abs() <= interval / 4, "{est:?}");
    }

    #[test]
    fn jitter_and_a_missed_clap_are_tolerated() {
        let interval = 33_333_333;
        let mut rng = Lcg(9);
        let contacts: Vec<i64> = (0..6)
            .map(|i| 400 * MS + i * 700 * MS + i * interval / 6)
            .collect();
        let (e, mut f) = session(&contacts, 25 * MS, interval, 3 * MS, false, 5_000 * MS);
        for fr in &mut f {
            fr.time_ns += ((rng.unit() - 0.5) * 2.0 * MS as f64) as i64;
        }
        // The camera missed the third clap (motion flattened).
        let third = contacts[2];
        for fr in &mut f {
            if (fr.time_ns - third).abs() < 3 * interval {
                fr.motion = 0.001;
            }
        }
        let est = estimate(&e, &f).unwrap();
        assert_eq!(est.pairs, 5);
        assert!((est.residual_ns - 25 * MS).abs() <= interval / 4, "{est:?}");
    }

    #[test]
    fn too_few_claps_no_motion_and_inconsistent_pairs_are_errors() {
        let interval = 33_333_333;
        let (e, f) = session(
            &claps(500 * MS, 700 * MS, 2),
            0,
            interval,
            0,
            false,
            3_000 * MS,
        );
        assert_eq!(estimate(&e, &f), Err(EstimateError::TooFewClaps));

        let still: Vec<PictureMotion> = (0..90)
            .map(|i| PictureMotion {
                time_ns: i * interval,
                motion: 0.001,
            })
            .collect();
        assert_eq!(estimate(&e, &still), Err(EstimateError::NoPictureMotion));
        assert_eq!(
            estimate(&e, &still[..1]),
            Err(EstimateError::NoPictureMotion)
        );

        // Sounds that do not follow the picture: each clap heard at a different lag.
        let contacts = claps(500 * MS, 700 * MS, 5);
        let (_, f) = session(&contacts, 0, interval, 0, false, 5_000 * MS);
        let lags = [0, 120, -90, 200, -150];
        let mut e = Vec::new();
        for t in (0..5_000).map(|i| i * MS) {
            let mut v = 1e-7f32;
            for (c, lag) in contacts.iter().zip(lags) {
                let d = t - (c + lag * MS);
                if (0..20 * MS).contains(&d) {
                    v += 0.05;
                }
            }
            e.push(AudioEnergy {
                time_ns: t,
                energy: v,
            });
        }
        assert_eq!(estimate(&e, &f), Err(EstimateError::Inconsistent));
    }

    #[test]
    fn quiet_rises_and_steady_sound_are_not_onsets() {
        // Noise rising from -90 to -60 dBFS: below the floor.
        let quiet: Vec<AudioEnergy> = (0..1000)
            .map(|i| AudioEnergy {
                time_ns: i * MS,
                energy: if i < 500 { 1e-9 } else { 1e-6 },
            })
            .collect();
        assert!(audio_onsets(&quiet).is_empty());
        // A steady loud tone has one onset at its start, none after.
        let tone: Vec<AudioEnergy> = (0..2000)
            .map(|i| AudioEnergy {
                time_ns: i * MS,
                energy: if i < 500 { 1e-7 } else { 0.01 },
            })
            .collect();
        assert_eq!(audio_onsets(&tone), vec![500 * MS]);
    }
}
