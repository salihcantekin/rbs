//! `audio.levels` for the control API (control-api spec).
//!
//! The studio's control thread publishes the [`LevelMeter`](rbs_audio::LevelMeter) of
//! each running audio source. The API thread samples them only while a client is
//! subscribed, and never takes the peaks the Mixer is reading.

use std::sync::{Arc, RwLock, TryLockError};

use rbs_api::types::SourceLevels;
use rbs_api::{EventPayload, Sampler, Topic};
use rbs_audio::LevelMeter;

/// Silence on the wire. JSON has no non-finite numbers, and a true −∞ dBFS would
/// become `null`. −120 dBFS is the floor the noise gate already uses.
const SILENCE_DB: f32 = -120.0;

/// The meters a subscribed client can sample. Shared by the control thread, which
/// replaces the list when the running sources change, and the API thread, which only
/// reads it.
#[derive(Debug)]
pub struct LevelFeed {
    slots: RwLock<Vec<(u32, Arc<LevelMeter>)>>,
}

impl LevelFeed {
    /// No sources yet.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            slots: RwLock::new(Vec::new()),
        })
    }

    /// Replaces the list when the meters are not already these, in this order.
    pub fn replace_if_changed(&self, next: Vec<(u32, Arc<LevelMeter>)>) {
        let mut guard = self.slots.write().unwrap_or_else(|p| p.into_inner());
        if same(&guard, &next) {
            return;
        }
        *guard = next;
    }

    fn sample_levels(&self) -> Option<EventPayload> {
        let guard = match self.slots.try_read() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => return None,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
        };
        if guard.is_empty() {
            return None;
        }
        let rows = guard
            .iter()
            .map(|(id, meter)| {
                let levels = meter.observe();
                let peak_db: Vec<f32> = (0..levels.channels())
                    .map(|ch| db(levels.peak(ch)))
                    .collect();
                let rms_db: Vec<f32> = (0..levels.channels())
                    .map(|ch| db(levels.rms(ch)))
                    .collect();
                SourceLevels {
                    source: *id,
                    rms_db,
                    peak_db,
                }
            })
            .collect();
        Some(EventPayload::AudioLevels(rows))
    }
}

/// [`Sampler`] over a [`LevelFeed`]. The API thread owns the box; the control thread
/// keeps the `Arc`.
pub struct LevelSampler(pub Arc<LevelFeed>);

impl Sampler for LevelSampler {
    fn sample(&mut self, topic: Topic) -> Option<EventPayload> {
        if topic == Topic::AudioLevels {
            self.0.sample_levels()
        } else {
            None
        }
    }
}

fn same(current: &[(u32, Arc<LevelMeter>)], next: &[(u32, Arc<LevelMeter>)]) -> bool {
    current.len() == next.len()
        && current
            .iter()
            .zip(next)
            .all(|(a, b)| a.0 == b.0 && Arc::ptr_eq(&a.1, &b.1))
}

fn db(peak: f32) -> f32 {
    if peak > 0.0 {
        (20.0 * peak.log10()).max(SILENCE_DB)
    } else {
        SILENCE_DB
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subscribed_sample_reads_peaks_without_clearing_the_ui_take() {
        let meter = Arc::new(LevelMeter::new(2));
        meter.record_block(0, 0.5, 0.25, 1);
        meter.record_block(1, 0.25, 0.01, 1);
        let feed = LevelFeed::new();
        feed.replace_if_changed(vec![(7, Arc::clone(&meter))]);
        let Some(EventPayload::AudioLevels(rows)) =
            LevelSampler(Arc::clone(&feed)).sample(Topic::AudioLevels)
        else {
            panic!("expected levels");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, 7);
        assert!((rows[0].peak_db[0] + 6.0206).abs() < 1e-3);
        assert!((rows[0].rms_db[0] + 6.0206).abs() < 1e-3);
        assert!(rows[0].rms_db[1] < rows[0].peak_db[1]);
        assert_eq!(meter.take().peak(0), 0.5);
        assert!(LevelSampler(feed).sample(Topic::RecordState).is_none());
    }

    #[test]
    fn silence_is_a_finite_floor_and_an_empty_feed_sends_nothing() {
        let meter = Arc::new(LevelMeter::new(1));
        let feed = LevelFeed::new();
        assert!(
            LevelSampler(Arc::clone(&feed))
                .sample(Topic::AudioLevels)
                .is_none()
        );
        feed.replace_if_changed(vec![(1, meter)]);
        let Some(EventPayload::AudioLevels(rows)) = LevelSampler(feed).sample(Topic::AudioLevels)
        else {
            panic!("expected levels");
        };
        assert_eq!(rows[0].peak_db, vec![SILENCE_DB]);
        assert_eq!(rows[0].rms_db, vec![SILENCE_DB]);
    }
}
