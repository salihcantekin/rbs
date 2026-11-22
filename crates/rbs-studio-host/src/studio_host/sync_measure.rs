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

use super::{Host, SourceKind, Update};
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

/// The host's measurement state: the next id, the running one, the last state
/// published (kept after the end, for `sync.measure.changed` subscribers), and the
/// desktop Measure window while it is open.
#[derive(Default)]
pub(super) struct MeasureHost {
    next_id: u64,
    run: Option<MeasureRun>,
    published: Option<api::SyncMeasureChanged>,
    window: Option<SyncMeasureView>,
}

/// Why the Measure window could not start (the API's refusal reasons).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMeasureRefusal {
    /// A measurement runs already.
    Busy,
    /// The program mix does not hear the source.
    NotHeard,
    /// The reference shows no picture now.
    SourceClosed,
    /// No reference was chosen or found.
    NoReference,
    /// The reference has no picture.
    NotPicture,
    /// The source has no sound, or another refusal.
    Other,
}

/// Where the Measure window is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SyncMeasurePhase {
    /// Waiting for Start.
    Ready,
    /// Start was refused.
    Refused(SyncMeasureRefusal),
    /// Measuring.
    Running {
        /// Claps paired so far.
        pairs: u32,
        /// Some sound was loud enough to be a clap.
        sound: bool,
        /// Some picture moved enough to be a clap.
        picture: bool,
        /// Share of the longest measurement gone, 0 to 1.
        progress: f32,
    },
    /// A result.
    Done {
        /// The offset to apply.
        suggested_ms: i32,
        /// The offset measured, before the clamp.
        measured_ms: i32,
        /// The two differ.
        clamped: bool,
        /// Claps the result rests on.
        pairs: u32,
        /// How much they disagree, in milliseconds.
        spread_ms: f32,
    },
    /// No result.
    Failed {
        /// Why.
        reason: api::SyncMeasureFailure,
        /// The source has an enabled gate or ducker, which can remove claps.
        onset_filter: bool,
    },
}

/// What the desktop Measure window shows (ADR-0126 step 6).
#[derive(Debug, Clone, PartialEq)]
pub struct SyncMeasureView {
    /// The measured source.
    pub source: SourceId,
    /// Its name.
    pub name: String,
    /// Its offset now, in milliseconds.
    pub current_ms: i32,
    /// The picture sources it can be measured against, by name.
    pub references: Vec<(SourceId, String)>,
    /// The one chosen.
    pub reference: Option<SourceId>,
    /// Where it is.
    pub phase: SyncMeasurePhase,
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
    pub(super) fn drive_sync_measure(&mut self, notify: &dyn Fn(Update)) -> Option<Duration> {
        let Some(run) = self.measure.run.as_ref() else {
            // `sync.measure_cancel` ended the window's measurement.
            self.window_failed(api::SyncMeasureFailure::Cancelled, notify);
            return None;
        };
        let studio_id = run.studio_id;
        match self.studio.sync_measure() {
            Some(SyncMeasureStatus { id, state, .. }) if id == studio_id => {
                self.publish_measure(state);
                self.window_follows(state, notify);
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
                self.window_failed(api::SyncMeasureFailure::StudioRestarted, notify);
                self.measure.run = None;
                None
            }
        }
    }

    /// Opens the Measure window for `source`: the cameras to clap in view of (owner
    /// decision 2026-11-16: only cameras are listed; the API still takes any picture
    /// source), with the default one chosen.
    pub(super) fn open_sync_measure_window(&mut self, source: SourceId, notify: &dyn Fn(Update)) {
        // A measurement the window started for another source ends with it.
        if self
            .measure
            .window
            .as_ref()
            .is_some_and(|w| w.source != source)
        {
            self.close_sync_measure_window(notify);
        }
        let Some(s) = self.collection.source(source) else {
            return;
        };
        let mut references: Vec<(SourceId, String)> = self
            .collection
            .sources()
            .filter(|r| SourceKind::of(&r.type_id) == Some(SourceKind::Camera))
            .map(|r| (r.id, r.name.clone()))
            .collect();
        references.sort_by_key(|r| r.1.to_lowercase());
        let reference = self.default_reference(source);
        let view = SyncMeasureView {
            source,
            name: s.name.clone(),
            current_ms: s.audio.sync_offset_ms,
            references,
            reference,
            phase: SyncMeasurePhase::Ready,
        };
        self.set_window(Some(view), notify);
    }

    /// The window's `sync.measure_start` of its Measure view's source ended with
    /// `result` (ADR-0168 decision 2): the view shows the reference and the phase.
    pub(super) fn measure_view_started(
        &mut self,
        source: SourceId,
        reference: Option<SourceId>,
        result: Result<(), &ApiError>,
        notify: &dyn Fn(Update),
    ) {
        let Some(mut view) = self.measure.window.clone().filter(|v| v.source == source) else {
            return;
        };
        if reference.is_some() {
            view.reference = reference;
        }
        view.phase = match result {
            Ok(()) => SyncMeasurePhase::Running {
                pairs: 0,
                sound: false,
                picture: false,
                progress: 0.0,
            },
            Err(e) => SyncMeasurePhase::Refused(
                match e
                    .data
                    .as_ref()
                    .and_then(|d| d.get("reason"))
                    .and_then(|r| r.as_str())
                {
                    Some("busy") => SyncMeasureRefusal::Busy,
                    Some("not_heard") => SyncMeasureRefusal::NotHeard,
                    Some("source_closed") => SyncMeasureRefusal::SourceClosed,
                    Some("no_reference") => SyncMeasureRefusal::NoReference,
                    Some("not_picture") => SyncMeasureRefusal::NotPicture,
                    _ => SyncMeasureRefusal::Other,
                },
            ),
        };
        self.set_window(Some(view), notify);
    }

    /// The Measure window closed (Cancel, Apply, or the window's close button): a
    /// measurement of its source still running is cancelled.
    pub(super) fn close_sync_measure_window(&mut self, notify: &dyn Fn(Update)) {
        let Some(view) = self.measure.window.take() else {
            return;
        };
        // `sync.measure_cancel`, built from the measurement that runs (ADR-0168
        // decision 1): whoever started it, as before.
        if let Some(run) = &self.measure.run
            && run.source == view.source
        {
            let call = rbs_api::Command::SyncMeasureCancel(rbs_api::request::SyncMeasureCancel {
                measurement: run.id.to_string(),
            });
            let _ = self.run_api(call, notify);
        }
        notify(Update::SyncMeasure(None));
    }

    fn set_window(&mut self, view: Option<SyncMeasureView>, notify: &dyn Fn(Update)) {
        if self.measure.window == view {
            return;
        }
        self.measure.window.clone_from(&view);
        notify(Update::SyncMeasure(view));
    }

    /// The window shows the measurement of its source.
    fn window_follows(&mut self, state: SyncMeasureState, notify: &dyn Fn(Update)) {
        let Some(run) = &self.measure.run else {
            return;
        };
        let Some(mut view) = self.measure.window.clone() else {
            return;
        };
        if view.source != run.source {
            return;
        }
        view.reference = Some(run.reference);
        view.phase = match state {
            SyncMeasureState::Running { pairs } => {
                let signals = self.studio.sync_measure_signals();
                #[allow(clippy::cast_precision_loss)] // at most ten seconds in ns
                let progress = signals.map_or(0.0, |s| {
                    (s.elapsed.as_secs_f32() / rbs_record::MEASURE_WINDOW.as_secs_f32())
                        .clamp(0.0, 1.0)
                });
                SyncMeasurePhase::Running {
                    pairs: u32::try_from(pairs).unwrap_or(u32::MAX),
                    sound: signals.is_some_and(|s| s.sound),
                    picture: signals.is_some_and(|s| s.picture),
                    progress,
                }
            }
            SyncMeasureState::Done(r) => SyncMeasurePhase::Done {
                suggested_ms: r.suggestion.offset_ms,
                measured_ms: r.suggestion.measured_ms,
                clamped: r.suggestion.clamped,
                pairs: u32::try_from(r.estimate.pairs).unwrap_or(u32::MAX),
                #[allow(clippy::cast_precision_loss)] // a spread of at most seconds
                spread_ms: r.estimate.spread_ns as f32 / 1e6,
            },
            SyncMeasureState::Failed(f) => SyncMeasurePhase::Failed {
                reason: api_failure(f),
                onset_filter: self.has_onset_filter(view.source),
            },
            _ => return,
        };
        if let Some(s) = self.collection.source(view.source) {
            view.current_ms = s.audio.sync_offset_ms;
        }
        self.set_window(Some(view), notify);
    }

    fn window_failed(&mut self, reason: api::SyncMeasureFailure, notify: &dyn Fn(Update)) {
        let Some(mut view) = self.measure.window.clone() else {
            return;
        };
        if !matches!(view.phase, SyncMeasurePhase::Running { .. }) {
            return;
        }
        view.phase = SyncMeasurePhase::Failed {
            reason,
            onset_filter: self.has_onset_filter(view.source),
        };
        self.set_window(Some(view), notify);
    }

    /// The source's chain has an enabled gate or ducker, which can remove claps.
    fn has_onset_filter(&self, source: SourceId) -> bool {
        self.collection.source(source).is_some_and(|s| {
            s.filters.iter().any(|f| {
                f.enabled
                    && (f.type_id.as_str() == rbs_audio_filters::gate::NOISE_GATE
                        || f.type_id.as_str() == rbs_audio_filters::ducker::DUCKER)
            })
        })
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
