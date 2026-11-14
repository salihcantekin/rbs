//! Separate video files (ADR-0086 step 3.2). One file per source marked for video,
//! with that source's own picture after its filters (decision 3), at the source's size
//! when the recording starts, rounded up to even, and at the canvas frame rate. The
//! export runs on the render partition after the master's and gives way when the
//! previous render tick ran long (decision 9). The encoders run on a pool of two
//! partitions, [`ISO_VIDEO_ENCODERS`]; the files share [`ISO_OUTPUT`] with the audio
//! files. The files are not [`super::MAX_OUTPUTS`] outputs.
//!
//! Not yet (later steps): a source whose size changes during the take (the export's
//! input no longer fits and its frames are lost, counted), `size: master` and
//! `max_height`, *Exclude filters*, off-air sources, offsets from `t0`.

use std::sync::Arc;
use std::time::Duration;

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::ContainerId;
use rbs_core::time::Timestamp;
use rbs_core::timing::Cadence;
use rbs_engine::Pacing;
use rbs_filters::{FilterNode, FilterType, Normalise, Values};
use rbs_gpu::node::GpuRuntime;
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::LinkKind;
use rbs_graph::runtime::Node;
use rbs_graph::runtime::queue::QueueStats;
use rbs_nodes::OutputNode;
use rbs_nodes::VideoEncoderNode;
use rbs_nodes::encode::EncoderNodeStats;
use rbs_nodes::export::{ExportNodeStats, VideoExportNode};
use rbs_scene::model::SourceId;
use rbs_transport::{TransportConfig, TransportInput};

use super::Added;
use super::iso_audio::{extension, track_url};
use crate::iso::{self, IsoError, IsoRole};
use crate::recording::{
    ISO_OUTPUT, ISO_VIDEO_ENCODERS, RENDER, RecordAdapters, RecordError, StartFailure,
    VideoSettings, add_active,
};
use crate::select::{self, scheme_of};

/// The share of the frame period after which an ISO export skips a canvas (decision
/// 9). A setting in a later step; three quarters leaves the master its whole period
/// when the tick before used most of it.
pub const DEFAULT_GIVE_WAY_SHARE: f64 = 0.75;

/// What [`super::Studio::start_iso_video`] opens.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct IsoVideoStart {
    /// Folder URL the files are written under. No trailing name: each file is
    /// `{folder}/{name}.{ext}`.
    pub folder_url: String,
    /// Container each file is written in.
    pub container: ContainerId,
    /// Encoder settings of every track (decision 4: the caller derives them from the
    /// master's, or from the profile's own).
    pub video: VideoSettings,
    /// Share of the frame period after which a track skips a canvas
    /// ([`DEFAULT_GIVE_WAY_SHARE`]).
    pub give_way_share: f64,
}

impl IsoVideoStart {
    /// `folder_url`, `container` and `video`, giving way at the default share.
    pub fn new(
        folder_url: impl Into<String>,
        container: ContainerId,
        video: VideoSettings,
    ) -> Self {
        Self {
            folder_url: folder_url.into(),
            container,
            video,
            give_way_share: DEFAULT_GIVE_WAY_SHARE,
        }
    }
}

/// One video file [`super::Studio::start_iso_video`] opened.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct IsoVideoTrack {
    /// The source.
    pub source: SourceId,
    /// File name inside the folder, with the container's extension.
    pub file_name: String,
    /// The file's picture size: the source's at the start, rounded up to even.
    pub size: (u32, u32),
    /// The track's export counters: frames, canvases skipped to give way, losses.
    pub stats: Arc<ExportNodeStats>,
    /// The track's encoder counters: `submitted` is the frames in the file.
    pub encoder: Arc<EncoderNodeStats>,
    /// The queue into the track's encoder: `video_drops` counts frames the export
    /// passed on that the encoder never took (its partition was busy with the other
    /// tracks on it). `None` only if the engine did not report the crossing.
    pub queue: Option<Arc<QueueStats>>,
}

/// What [`super::Studio::start_iso_video`] opened. Empty when nothing is marked for
/// video: no node and no worker were added.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct IsoVideoStarted {
    /// The files, in source id order.
    pub tracks: Vec<IsoVideoTrack>,
}

pub(super) struct IsoVideoRun {
    nodes: Vec<Added>,
}

/// A marked source, the node its picture leaves its chain from, and its file.
struct Tap {
    source: SourceId,
    producer: NodeId,
    pin: String,
    /// The producer is the source node, whose picture may be 8-bit RGB: a
    /// normalisation pass goes first.
    normalise: bool,
    picture: (u32, u32),
    file_name: String,
}

impl super::Studio {
    /// Opens one video file per source marked for separate video. The input is that
    /// source's chain tail (its last enabled filter, or the source through a
    /// normalisation pass). The files are not outputs, so [`super::MAX_OUTPUTS`] is
    /// unchanged. Nothing marked: nothing is added. A marked source with no picture
    /// yet is [`IsoError::NoPicture`]. One failure removes anything this call added.
    /// A second call while files are open is [`RecordError::IsoRunning`].
    pub fn start_iso_video(
        &mut self,
        adapters: &RecordAdapters,
        start: &IsoVideoStart,
    ) -> Result<IsoVideoStarted, RecordError> {
        if self.iso_video.is_some() {
            return Err(RecordError::IsoRunning);
        }
        let planned = iso::plan(&self.collection).map_err(RecordError::Iso)?;
        let marked: Vec<_> = planned
            .tracks
            .iter()
            .filter(|track| track.role == IsoRole::Video)
            .collect();
        if marked.is_empty() {
            return Ok(IsoVideoStarted { tracks: Vec::new() });
        }
        if scheme_of(&start.folder_url).is_none() {
            return Err(RecordError::InvalidUrl(start.folder_url.clone()));
        }
        let ext = extension(start.container);
        let mut taps = Vec::with_capacity(marked.len());
        for track in marked {
            let no_picture = || {
                RecordError::Iso(IsoError::NoPicture {
                    source: track.source,
                    name: track.source_name.clone(),
                })
            };
            let (producer, is_filter) = self
                .binder
                .chain_tail(track.source)
                .ok_or_else(no_picture)?;
            let picture = self
                .picture_size(track.source)
                .filter(|(w, h)| *w > 0 && *h > 0)
                .ok_or_else(no_picture)?;
            taps.push(Tap {
                source: track.source,
                producer,
                pin: video_pin(&self.graph, producer)?,
                normalise: !is_filter,
                picture,
                file_name: format!("{}.{}", track.file_name, ext),
            });
        }
        let period = self.fps.frame_duration();
        let budget = Duration::from_secs_f64(period.as_secs_f64() * start.give_way_share.max(0.0));
        // The queues into the running master encoders: a drop there makes every
        // track skip canvases for a second (decision 9), so shared encoder hardware
        // loses ISO frames first.
        let queues: Vec<_> = self
            .chains
            .video
            .iter()
            .flatten()
            .filter_map(|chain| chain.queue.clone())
            .collect();
        let masters: rbs_nodes::export::MasterDrops =
            Arc::new(move || queues.iter().map(|q| q.video_drops()).sum());
        let hold = (Duration::from_secs(1).as_nanos() / period.as_nanos().max(1)) as u64;
        let mut chains = Vec::with_capacity(taps.len());
        for tap in &taps {
            let size = even(tap.picture);
            let export = VideoExportNode::with_tone(
                start.video.format,
                tap.picture.0,
                tap.picture.1,
                size.0,
                size.1,
                start.video.color,
                start.video.open_shadows,
            )
            .give_way_after(budget)
            .yield_to(masters.clone(), hold)
            .every_tick();
            let config = super::video_encoder_config(&start.video, export.frame_format(), self.fps);
            let (encoder, _) = select::video_encoder(&adapters.video_encoders, &config)?;
            chains.push((export, encoder, size));
        }
        let (muxer, _) = select::muxer(&adapters.muxers, start.container)?;
        let input = TransportInput::Container(start.container);
        let (transport, _) = select::transport(&adapters.transports, &start.folder_url, input)?;
        self.ensure_iso_partitions()?;
        self.ensure_iso_video_partitions()?;
        let (nodes, tracks) = self.install_video(&taps, chains, &transport, &muxer, start)?;
        self.iso_video = Some(IsoVideoRun { nodes });
        Ok(IsoVideoStarted { tracks })
    }

    /// True while separate video files are open.
    pub fn iso_video_running(&self) -> bool {
        self.iso_video.is_some()
    }

    /// Closes separate video files and drops their nodes. False when none were open.
    /// The partitions stay, with no worker (ADR-0013).
    pub fn stop_iso_video(&mut self) -> bool {
        let Some(run) = self.iso_video.take() else {
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

    fn ensure_iso_video_partitions(&mut self) -> Result<(), RecordError> {
        // Paced like the master's encoders. An empty partition starts no thread
        // (ADR-0013); a worker runs while the apply below schedules nodes on it.
        let frames = Cadence::frames(Timestamp::ZERO, self.fps);
        for part in ISO_VIDEO_ENCODERS {
            if self.engine.worker_stats(part).is_none() {
                self.engine.add_partition::<()>(
                    part,
                    Arc::new(()),
                    (),
                    Pacing::cadence_after(frames, self.fps.frame_duration() / 4),
                )?;
            }
        }
        Ok(())
    }

    fn install_video(
        &mut self,
        taps: &[Tap],
        chains: Vec<(VideoExportNode, VideoEncoderNode, (u32, u32))>,
        transport: &Arc<dyn rbs_transport::TransportFactory>,
        muxer: &Arc<dyn rbs_mux::MuxerFactory>,
        start: &IsoVideoStart,
    ) -> Result<(Vec<Added>, Vec<IsoVideoTrack>), RecordError> {
        let mut graph = self.graph.clone();
        let mut adopted: Vec<Added> = Vec::with_capacity(taps.len() * 4);
        let mut tracks = Vec::with_capacity(taps.len());
        let mut encoders = Vec::with_capacity(taps.len());
        for (i, (tap, (export, encoder, size))) in taps.iter().zip(chains).enumerate() {
            let encoder_part = ISO_VIDEO_ENCODERS[i % ISO_VIDEO_ENCODERS.len()];
            let output = OutputNode::new(
                vec![encoder.stream().clone()],
                Arc::clone(transport),
                TransportConfig::new(
                    track_url(&start.folder_url, &tap.file_name),
                    TransportInput::Container(start.container),
                ),
            )
            .with_muxer(Arc::clone(muxer), start.container, None);
            let stats = export.stats();
            let encoder_stats = encoder.stats();
            let encoder_node = self.add_track(
                &mut graph,
                &mut adopted,
                tap,
                export,
                encoder,
                encoder_part,
                output,
            );
            let encoder_node = match encoder_node {
                Ok(node) => node,
                Err(e) => {
                    self.abandon(adopted);
                    return Err(e);
                }
            };
            encoders.push(encoder_node);
            tracks.push(IsoVideoTrack {
                source: tap.source,
                file_name: tap.file_name.clone(),
                size,
                stats,
                encoder: encoder_stats,
                queue: None,
            });
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
                                node: "separate video".to_owned(),
                                reason: why.to_string(),
                            });
                        }
                    }
                }
                if failed.is_empty() {
                    let crossings = self.engine.crossings();
                    for (track, node) in tracks.iter_mut().zip(&encoders) {
                        track.queue = crossings
                            .iter()
                            .find(|c| c.link.to.node == *node)
                            .map(|c| Arc::clone(&c.stats));
                    }
                    Ok((adopted, tracks))
                } else {
                    self.remove_added(&adopted);
                    Err(RecordError::Start(failed))
                }
            }
        }
    }

    /// Adds one track's nodes to `graph` and adopts them, pushing each adopted node
    /// to `adopted` so a failure can take them back. Returns the encoder's node.
    #[allow(clippy::too_many_arguments)]
    fn add_track(
        &mut self,
        graph: &mut GraphDescription,
        adopted: &mut Vec<Added>,
        tap: &Tap,
        export: VideoExportNode,
        encoder: VideoEncoderNode,
        encoder_part: rbs_graph::description::PartitionId,
        output: OutputNode,
    ) -> Result<NodeId, RecordError> {
        // The normalisation pass draws the source's picture into the canvas format
        // (decision 9, case b); a filter tail already is (case a).
        let mut from = (tap.producer, tap.pin.clone());
        if tap.normalise {
            let ty: Arc<dyn FilterType> = Arc::new(Normalise);
            let values = Values::defaults(ty.as_ref());
            let (node, _, _) = FilterNode::new(ty, values, Some([tap.picture.0, tap.picture.1]));
            let id = add_active(graph, Node::<GpuRuntime>::descriptor(&node), RENDER)?;
            graph.connect(from.0, &from.1, id, "in")?;
            self.engine
                .adopt::<GpuRuntime>(RENDER, id, Box::new(node))?;
            adopted.push(Added {
                node: id,
                partition: RENDER,
                gpu: true,
            });
            from = (id, "out".to_owned());
        }
        let export_id = add_active(graph, Node::<GpuRuntime>::descriptor(&export), RENDER)?;
        let encoder_id = add_active(graph, Node::<()>::descriptor(&encoder), encoder_part)?;
        let output_id = add_active(graph, Node::<()>::descriptor(&output), ISO_OUTPUT)?;
        graph.connect(from.0, &from.1, export_id, "canvas")?;
        graph.connect(export_id, "video", encoder_id, "video")?;
        graph.connect(encoder_id, "packets", output_id, "s0")?;
        self.engine
            .adopt::<GpuRuntime>(RENDER, export_id, Box::new(export))?;
        adopted.push(Added {
            node: export_id,
            partition: RENDER,
            gpu: true,
        });
        self.engine
            .adopt::<()>(encoder_part, encoder_id, Box::new(encoder))?;
        adopted.push(Added {
            node: encoder_id,
            partition: encoder_part,
            gpu: false,
        });
        self.engine
            .adopt::<()>(ISO_OUTPUT, output_id, Box::new(output))?;
        adopted.push(Added {
            node: output_id,
            partition: ISO_OUTPUT,
            gpu: false,
        });
        Ok(encoder_id)
    }
}

/// The picture rounded up to even: encoders take even sizes only.
fn even((w, h): (u32, u32)) -> (u32, u32) {
    (w + (w & 1), h + (h & 1))
}

fn video_pin(graph: &GraphDescription, node: NodeId) -> Result<String, RecordError> {
    let entry = graph
        .node(node)
        .ok_or(RecordError::NotBound("video source"))?;
    entry
        .descriptor
        .outputs
        .iter()
        .find(|pin| pin.kind == LinkKind::Video)
        .map(|pin| pin.name.to_string())
        .ok_or(RecordError::NotBound("video source"))
}

#[cfg(test)]
mod tests {
    use super::even;

    #[test]
    fn odd_pictures_round_up_to_even() {
        assert_eq!(even((1920, 1080)), (1920, 1080));
        assert_eq!(even((1281, 721)), (1282, 722));
    }
}
