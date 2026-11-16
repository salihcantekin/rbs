//! Measuring a source's sync offset with a clap in the host (ADR-0126 decisions 1, 5
//! and 10): the default reference, the API's refusals, driving the studio's
//! measurement from the control loop while one runs, and the topic
//! `sync.measure.changed`.

use std::time::Duration;

use rbs_api::error::{ApiError, ErrorKind};
use rbs_api::event::{EventPayload, StateKey, Topic};
use rbs_api::types as api;
use rbs_record::{
    RecordError, SyncMeasureFailure, SyncMeasureRefused, SyncMeasureState, SyncMeasureStatus,
};
use rbs_scene::COMPANION_OF_HINT;
use rbs_scene::model::{ItemContent, SourceId};

use super::{Host, SourceKind};
use crate::device_sync_file::{DeviceEntry, Origin};

/// How often the control loop reads a running measurement's probes.
pub(super) const MEASURE_POLL: Duration = Duration::from_millis(100);

/// The measurement the host runs: its API id and the last state published.
pub(super) struct MeasureRun {
    /// The API's id (the studio's ids restart with each studio).
    id: u64,
    /// The studio's id.
    studio_id: u64,
    source: SourceId,
    reference: SourceId,
}

/// The host's measurement state: the next id, the running one, and the last state
/// published (kept after the end, for `sync.measure.changed` subscribers).
#[derive(Default)]
pub(super) struct MeasureHost {
    next_id: u64,
    run: Option<MeasureRun>,
    published: Option<api::SyncMeasureChanged>,
}

fn refusal(kind: ErrorKind, reason: &str, message: impl Into<String>) -> ApiError {
    let mut data = serde_json::Map::new();
    data.insert("reason".into(), serde_json::Value::String(reason.into()));
    ApiError::with_data(kind, message, data)
}

fn api_failure(f: SyncMeasureFailure) -> api::SyncMeasureFailure {
    use api::SyncMeasureFailure as A;
    match f {
        SyncMeasureFailure::TooFewClaps => A::TooFewClaps,
        SyncMeasureFailure::NoPictureMotion => A::NoPictureMotion,
        SyncMeasureFailure::Inconsistent => A::Inconsistent,
        SyncMeasureFailure::OffsetChanged => A::OffsetChanged,
        SyncMeasureFailure::ChainChanged => A::ChainChanged,
        SyncMeasureFailure::SourceClosed => A::SourceClosed,
        SyncMeasureFailure::Cancelled => A::Cancelled,
        _ => A::Other,
    }
}

fn api_state(state: SyncMeasureState) -> api::SyncMeasureState {
    match state {
        SyncMeasureState::Running { pairs } => api::SyncMeasureState::Running {
            pairs: u32::try_from(pairs).unwrap_or(u32::MAX),
        },
        SyncMeasureState::Done(r) => api::SyncMeasureState::Done {
            suggested_offset_ms: r.suggestion.offset_ms,
            measured_offset_ms: r.suggestion.measured_ms,
            effective_offset_ms: r.effective_offset_ms,
            clamped: r.suggestion.clamped,
            pairs: u32::try_from(r.estimate.pairs).unwrap_or(u32::MAX),
            #[allow(clippy::cast_precision_loss)] // a spread of at most seconds
            spread_ms: r.estimate.spread_ns as f64 / 1e6,
        },
        SyncMeasureState::Failed(f) => api::SyncMeasureState::Failed {
            reason: api_failure(f),
        },
        _ => api::SyncMeasureState::Other,
    }
}

impl Host {
    /// The reference a measurement of `source` uses when none is named (ADR-0126
    /// decision 1): the camera of a camera microphone, else the only camera in the
    /// program scene.
    fn default_reference(&self, source: SourceId) -> Option<SourceId> {
        let companion = self
            .collection
            .source(source)
            .and_then(|s| s.hints.get(COMPANION_OF_HINT))
            .and_then(|id| id.parse::<u32>().ok())
            .map(SourceId::from_raw)
            .filter(|id| self.collection.source(*id).is_some());
        if companion.is_some() {
            return companion;
        }
        let scene = self.collection.scene(self.scene)?;
        let mut cameras = scene.items.iter().filter_map(|i| match i.content {
            ItemContent::Source(id) => self
                .collection
                .source(id)
                .filter(|s| SourceKind::of(&s.type_id) == Some(SourceKind::Camera))
                .map(|s| s.id),
            _ => None,
        });
        let first = cameras.next()?;
        cameras.all(|c| c == first).then_some(first)
    }

    /// `sync.measure_start`: starts measuring `source` against `reference` (or the
    /// default one). Returns the measurement's id and the reference used.
    pub(super) fn start_sync_measure(
        &mut self,
        source: SourceId,
        reference: Option<SourceId>,
    ) -> Result<(String, SourceId), ApiError> {
        let s = self.collection.source(source).ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!("There is no source {}.", source.get()),
            )
        })?;
        if !SourceKind::of(&s.type_id).is_some_and(SourceKind::is_audio) {
            return Err(refusal(
                ErrorKind::InvalidParams,
                "not_audio",
                format!("{} has no sound to measure.", s.name),
            ));
        }
        let reference = match reference {
            Some(r) => r,
            None => self.default_reference(source).ok_or_else(|| {
                refusal(
                    ErrorKind::InvalidParams,
                    "no_reference",
                    "Name the picture source to clap in view of: there is no camera of this microphone and not exactly one camera in the program scene.",
                )
            })?,
        };
        let r = self.collection.source(reference).ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!("There is no source {}.", reference.get()),
            )
        })?;
        if SourceKind::of(&r.type_id).is_none_or(SourceKind::is_audio) {
            return Err(refusal(
                ErrorKind::InvalidParams,
                "not_picture",
                format!("{} has no picture.", r.name),
            ));
        }
        if self.measure.run.is_some() {
            return Err(refusal(
                ErrorKind::Conflict,
                "busy",
                "A sync measurement runs already.",
            ));
        }
        let studio_id = self
            .studio
            .start_sync_measure(source, reference)
            .map_err(|e| match e {
                RecordError::SyncMeasure(SyncMeasureRefused::Busy) => refusal(
                    ErrorKind::Conflict,
                    "busy",
                    "A sync measurement runs already.",
                ),
                RecordError::SyncMeasure(SyncMeasureRefused::NoPicture) => refusal(
                    ErrorKind::Failed,
                    "source_closed",
                    "The reference shows no picture now.",
                ),
                RecordError::SyncMeasure(_) => refusal(
                    ErrorKind::Failed,
                    "not_heard",
                    "The program mix does not hear this source.",
                ),
                other => ApiError::new(ErrorKind::Failed, other.to_string()),
            })?;
        self.measure.next_id += 1;
        let id = self.measure.next_id;
        self.measure.run = Some(MeasureRun {
            id,
            studio_id,
            source,
            reference,
        });
        self.publish_measure(SyncMeasureState::Running { pairs: 0 });
        Ok((id.to_string(), reference))
    }

    /// `sync.measure_cancel`: cancels measurement `measurement`. `not_found` when it
    /// is not the one running.
    pub(super) fn cancel_sync_measure(&mut self, measurement: &str) -> Result<(), ApiError> {
        let running = self
            .measure
            .run
            .as_ref()
            .is_some_and(|r| r.id.to_string() == measurement);
        if !running {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("No measurement {measurement} runs."),
            ));
        }
        let _ = self.studio.cancel_sync_measure();
        self.publish_measure(SyncMeasureState::Failed(SyncMeasureFailure::Cancelled));
        self.measure.run = None;
        Ok(())
    }

    /// Reads the running measurement and publishes what changed; ends it when the
    /// studio did. The wait until the next read while one runs; `None` otherwise, so
    /// nothing is polled without a measurement (P13).
    pub(super) fn drive_sync_measure(&mut self) -> Option<Duration> {
        let run = self.measure.run.as_ref()?;
        let studio_id = run.studio_id;
        match self.studio.sync_measure() {
            Some(SyncMeasureStatus { id, state, .. }) if id == studio_id => {
                self.publish_measure(state);
                if matches!(state, SyncMeasureState::Running { .. }) {
                    return Some(MEASURE_POLL);
                }
                if let SyncMeasureState::Done(r) = state
                    && let Some(run) = &self.measure.run
                {
                    let (source, reference) = (run.source, run.reference);
                    self.offer_measured(source, r.suggestion.offset_ms, reference);
                }
                self.measure.run = None;
                None
            }
            // A new studio (a canvas change, a collection switch) has none of it.
            _ => {
                self.publish_studio_restarted();
                self.measure.run = None;
                None
            }
        }
    }

    fn publish_studio_restarted(&mut self) {
        let Some(run) = &self.measure.run else {
            return;
        };
        let changed = api::SyncMeasureChanged {
            measurement: run.id.to_string(),
            source: run.source.get(),
            reference: run.reference.get(),
            state: api::SyncMeasureState::Failed {
                reason: api::SyncMeasureFailure::StudioRestarted,
            },
        };
        self.set_measure_changed(changed);
    }

    fn publish_measure(&mut self, state: SyncMeasureState) {
        let Some(run) = &self.measure.run else {
            return;
        };
        let changed = api::SyncMeasureChanged {
            measurement: run.id.to_string(),
            source: run.source.get(),
            reference: run.reference.get(),
            state: api_state(state),
        };
        self.set_measure_changed(changed);
    }

    fn set_measure_changed(&mut self, changed: api::SyncMeasureChanged) {
        if self.measure.published.as_ref() == Some(&changed) {
            return;
        }
        let payload = changed.clone();
        self.events
            .changed(StateKey::topic(Topic::SyncMeasureChanged), || {
                EventPayload::SyncMeasureChanged(payload)
            });
        self.measure.published = Some(changed);
    }

    /// `sync.devices`: the remembered offsets, newest first.
    pub(super) fn api_sync_devices(&self) -> Vec<api::DeviceSyncEntry> {
        let mut entries: Vec<&DeviceEntry> = self.device_sync.devices.iter().collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.set_at));
        entries
            .into_iter()
            .map(|e| api::DeviceSyncEntry {
                key: api_key(&e.key),
                offset_ms: e.offset_ms,
                origin: match e.origin {
                    Origin::Measured => api::DeviceSyncOrigin::Measured,
                    Origin::Manual => api::DeviceSyncOrigin::Manual,
                },
                reference: e.reference.as_ref().map(|r| api::DeviceSyncReference {
                    key: r.key.as_ref().map(api_key),
                    name: r.name.clone(),
                }),
                set_at: e.set_at,
            })
            .collect()
    }
}

fn api_key(k: &crate::device_sync_file::DeviceKey) -> api::DeviceKey {
    api::DeviceKey {
        type_id: k.type_id.clone(),
        adapter: k.adapter.clone(),
        target: k.target.clone(),
    }
}

/// The file's key of an API key.
pub(super) fn file_key(k: api::DeviceKey) -> crate::device_sync_file::DeviceKey {
    crate::device_sync_file::DeviceKey {
        type_id: k.type_id,
        adapter: k.adapter,
        target: k.target,
    }
}
