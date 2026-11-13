//! Separate audio files (ADR-0086 step 2). One AAC file per source marked for audio,
//! in the caller's container, tapped after that source's filters and before the fader.
//! Wav and Flac wait for ADR-0085. The files are not [`super::MAX_OUTPUTS`] outputs.

use std::path::PathBuf;
use std::sync::Arc;

use rbs_audio::EncoderNode;
use rbs_codec::{AudioEncoder, AudioEncoderConfig};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::ContainerId;
use rbs_engine::{DEFAULT_DOORBELL_TIMER, Pacing};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::LinkKind;
use rbs_graph::runtime::Node;
use rbs_nodes::OutputNode;
use rbs_scene::model::SourceId;
use rbs_transport::{TransportConfig, TransportInput};

use super::{AUDIO_ENCODER_PACKETS, Added};
use crate::iso::{self, IsoError, IsoRole, RecordingManifest};
use crate::recording::{
    AudioSettings, ISO_AUDIO_ENCODER, ISO_OUTPUT, RecordAdapters, RecordError, StartFailure,
    add_active,
};
use crate::select::{self, scheme_of};

/// What [`super::Studio::start_iso_audio`] opens. Audio defaults to AAC at 160 kbit/s,
/// the master's usual audio settings.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoAudioStart {
    /// Folder URL the files are written under, for example `file:///C:/Videos/take`
    /// or `mem://iso`. No trailing name: each file is `{folder}/{name}.{ext}`.
    pub folder_url: String,
    /// The master's container. AAC is carried in it; Wav and Flac are not this step.
    pub container: ContainerId,
    /// Codec and bitrate. A lossy codec (AAC) is what this step can open.
    pub audio: AudioSettings,
    /// Master file name in the manifest. One name, not a path. This call does not
    /// open that file.
    pub master_name: String,
    /// Wall-clock UTC of `t0`, as the caller formatted it. Must not be empty.
    pub t0_utc: String,
    /// `t0` on the master clock, in nanoseconds.
    pub t0_ns: u64,
    /// When set, `recording.rbs.toml` is written here after the audio files are open.
    pub manifest_dir: Option<PathBuf>,
}

impl IsoAudioStart {
    /// `folder_url` and `container`, AAC at 160 kbit/s, no manifest directory.
    pub fn new(
        folder_url: impl Into<String>,
        container: ContainerId,
        master_name: impl Into<String>,
        t0_utc: impl Into<String>,
        t0_ns: u64,
    ) -> Self {
        Self {
            folder_url: folder_url.into(),
            container,
            audio: AudioSettings::default(),
            master_name: master_name.into(),
            t0_utc: t0_utc.into(),
            t0_ns,
            manifest_dir: None,
        }
    }
}

/// What [`super::Studio::start_iso_audio`] opened. `audio_tracks` is 0 when nothing
/// was marked for audio: no node and no worker were added. `video_pending` counts
/// video flags, which this step lists and does not open.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoAudioStarted {
    /// The manifest at start. Audio names include the container extension.
    pub manifest: RecordingManifest,
    /// Audio files opened.
    pub audio_tracks: usize,
    /// Video flags named in the manifest and not opened.
    pub video_pending: usize,
}

pub(super) struct IsoAudioRun {
    nodes: Vec<Added>,
}

/// A source the program mix is hearing, and the file this step opens for it.
struct Tap {
    source: SourceId,
    producer: NodeId,
    pin: String,
    file_name: String,
}

impl super::Studio {
    /// Opens one audio-only file per source marked for separate audio, and returns
    /// the start manifest. The tap is that source's mixer input (after its filters,
    /// before the fader and mute). Files share [`ISO_AUDIO_ENCODER`] and [`ISO_OUTPUT`];
    /// they are not outputs, so [`super::MAX_OUTPUTS`] is unchanged. No audio flag:
    /// the manifest is returned and nothing is added. A video flag is named and not
    /// opened. One failure removes anything this call added. A second call while
    /// files are open is [`RecordError::IsoRunning`].
    pub fn start_iso_audio(
        &mut self,
        adapters: &RecordAdapters,
        start: &IsoAudioStart,
    ) -> Result<IsoAudioStarted, RecordError> {
        if self.iso_audio.is_some() {
            return Err(RecordError::IsoRunning);
        }
        let planned = iso::plan(&self.collection).map_err(RecordError::Iso)?;
        let mut manifest =
            RecordingManifest::at_start(&planned, &start.master_name, &start.t0_utc, start.t0_ns)
                .map_err(RecordError::Iso)?;
        let video_pending = planned
            .tracks
            .iter()
            .filter(|track| track.role == IsoRole::Video)
            .count();
        let audio: Vec<_> = planned
            .tracks
            .iter()
            .filter(|track| track.role == IsoRole::Audio)
            .cloned()
            .collect();
        if audio.is_empty() {
            return Ok(IsoAudioStarted {
                manifest,
                audio_tracks: 0,
                video_pending,
            });
        }
        let taps = self.taps(&audio, start.container)?;
        for file in &mut manifest.files {
            if file.role == "audio"
                && let Some(source) = file.source
                && let Some(tap) = taps.iter().find(|tap| tap.source == source)
            {
                file.file_name.clone_from(&tap.file_name);
            }
        }
        if scheme_of(&start.folder_url).is_none() {
            return Err(RecordError::InvalidUrl(start.folder_url.clone()));
        }
        let ext_config = AudioEncoderConfig::new(
            start.audio.codec,
            self.settings.audio_graph.format,
            start.audio.bitrate_bps,
        );
        let mut sessions = Vec::with_capacity(taps.len());
        for _ in &taps {
            let (session, _) = select::audio_encoder(&adapters.audio_encoders, &ext_config)?;
            let stream = session.stream().clone();
            sessions.push((session, stream));
        }
        let (muxer, _) = select::muxer(&adapters.muxers, start.container)?;
        let input = TransportInput::Container(start.container);
        let (transport, _) = select::transport(&adapters.transports, &start.folder_url, input)?;
        self.ensure_iso_partitions()?;
        let nodes = self.install_audio(&taps, sessions, &transport, &muxer, start)?;
        self.iso_audio = Some(IsoAudioRun { nodes });
        if let Some(dir) = &start.manifest_dir
            && let Err(e) = iso::write_manifest(dir, &manifest)
        {
            self.stop_iso_audio();
            return Err(RecordError::Iso(e));
        }
        Ok(IsoAudioStarted {
            manifest,
            audio_tracks: taps.len(),
            video_pending,
        })
    }

    /// True while separate audio files are open.
    pub fn iso_audio_running(&self) -> bool {
        self.iso_audio.is_some()
    }

    /// Closes separate audio files and drops their nodes. False when none were open.
    /// The partitions stay, with no worker (ADR-0013).
    pub fn stop_iso_audio(&mut self) -> bool {
        let Some(run) = self.iso_audio.take() else {
            return false;
        };
        for node in &run.nodes {
            let _ = self.graph.set_state(node.node, Lifecycle::Standby);
            let _ = self.graph.remove_node(node.node);
        }
        let _ = self.engine.apply_draining(&self.graph);
        for node in run.nodes {
            if !super::take(&mut self.engine, node) {
                self.pending_takes.push((node, None));
            }
        }
        true
    }

    /// The program mix must already be hearing each marked source.
    fn taps(
        &self,
        audio: &[crate::iso::IsoTrack],
        container: ContainerId,
    ) -> Result<Vec<Tap>, RecordError> {
        let mixer = self
            .bound_mixer
            .as_ref()
            .ok_or(RecordError::NotBound("mixer"))?;
        let ext = extension(container);
        let mut taps = Vec::with_capacity(audio.len());
        for track in audio {
            let input = mixer
                .inputs
                .iter()
                .find(|input| input.source == track.source)
                .ok_or_else(|| {
                    RecordError::Iso(IsoError::NotHeard {
                        source: track.source,
                        name: track.source_name.clone(),
                    })
                })?;
            let pin = audio_pin(&self.graph, input.producer)?;
            taps.push(Tap {
                source: track.source,
                producer: input.producer,
                pin,
                file_name: format!("{}.{}", track.file_name, ext),
            });
        }
        Ok(taps)
    }

    fn ensure_iso_partitions(&mut self) -> Result<(), RecordError> {
        // An empty partition starts no thread (ADR-0013). A worker starts when the
        // apply below schedules nodes, and stops when [`Self::stop_iso_audio`] removes them.
        if self.engine.worker_stats(ISO_AUDIO_ENCODER).is_none() {
            self.engine.add_partition::<()>(
                ISO_AUDIO_ENCODER,
                Arc::new(()),
                (),
                Pacing::doorbell(DEFAULT_DOORBELL_TIMER),
            )?;
        }
        if self.engine.worker_stats(ISO_OUTPUT).is_none() {
            self.engine
                .add_partition::<()>(ISO_OUTPUT, Arc::new(()), (), Pacing::output())?;
        }
        Ok(())
    }

    fn install_audio(
        &mut self,
        taps: &[Tap],
        sessions: Vec<(Box<dyn AudioEncoder>, rbs_core::media::StreamDescriptor)>,
        transport: &Arc<dyn rbs_transport::TransportFactory>,
        muxer: &Arc<dyn rbs_mux::MuxerFactory>,
        start: &IsoAudioStart,
    ) -> Result<Vec<Added>, RecordError> {
        let mut graph = self.graph.clone();
        let mut adopted = Vec::with_capacity(taps.len() * 2);
        for (tap, (session, stream)) in taps.iter().zip(sessions) {
            let encoder = EncoderNode::new(session, AUDIO_ENCODER_PACKETS);
            let output = OutputNode::new(
                vec![stream],
                Arc::clone(transport),
                TransportConfig::new(
                    track_url(&start.folder_url, &tap.file_name),
                    TransportInput::Container(start.container),
                ),
            )
            .with_muxer(Arc::clone(muxer), start.container, None);
            let enc_id = match add_active(
                &mut graph,
                Node::<()>::descriptor(&encoder),
                ISO_AUDIO_ENCODER,
            ) {
                Ok(id) => id,
                Err(e) => {
                    self.abandon(adopted);
                    return Err(e.into());
                }
            };
            let out_id = match add_active(&mut graph, Node::<()>::descriptor(&output), ISO_OUTPUT) {
                Ok(id) => id,
                Err(e) => {
                    self.abandon(adopted);
                    return Err(e.into());
                }
            };
            if let Err(e) = graph
                .connect(tap.producer, &tap.pin, enc_id, "in")
                .and_then(|_| graph.connect(enc_id, "packets", out_id, "s0"))
            {
                self.abandon(adopted);
                return Err(e.into());
            }
            let enc = Added {
                node: enc_id,
                partition: ISO_AUDIO_ENCODER,
                gpu: false,
            };
            let out = Added {
                node: out_id,
                partition: ISO_OUTPUT,
                gpu: false,
            };
            if let Err(e) = self
                .engine
                .adopt::<()>(ISO_AUDIO_ENCODER, enc_id, Box::new(encoder))
            {
                self.abandon(adopted);
                return Err(e.into());
            }
            adopted.push(enc);
            if let Err(e) = self
                .engine
                .adopt::<()>(ISO_OUTPUT, out_id, Box::new(output))
            {
                self.abandon(adopted);
                return Err(e.into());
            }
            adopted.push(out);
        }
        match self.engine.apply(&graph) {
            Err(e) => {
                self.abandon(adopted);
                Err(e.into())
            }
            Ok(applied) => {
                self.graph = graph;
                let mut failed = super::engine_failures(&applied);
                for report in applied.reports.values() {
                    for (node, why) in &report.failed {
                        if adopted.iter().any(|added| added.node == *node) {
                            failed.push(StartFailure {
                                node: "separate audio".to_owned(),
                                reason: why.to_string(),
                            });
                        }
                    }
                }
                if failed.is_empty() {
                    Ok(adopted)
                } else {
                    self.remove_added(&adopted);
                    Err(RecordError::Start(failed))
                }
            }
        }
    }

    /// Instances adopted before the graph was applied.
    fn abandon(&mut self, adopted: Vec<Added>) {
        for node in adopted {
            if !super::take(&mut self.engine, node) {
                self.pending_takes.push((node, None));
            }
        }
    }

    /// The apply installed these nodes and one of them did not join.
    fn remove_added(&mut self, nodes: &[Added]) {
        for node in nodes {
            let _ = self.graph.set_state(node.node, Lifecycle::Standby);
            let _ = self.graph.remove_node(node.node);
        }
        let _ = self.engine.apply(&self.graph);
        for node in nodes {
            if !super::take(&mut self.engine, *node) {
                self.pending_takes.push((*node, None));
            }
        }
    }
}

fn audio_pin(graph: &GraphDescription, node: NodeId) -> Result<String, RecordError> {
    let entry = graph
        .node(node)
        .ok_or(RecordError::NotBound("audio source"))?;
    entry
        .descriptor
        .outputs
        .iter()
        .find(|pin| pin.kind == LinkKind::Audio)
        .map(|pin| pin.name.to_string())
        .ok_or(RecordError::NotBound("audio source"))
}

fn extension(container: ContainerId) -> &'static str {
    match container {
        ContainerId::Matroska => "mkv",
        ContainerId::Mp4Fragmented => "mp4",
        ContainerId::MpegTs => "ts",
        ContainerId::Flv => "flv",
        ContainerId::Wav => "wav",
        ContainerId::Flac => "flac",
        _ => "bin",
    }
}

/// `{folder}/{name}` with a space, `%`, `#` or `?` in the name escaped.
fn track_url(folder: &str, file_name: &str) -> String {
    let folder = folder.trim_end_matches('/');
    let mut name = String::new();
    for c in file_name.chars() {
        match c {
            ' ' => name.push_str("%20"),
            '%' => name.push_str("%25"),
            '#' => name.push_str("%23"),
            '?' => name.push_str("%3F"),
            other => name.push(other),
        }
    }
    format!("{folder}/{name}")
}
