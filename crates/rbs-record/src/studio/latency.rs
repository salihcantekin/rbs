//! Filter latency compensation (ADR-0130 decisions 2 and 4; spec `audio-filters.md`,
//! "Latency").
//!
//! The chain latency `L` of a source is the sum of the latencies of the audio filter
//! instances of that source that the plan schedules (enabled, not Standby). For a
//! source with an input node the studio sends the input `stored - L`, which the input
//! clamps to `a`, so the program hears the source at `h = a + L`: the stored offset
//! when the clamp does not bite. The stored offset (`StudioSettings::audio_offsets`,
//! the device memory of ADR-0126) is never changed here. A source without an input
//! node is simply late by `L`.

use rbs_audio::effective_offset_ns;
use rbs_scene::model::MAX_SYNC_OFFSET_MS;

use super::*;

/// How a source's filter latency is compensated (ADR-0130 decision 4). All in ns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct AudioLatency {
    /// `L`: the latency of the source's scheduled filters.
    pub filter_latency_ns: i64,
    /// `h = a + L`: the offset the program hears the source at; the stored offset while
    /// the compensation is complete.
    pub heard_offset_ns: i64,
    /// What the input's advance limit left of `L`: `h - clamp(stored)`, 0 when complete.
    /// A source without an input node cannot be advanced: then it is `L`.
    pub uncompensated_ns: i64,
}

/// The compensation of `latency_ns` for a source with `stored_ns` on an input of
/// `config` that delays at most `max_offset`: what the input is sent, and what the
/// program hears.
pub fn compensate(
    config: &AudioGraphConfig,
    max_offset: Duration,
    stored_ns: i64,
    latency_ns: i64,
) -> (i64, AudioLatency) {
    let sent = stored_ns.saturating_sub(latency_ns);
    let applied = effective_offset_ns(config, max_offset, sent);
    let heard = applied.saturating_add(latency_ns);
    let device = effective_offset_ns(config, max_offset, stored_ns);
    (
        sent,
        AudioLatency {
            filter_latency_ns: latency_ns,
            heard_offset_ns: heard,
            uncompensated_ns: heard.saturating_sub(device).max(0),
        },
    )
}

/// The input's longest delay (the scene's offset range).
pub(super) fn max_offset() -> Duration {
    Duration::from_millis(MAX_SYNC_OFFSET_MS.unsigned_abs().into())
}

impl Studio {
    /// The latency of `source`'s scheduled audio filters, in ns at the graph rate.
    fn chain_latency_ns(&self, source: SourceId) -> i64 {
        let frames: u64 = self
            .filters
            .iter()
            .filter(|f| f.source == source && f.scheduled)
            .filter_map(|f| match &f.instance {
                Some(FilterInstance::Audio { stats, .. }) => {
                    Some(u64::from(stats.latency_frames()))
                }
                _ => None,
            })
            .sum();
        if frames == 0 {
            return 0;
        }
        let rate = u64::from(self.settings.audio_graph.format.sample_rate.hz()).max(1);
        i64::try_from(frames.saturating_mul(1_000_000_000) / rate).unwrap_or(i64::MAX)
    }

    /// The latency of one audio filter, in ns at the graph rate: 0 unless the plan
    /// schedules it (a disabled filter is Standby) or for a video filter.
    pub fn audio_filter_latency_ns(&self, filter: FilterId) -> i64 {
        let frames = self
            .filters
            .iter()
            .find(|f| f.filter == filter && f.scheduled)
            .and_then(|f| match &f.instance {
                Some(FilterInstance::Audio { stats, .. }) => {
                    Some(u64::from(stats.latency_frames()))
                }
                _ => None,
            })
            .unwrap_or(0);
        if frames == 0 {
            return 0;
        }
        let rate = u64::from(self.settings.audio_graph.format.sample_rate.hz()).max(1);
        i64::try_from(frames.saturating_mul(1_000_000_000) / rate).unwrap_or(i64::MAX)
    }

    /// How `source`'s filter latency is compensated, or `None` for a source the studio
    /// does not run.
    pub fn audio_latency(&self, source: SourceId) -> Option<AudioLatency> {
        let entry = self.entries.iter().find(|e| e.source == source)?;
        let latency = self.chain_latency_ns(source);
        let stored = self
            .settings
            .audio_offsets
            .get(&source)
            .copied()
            .unwrap_or(0);
        let has_input = entry.instance.as_ref().is_some_and(|i| i.offset.is_some());
        if has_input {
            let (_, l) = compensate(&self.settings.audio_graph, max_offset(), stored, latency);
            Some(l)
        } else {
            Some(AudioLatency {
                filter_latency_ns: latency,
                heard_offset_ns: latency,
                uncompensated_ns: latency,
            })
        }
    }

    /// The offset the program hears `source` at (`h`); the effective offset of a sync
    /// measurement (ADR-0126 decision 4 as amended by ADR-0130).
    pub(super) fn heard_offset_ns(&self, source: SourceId) -> i64 {
        let stored = self
            .settings
            .audio_offsets
            .get(&source)
            .copied()
            .unwrap_or(0);
        let latency = self.chain_latency_ns(source);
        compensate(&self.settings.audio_graph, max_offset(), stored, latency)
            .1
            .heard_offset_ns
    }

    /// Marks the filters `scheduled` runs and sends every running input the offset that
    /// compensates its source's filters, when it changed. Never touches the stored
    /// offsets. Returns the sources whose input got a new value.
    pub(super) fn apply_input_offsets(&mut self, scheduled: &BTreeSet<NodeId>) -> Vec<SourceId> {
        for f in &mut self.filters {
            f.scheduled = scheduled.contains(&f.node);
        }
        let wanted: Vec<(SourceId, i64)> = self
            .entries
            .iter()
            .filter(|e| e.instance.as_ref().is_some_and(|i| i.offset.is_some()))
            .map(|e| {
                let stored = self
                    .settings
                    .audio_offsets
                    .get(&e.source)
                    .copied()
                    .unwrap_or(0);
                (
                    e.source,
                    stored.saturating_sub(self.chain_latency_ns(e.source)),
                )
            })
            .collect();
        let mut changed = Vec::new();
        for (source, sent) in wanted {
            for e in self.entries.iter_mut().filter(|e| e.source == source) {
                let Some(instance) = e.instance.as_mut() else {
                    continue;
                };
                let Some(offset) = instance.offset.as_mut() else {
                    continue;
                };
                if instance.sent_offset_ns != sent {
                    offset.send(input_params(sent));
                    instance.sent_offset_ns = sent;
                    changed.push(source);
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    fn config() -> AudioGraphConfig {
        AudioGraphConfig::default()
    }

    #[test]
    fn no_latency_changes_nothing() {
        for stored in [-50 * MS, -20 * MS, 0, 30 * MS] {
            let (sent, l) = compensate(&config(), max_offset(), stored, 0);
            assert_eq!(sent, stored);
            assert_eq!(
                l.heard_offset_ns,
                effective_offset_ns(&config(), max_offset(), stored)
            );
            assert_eq!(l.uncompensated_ns, 0);
        }
    }

    #[test]
    fn latency_inside_the_advance_budget_is_fully_compensated() {
        let (sent, l) = compensate(&config(), max_offset(), 0, 10 * MS);
        assert_eq!(sent, -10 * MS);
        assert_eq!((l.heard_offset_ns, l.uncompensated_ns), (0, 0));
        let (sent, l) = compensate(&config(), max_offset(), 40 * MS, 12 * MS);
        assert_eq!(sent, 28 * MS);
        assert_eq!((l.heard_offset_ns, l.uncompensated_ns), (40 * MS, 0));
    }

    #[test]
    fn the_rest_beyond_the_advance_limit_is_reported() {
        // Default graph: 35 ms of advance. A device needing -30 ms and 10 ms of
        // filters: the input applies -35, the program hears -25, 5 ms are left.
        let limit = effective_offset_ns(&config(), max_offset(), i64::MIN / 2);
        assert_eq!(limit, -35 * MS);
        let (_, l) = compensate(&config(), max_offset(), -30 * MS, 10 * MS);
        assert_eq!(l.heard_offset_ns, -25 * MS);
        assert_eq!(l.uncompensated_ns, 5 * MS);
    }

    #[test]
    fn a_stored_offset_beyond_the_limit_is_not_blamed_on_the_filters() {
        // Stored -50 is itself clamped to -35 (ADR-0126's `clamped`); the filters'
        // 10 ms cannot be advanced at all, so 10 ms are uncompensated, not 25.
        let (_, l) = compensate(&config(), max_offset(), -50 * MS, 10 * MS);
        assert_eq!(l.heard_offset_ns, -25 * MS);
        assert_eq!(l.uncompensated_ns, 10 * MS);
    }

    #[test]
    fn the_suggestion_from_the_heard_offset_is_the_devices_own_value() {
        // ADR-0130 decision 4: residual r = h - X, so h - r = X with or without clamp.
        for (stored, latency, device_needs) in [
            (0, 10 * MS, -8 * MS),
            (-30 * MS, 10 * MS, -30 * MS),
            (-50 * MS, 10 * MS, -20 * MS),
        ] {
            let (_, l) = compensate(&config(), max_offset(), stored, latency);
            let residual = l.heard_offset_ns - device_needs;
            assert_eq!(l.heard_offset_ns - residual, device_needs);
        }
    }
}
