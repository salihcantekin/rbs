//! A sync offset measurement on a running studio (ADR-0126 decisions 1 to 6; spec
//! `docs/specs/recording.md`, "Sync offset measurement").
//!
//! [`Studio::start_sync_measure`] adds two probes: a [`SyncProbeNode`] on the measured
//! source's mixer input producer (after its filters and its offset, before the fader)
//! and a [`SyncPictureProbeNode`] on the reference source's chain tail. Both are
//! parallel readers: nothing they read from changes. [`Studio::sync_measure`], called
//! from the host's service loop, drains their logs, estimates, and ends the measurement
//! when it has [`MEASURE_CLAPS`] pairs, when [`MEASURE_WINDOW`] has passed, or when
//! something it depends on changed; it then removes both probes. While no measurement
//! runs nothing of it exists (ADR-0013).

use rbs_audio::{SyncProbeLog, SyncProbeNode, effective_offset_ns};
use rbs_nodes::{SyncPictureLog, SyncPictureProbeNode};
use rbs_scene::model::MAX_SYNC_OFFSET_MS;

use super::*;
use crate::sync_measure::{
    AudioEnergy, Estimate, EstimateError, MIN_PEAK_MOTION, ONSET_FLOOR_DB, PictureMotion,
    Suggestion, estimate, pair_count, suggest,
};

/// Longest measurement (ADR-0126 decision 1).
pub const MEASURE_WINDOW: Duration = Duration::from_secs(10);
/// Pairs after which a consistent measurement ends early (ADR-0126 decision 1).
pub const MEASURE_CLAPS: usize = 5;

/// Why a measurement could not start.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMeasureRefused {
    /// A measurement runs already.
    Busy,
    /// The measured source is not heard by the program mix (or has no sound).
    NotHeard,
    /// The reference source has no picture in the studio.
    NoPicture,
    /// The studio has no audio graph.
    NoAudio,
}

impl fmt::Display for SyncMeasureRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Busy => "a sync measurement runs already",
            Self::NotHeard => "the program mix does not hear this source",
            Self::NoPicture => "the reference source has no picture",
            Self::NoAudio => "the studio has no audio",
        })
    }
}

/// Why a measurement ended without a result (ADR-0126 decision 5).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMeasureFailure {
    /// Fewer than three claps heard near a movement in the picture.
    TooFewClaps,
    /// No clap seen in the picture.
    NoPictureMotion,
    /// The claps disagree by more than one frame.
    Inconsistent,
    /// The measured source's offset changed, or its input resynchronised.
    OffsetChanged,
    /// The node either probe reads from was replaced (a filter edit, a rebind).
    ChainChanged,
    /// The measured source left the program mix, or the reference its picture.
    SourceClosed,
    /// [`Studio::cancel_sync_measure`].
    Cancelled,
}

impl SyncMeasureFailure {
    fn of(e: EstimateError) -> Self {
        match e {
            EstimateError::TooFewClaps => Self::TooFewClaps,
            EstimateError::NoPictureMotion => Self::NoPictureMotion,
            EstimateError::Inconsistent => Self::Inconsistent,
        }
    }
}

/// A finished measurement's result.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncMeasureResult {
    /// The offset to offer, clamped, with the unclamped value.
    pub suggestion: Suggestion,
    /// What the estimator found.
    pub estimate: Estimate,
    /// The offset the input applied during the measurement (ms, after its clamp).
    pub effective_offset_ms: i32,
}

/// Where a measurement is.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMeasureState {
    /// Running, with the pairs found so far.
    Running {
        /// Pairs of a sound and a picture event.
        pairs: usize,
    },
    /// Ended with a result. Nothing was applied.
    Done(SyncMeasureResult),
    /// Ended without a result. Nothing changed.
    Failed(SyncMeasureFailure),
}

/// One measurement's status.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncMeasureStatus {
    /// The id [`Studio::start_sync_measure`] returned.
    pub id: u64,
    /// The measured audio source.
    pub source: SourceId,
    /// The reference picture source.
    pub reference: SourceId,
    /// Where it is.
    pub state: SyncMeasureState,
}

/// What a running measurement has collected ([`Studio::sync_measure_signals`]).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncMeasureSignals {
    /// Time since the start.
    pub elapsed: Duration,
    /// Some sound was loud enough to be a clap.
    pub sound: bool,
    /// Some picture moved enough to be a clap.
    pub picture: bool,
}

/// The probes and what they collected while a measurement runs.
pub(super) struct SyncMeasureRun {
    id: u64,
    source: SourceId,
    reference: SourceId,
    audio: Added,
    picture: Added,
    audio_log: Arc<SyncProbeLog>,
    picture_log: Arc<SyncPictureLog>,
    audio_from: usize,
    picture_from: usize,
    energy: Vec<AudioEnergy>,
    frames: Vec<PictureMotion>,
    /// The nodes the probes read from at the start.
    producer: NodeId,
    reference_tail: NodeId,
    started: Timestamp,
    /// Some sound reached the onset floor.
    heard: bool,
    /// Some picture moved enough to count as a clap.
    seen: bool,
    effective_offset_ms: i32,
    stored_offset_ns: i64,
    resyncs: Option<u64>,
    /// Set by `set_audio_offset` for the measured source.
    offset_changed: bool,
}

impl Studio {
    /// Starts measuring `source`'s sync offset against `reference`'s picture
    /// (ADR-0126). Returns the measurement's id. The user claps in view of the
    /// reference; [`Self::sync_measure`] reports progress and the result. Refused
    /// while one runs, when the program mix does not hear `source`, or when
    /// `reference` has no picture; on any failure nothing of it stays.
    pub fn start_sync_measure(
        &mut self,
        source: SourceId,
        reference: SourceId,
    ) -> Result<u64, RecordError> {
        if self.sync_measure.is_some() {
            return Err(RecordError::SyncMeasure(SyncMeasureRefused::Busy));
        }
        let Some(mixer) = &self.bound_mixer else {
            return Err(RecordError::SyncMeasure(SyncMeasureRefused::NoAudio));
        };
        let Some(producer) = mixer
            .inputs
            .iter()
            .find(|input| input.source == source)
            .map(|input| input.producer)
        else {
            return Err(RecordError::SyncMeasure(SyncMeasureRefused::NotHeard));
        };
        let Some((reference_tail, _)) = self.binder.chain_tail(reference) else {
            return Err(RecordError::SyncMeasure(SyncMeasureRefused::NoPicture));
        };
        let audio_pin = super::iso_audio::audio_pin(&self.graph, producer)?;
        let picture_pin = super::iso_video::video_pin(&self.graph, reference_tail)
            .map_err(|_| RecordError::SyncMeasure(SyncMeasureRefused::NoPicture))?;

        let (audio_node, audio_log) = SyncProbeNode::new(self.settings.audio_graph, MEASURE_WINDOW);
        let (picture_node, picture_log) = SyncPictureProbeNode::new(MEASURE_WINDOW);
        let partition = self.settings.partitions.audio;
        let mut graph = self.graph.clone();
        let audio_id = add_active(&mut graph, Node::<()>::descriptor(&audio_node), partition)?;
        let picture_id = add_active(
            &mut graph,
            Node::<GpuRuntime>::descriptor(&picture_node),
            RENDER,
        )?;
        graph.connect(producer, &audio_pin, audio_id, "in")?;
        graph.connect(reference_tail, &picture_pin, picture_id, "picture")?;
        let audio = Added {
            node: audio_id,
            partition,
            gpu: false,
        };
        let picture = Added {
            node: picture_id,
            partition: RENDER,
            gpu: true,
        };
        let mut adopted = Vec::with_capacity(2);
        let result = (|| -> Result<(), RecordError> {
            self.engine
                .adopt::<()>(partition, audio_id, Box::new(audio_node))?;
            adopted.push(audio);
            self.engine
                .adopt::<GpuRuntime>(RENDER, picture_id, Box::new(picture_node))?;
            adopted.push(picture);
            self.engine.apply(&graph)?;
            Ok(())
        })();
        if let Err(e) = result {
            for a in adopted {
                if !take(&mut self.engine, a) {
                    self.pending_takes.push((a, None));
                }
            }
            return Err(e);
        }
        self.graph = graph;

        let stored_offset_ns = self
            .settings
            .audio_offsets
            .get(&source)
            .copied()
            .unwrap_or(0);
        // What the program hears: the input's offset plus the latency of the source's
        // filters (ADR-0126 decision 4 as amended by ADR-0130 decision 4).
        let effective = self.heard_offset_ns(source);
        self.sync_measure_next_id += 1;
        let id = self.sync_measure_next_id;
        self.sync_measure = Some(SyncMeasureRun {
            id,
            source,
            reference,
            audio,
            picture,
            audio_log,
            picture_log,
            audio_from: 0,
            picture_from: 0,
            energy: Vec::with_capacity(MEASURE_WINDOW.as_millis() as usize + 64),
            frames: Vec::with_capacity(2_400),
            producer,
            reference_tail,
            started: self.engine.clock().now(),
            heard: false,
            seen: false,
            effective_offset_ms: i32::try_from(effective / 1_000_000).unwrap_or(0),
            stored_offset_ns,
            resyncs: self.input_resyncs(source),
            offset_changed: false,
        });
        Ok(id)
    }

    /// Cancels the running measurement and removes its probes. Returns its final
    /// status (`Failed(Cancelled)`), or `None` when none runs.
    pub fn cancel_sync_measure(&mut self) -> Option<SyncMeasureStatus> {
        let run = self.sync_measure.as_ref()?;
        let status = SyncMeasureStatus {
            id: run.id,
            source: run.source,
            reference: run.reference,
            state: SyncMeasureState::Failed(SyncMeasureFailure::Cancelled),
        };
        self.end_sync_measure();
        Some(status)
    }

    /// The running measurement's status, read from its probes now; call it from the
    /// service loop. When the measurement ends (a result, a failure) its probes are
    /// removed and the final status is returned once; after that `None` until the
    /// next start.
    pub fn sync_measure(&mut self) -> Option<SyncMeasureStatus> {
        let now = self.engine.clock().now();
        let failure = self.sync_measure_broken();
        let resyncs = self
            .sync_measure
            .as_ref()
            .and_then(|r| self.input_resyncs(r.source));
        let run = self.sync_measure.as_mut()?;
        let mut buf = Vec::new();
        run.audio_from = run.audio_log.read_from(run.audio_from, &mut buf);
        run.energy.extend(
            buf.iter()
                .map(|&(time_ns, energy)| AudioEnergy { time_ns, energy }),
        );
        let floor = 10f32.powf(ONSET_FLOOR_DB / 10.0);
        run.heard |= buf.iter().any(|&(_, energy)| energy >= floor);
        buf.clear();
        run.picture_from = run.picture_log.read_from(run.picture_from, &mut buf);
        run.frames.extend(
            buf.iter()
                .map(|&(time_ns, motion)| PictureMotion { time_ns, motion }),
        );
        run.seen |= buf.iter().any(|&(_, motion)| motion >= MIN_PEAK_MOTION);

        let resynced = run.resyncs.is_some() && resyncs != run.resyncs;
        let failure = failure
            .or((run.offset_changed || resynced).then_some(SyncMeasureFailure::OffsetChanged));
        let elapsed = now.as_nanos().saturating_sub(run.started.as_nanos());
        let window_over = u128::from(elapsed) >= MEASURE_WINDOW.as_nanos();
        let state = match failure {
            Some(f) => SyncMeasureState::Failed(f),
            None => match estimate(&run.energy, &run.frames) {
                Ok(e) if e.pairs >= MEASURE_CLAPS || window_over => {
                    SyncMeasureState::Done(SyncMeasureResult {
                        suggestion: suggest(
                            run.effective_offset_ms,
                            &e,
                            advance_limit_ms(&self.settings.audio_graph),
                            MAX_SYNC_OFFSET_MS,
                        ),
                        estimate: e,
                        effective_offset_ms: run.effective_offset_ms,
                    })
                }
                Err(e) if window_over => SyncMeasureState::Failed(SyncMeasureFailure::of(e)),
                _ => SyncMeasureState::Running {
                    pairs: pair_count(&run.energy, &run.frames),
                },
            },
        };
        let status = SyncMeasureStatus {
            id: run.id,
            source: run.source,
            reference: run.reference,
            state,
        };
        if !matches!(state, SyncMeasureState::Running { .. }) {
            self.end_sync_measure();
        }
        Some(status)
    }

    /// What the running measurement has collected: milliseconds of sound and
    /// pictures logged (each pulled in by [`Self::sync_measure`]); `None` when none
    /// runs. A UI can show that it hears and sees something.
    pub fn sync_measure_collected(&self) -> Option<(usize, usize)> {
        self.sync_measure
            .as_ref()
            .map(|r| (r.energy.len(), r.frames.len()))
    }

    /// What the running measurement has heard and seen so far, read by the last
    /// [`Self::sync_measure`]: `None` when none runs. A UI shows it so a muted
    /// microphone or a covered camera is plain before the window ends.
    pub fn sync_measure_signals(&self) -> Option<SyncMeasureSignals> {
        let now = self.engine.clock().now();
        self.sync_measure.as_ref().map(|r| SyncMeasureSignals {
            elapsed: Duration::from_nanos(now.as_nanos().saturating_sub(r.started.as_nanos())),
            sound: r.heard,
            picture: r.seen,
        })
    }

    /// Nodes in the studio's graph. For tests that check nothing stays behind.
    #[doc(hidden)]
    pub fn graph_node_count(&self) -> usize {
        self.graph.nodes().len()
    }

    /// Notes a changed offset of the measured source (called by `set_audio_offset`).
    pub(super) fn sync_measure_offset_set(&mut self, source: SourceId, offset_ns: i64) {
        if let Some(run) = self.sync_measure.as_mut()
            && run.source == source
            && run.stored_offset_ns != offset_ns
        {
            run.offset_changed = true;
        }
    }

    /// Removes the probes, if a measurement runs.
    pub(super) fn end_sync_measure(&mut self) {
        let Some(run) = self.sync_measure.take() else {
            return;
        };
        for added in [run.audio, run.picture] {
            let _ = self.graph.set_state(added.node, Lifecycle::Standby);
            let _ = self.graph.remove_node(added.node);
        }
        let _ = self.engine.apply(&self.graph);
        for added in [run.audio, run.picture] {
            if !take(&mut self.engine, added) {
                self.pending_takes.push((added, None));
            }
        }
    }

    /// A reason the running measurement can no longer hold: its source left the mix,
    /// the reference lost its picture, or a node a probe reads from was replaced.
    fn sync_measure_broken(&self) -> Option<SyncMeasureFailure> {
        let run = self.sync_measure.as_ref()?;
        let producer = self
            .bound_mixer
            .as_ref()
            .and_then(|m| m.inputs.iter().find(|i| i.source == run.source))
            .map(|i| i.producer);
        let tail = self.binder.chain_tail(run.reference).map(|(n, _)| n);
        let (Some(producer), Some(tail)) = (producer, tail) else {
            return Some(SyncMeasureFailure::SourceClosed);
        };
        let linked = |from: NodeId, to: NodeId| {
            self.graph
                .links()
                .iter()
                .any(|l| l.from.node == from && l.to.node == to)
        };
        if producer != run.producer
            || tail != run.reference_tail
            || !linked(producer, run.audio.node)
            || !linked(tail, run.picture.node)
        {
            return Some(SyncMeasureFailure::ChainChanged);
        }
        None
    }

    /// The resync count of `source`'s audio input, when it has one running.
    fn input_resyncs(&self, source: SourceId) -> Option<u64> {
        self.entries
            .iter()
            .filter(|e| e.source == source)
            .find_map(|e| match e.instance.as_ref().map(|i| &i.watch) {
                Some(Watch::Audio(stats, _)) => Some(stats.read().resyncs),
                _ => None,
            })
    }
}

/// The largest advance (ms) an input of `config` applies.
fn advance_limit_ms(config: &AudioGraphConfig) -> i32 {
    let advance = effective_offset_ns(config, Duration::ZERO, i64::MIN / 2);
    i32::try_from(-advance / 1_000_000).unwrap_or(0)
}
