//! The replay buffer on a running studio (ADR-0088, spec `docs/specs/recording.md`,
//! "Replay buffer").
//!
//! Arming adds a video export and encoder, an audio encoder (with `audio`) and a
//! [`ReplayRingNode`] to the graph, each on the replay partitions
//! ([`Partitions::replay`]); disarming removes them and frees the buffered past. A
//! disarmed buffer has no node, thread or memory (ADR-0013). This first version opens
//! its own encoder sessions; sharing the encoders of a running output with equal
//! settings (ADR-0088 decision 2) follows.

use std::time::Duration;

use rbs_core::media::StreamIndex;
use rbs_nodes::replay::{Clip, ClipError, ReplayLimits};
use rbs_nodes::replay_node::{ReplayHandle, ReplayRingNode, ReplayStatus, RequestError};

use super::*;

/// Audio packets per second the capacity estimate allows for (AAC at 48 kHz is about
/// 47, Opus at 20 ms is 50); generous so a different codec still fits.
const AUDIO_PACKETS_PER_SECOND: f64 = 100.0;
/// Headroom over the estimate, so the ring is bounded by `length` and not by capacity.
const CAPACITY_HEADROOM: f64 = 1.25;
/// Extra packets on top of the headroom, for very short buffers.
const CAPACITY_FLOOR: usize = 256;

/// What the replay buffer holds and how it encodes (ADR-0088 decision 3). All values
/// come from the user's saved settings.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ReplaySettings {
    /// How the video is encoded; usually the recording's (`Match the recording`).
    pub video: VideoSettings,
    /// How the audio is encoded, or `None` for a buffer without sound.
    pub audio: Option<AudioSettings>,
    /// The wanted length of the past.
    pub length: Duration,
    /// Most payload bytes the buffer may hold.
    pub memory_limit: usize,
}

impl ReplaySettings {
    /// Settings with the given encoders, length and memory limit.
    pub fn new(
        video: VideoSettings,
        audio: Option<AudioSettings>,
        length: Duration,
        memory_limit: usize,
    ) -> Self {
        Self {
            video,
            audio,
            length,
            memory_limit,
        }
    }
}

/// Packets the ring reserves room for: `length` of video at `fps` and of audio, plus
/// headroom.
pub(crate) fn ring_capacity(fps: FrameRate, length: Duration, audio: bool) -> usize {
    let per_second = fps.per_second().as_f64() + if audio { AUDIO_PACKETS_PER_SECOND } else { 0.0 };
    let n = per_second * length.as_secs_f64() * CAPACITY_HEADROOM;
    // `as` saturates; a buffer this long would be refused by the memory limit first.
    (n as usize).saturating_add(CAPACITY_FLOOR)
}

/// The armed replay buffer.
pub(super) struct ReplayRun {
    nodes: Vec<Added>,
    handle: Arc<ReplayHandle>,
    settings: ReplaySettings,
    video: Chosen,
    audio: Option<Chosen>,
    streams: Vec<StreamDescriptor>,
}

impl Studio {
    /// Arms the replay buffer: adds its encoders and ring node and applies. Fails with
    /// [`RecordError::ReplayArmed`] when it is armed already, `NoAdapter` before
    /// anything opens, and `Start` (everything removed again) if a node does not start.
    /// The studio and its outputs keep running either way.
    pub fn arm_replay(
        &mut self,
        adapters: &RecordAdapters,
        settings: &ReplaySettings,
    ) -> Result<(), RecordError> {
        if self.replay.is_some() {
            return Err(RecordError::ReplayArmed);
        }
        if settings.audio.is_some() && self.mixer.is_none() {
            return Err(RecordError::NotBound("mixer"));
        }
        let parts = self.settings.partitions.replay;
        let busy: BTreeSet<PartitionId> = self
            .pending_takes
            .iter()
            .map(|(a, _)| a.partition)
            .collect();
        if [parts.video_encoder, parts.audio_encoder, parts.output]
            .iter()
            .any(|p| busy.contains(p))
        {
            return Err(RecordError::OutputFinishing);
        }

        // Adapters first: nothing is in the graph if none can serve.
        // The replay buffer stays at the canvas size (ADR-0102): both sizes equal,
        // so prepare uses the 1:1 download. An output's size does not apply here.
        let (width, height) = self.size;
        let export = VideoExportNode::with_tone(
            settings.video.format,
            width,
            height,
            width,
            height,
            settings.video.color,
            settings.video.open_shadows,
        );
        let config = video_encoder_config(&settings.video, export.frame_format(), self.fps);
        let (encoder, video_chosen) = select::video_encoder(&adapters.video_encoders, &config)?;
        let mut streams = vec![encoder.stream().clone()];
        let audio = match &settings.audio {
            Some(a) => {
                let config = a.encoder_config(self.settings.audio_graph.format);
                let (session, chosen) = select::audio_encoder(&adapters.audio_encoders, &config)?;
                streams.push(session.stream().clone());
                Some((EncoderNode::new(session, AUDIO_ENCODER_PACKETS), chosen))
            }
            None => None,
        };
        let limits = ReplayLimits {
            length: settings.length,
            memory_limit: settings.memory_limit,
            capacity: ring_capacity(self.fps, settings.length, audio.is_some()),
        };
        let ring = ReplayRingNode::new(streams.len(), StreamIndex(0), limits);
        let handle = ring.handle();

        // The graph.
        let mut graph = self.graph.clone();
        let ring_id = add_active(&mut graph, Node::<()>::descriptor(&ring), parts.output)?;
        let export_id = add_active(&mut graph, Node::<GpuRuntime>::descriptor(&export), RENDER)?;
        let encoder_id = add_active(
            &mut graph,
            Node::<()>::descriptor(&encoder),
            parts.video_encoder,
        )?;
        graph.connect(self.compositor, "canvas", export_id, "canvas")?;
        graph.connect(export_id, "video", encoder_id, "video")?;
        graph.connect(encoder_id, "packets", ring_id, "s0")?;
        let mut nodes = vec![
            Added {
                node: export_id,
                partition: RENDER,
                gpu: true,
            },
            Added {
                node: encoder_id,
                partition: parts.video_encoder,
                gpu: false,
            },
        ];
        let audio_id = match &audio {
            Some((node, _)) => {
                let mixer = self
                    .mixer
                    .as_ref()
                    .map(|m| m.node)
                    .ok_or(RecordError::NotBound("mixer"))?;
                let id = add_active(
                    &mut graph,
                    Node::<()>::descriptor(node),
                    parts.audio_encoder,
                )?;
                graph.connect(mixer, "main", id, "in")?;
                graph.connect(id, "packets", ring_id, "s1")?;
                nodes.push(Added {
                    node: id,
                    partition: parts.audio_encoder,
                    gpu: false,
                });
                Some(id)
            }
            None => None,
        };
        let ring_added = Added {
            node: ring_id,
            partition: parts.output,
            gpu: false,
        };
        nodes.push(ring_added);

        // Instances last; one that is refused takes the adopted ones back.
        let mut adopted: Vec<Added> = Vec::new();
        let r = self
            .engine
            .adopt::<GpuRuntime>(RENDER, export_id, Box::new(export));
        self.adopt_or_undo(&mut adopted, nodes[0], r)?;
        let r = self
            .engine
            .adopt::<()>(parts.video_encoder, encoder_id, Box::new(encoder));
        self.adopt_or_undo(&mut adopted, nodes[1], r)?;
        let audio_chosen = match (audio, audio_id) {
            (Some((node, chosen)), Some(id)) => {
                let r = self
                    .engine
                    .adopt::<()>(parts.audio_encoder, id, Box::new(node));
                self.adopt_or_undo(&mut adopted, nodes[2], r)?;
                Some(chosen)
            }
            _ => None,
        };
        let r = self
            .engine
            .adopt::<()>(parts.output, ring_id, Box::new(ring));
        self.adopt_or_undo(&mut adopted, ring_added, r)?;

        self.graph = graph;
        self.replay = Some(ReplayRun {
            nodes,
            handle,
            settings: settings.clone(),
            video: video_chosen,
            audio: audio_chosen,
            streams,
        });

        // One apply schedules them (and opens sources only now scheduled).
        let failed = match self.reconcile(&adapters.sources, None, Strictness::Lenient) {
            Ok((failed, _)) => failed,
            Err(e) => {
                let _ = self.disarm_replay();
                return Err(e);
            }
        };
        let new_failed = self
            .replay
            .as_ref()
            .is_some_and(|r| r.nodes.iter().any(|a| self.failed_nodes.contains(&a.node)));
        let engine_failed = failed
            .iter()
            .any(|f| f.node == "engine" || f.node.starts_with("link"));
        if new_failed || engine_failed {
            let mut fatal: Vec<StartFailure> =
                failed.into_iter().filter(|f| f.node != "source").collect();
            for e in self.disarm_replay_errors() {
                fatal.push(StartFailure {
                    node: "rollback".to_owned(),
                    reason: e,
                });
            }
            return Err(RecordError::Start(fatal));
        }
        Ok(())
    }

    /// Takes the adopted instances back when one adoption is refused.
    fn adopt_or_undo(
        &mut self,
        adopted: &mut Vec<Added>,
        a: Added,
        r: Result<(), rbs_engine::EngineError>,
    ) -> Result<(), RecordError> {
        match r {
            Ok(()) => {
                adopted.push(a);
                Ok(())
            }
            Err(e) => {
                for a in adopted.drain(..) {
                    take(&mut self.engine, a);
                }
                Err(RecordError::from(e))
            }
        }
    }

    /// True while the replay buffer is armed.
    pub fn replay_armed(&self) -> bool {
        self.replay.is_some()
    }

    /// What the replay buffer chose, while armed.
    pub fn replay_settings(&self) -> Option<&ReplaySettings> {
        self.replay.as_ref().map(|r| &r.settings)
    }

    /// The adapters the armed buffer's encoders use: video, then audio.
    pub fn replay_adapters(&self) -> Option<(&Chosen, Option<&Chosen>)> {
        self.replay.as_ref().map(|r| (&r.video, r.audio.as_ref()))
    }

    /// The buffer's streams (video, then audio), the descriptors
    /// [`write_clip`](crate::write_clip) needs; `None` while disarmed.
    pub fn replay_streams(&self) -> Option<&[StreamDescriptor]> {
        self.replay.as_ref().map(|r| r.streams.as_slice())
    }

    /// What the buffer holds now (usable past, bytes, packets, whether memory is the
    /// limit), while armed.
    pub fn replay_status(&self) -> Option<ReplayStatus> {
        self.replay.as_ref().map(|r| r.handle.status())
    }

    /// Asks the buffer to cut a clip of the last `seconds` (the whole buffer for
    /// `None`) on its next tick; fetch it with [`Studio::take_replay_clip`]. `None`
    /// while disarmed.
    pub fn request_replay_clip(
        &self,
        seconds: Option<Duration>,
    ) -> Option<Result<(), RequestError>> {
        self.replay.as_ref().map(|r| r.handle.request_clip(seconds))
    }

    /// The clip cut for the last request, once the buffer's next tick made it. The
    /// packets are shared with the buffer, not copied; write them with
    /// [`Clip::rebased`]. `None` while disarmed or not ready.
    pub fn take_replay_clip(&self) -> Option<Result<Clip, ClipError>> {
        self.replay.as_ref().and_then(|r| r.handle.take_clip())
    }

    /// Disarms the buffer: removes its nodes in one apply, frees the buffered past and
    /// ends the reaper thread. False when it was not armed.
    pub fn disarm_replay(&mut self) -> bool {
        let armed = self.replay.is_some();
        let _ = self.disarm_replay_errors();
        armed
    }

    /// [`Studio::disarm_replay`], returning the engine's errors of the removal.
    fn disarm_replay_errors(&mut self) -> Vec<String> {
        let Some(run) = self.replay.take() else {
            return Vec::new();
        };
        for n in &run.nodes {
            let _ = self.graph.set_state(n.node, Lifecycle::Standby);
            let _ = self.graph.remove_node(n.node);
        }
        let errors = match self.engine.apply(&self.graph) {
            Ok(a) => a.errors.iter().map(ToString::to_string).collect(),
            Err(e) => vec![e.to_string()],
        };
        for n in run.nodes {
            if !take(&mut self.engine, n) {
                self.pending_takes.push((n, None));
            }
        }
        // The plan without the buffer may leave sources unscheduled (no preview); the
        // layer hints follow the new plan, as after an output stops.
        let plan = self.graph.plan();
        let scheduled: BTreeSet<NodeId> = plan.order.iter().copied().collect();
        for e in &mut self.entries {
            e.scheduled = scheduled.contains(&e.state.node);
        }
        let _ = self.layers.send(node_layers(&self.bound, &plan));
        if let Some(sp) = &mut self.scene_preview {
            let _ = sp.layers.send(node_layers(&sp.bound, &plan));
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_scales_with_length_and_audio() {
        let fps = FrameRate::FPS_60;
        let two_min = ring_capacity(fps, Duration::from_secs(120), true);
        assert_eq!(two_min, ((160.0_f64 * 120.0 * 1.25) as usize) + 256);
        assert!(ring_capacity(fps, Duration::from_secs(120), false) < two_min);
        assert_eq!(ring_capacity(fps, Duration::ZERO, true), 256);
    }
}
