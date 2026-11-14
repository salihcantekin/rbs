//! Separate video files (ADR-0086 step 3.2, ADR-0119). One file per source marked for
//! video, with that source's own picture: without its filters by default (ADR-0119
//! decision 1), or after them when [`IsoVideoStart::include_filters`] is set (ADR-0086
//! decision 3). The file is the source's size when the recording starts, rounded up
//! to even, at the canvas frame rate. The export runs on the render partition after
//! the master's and gives way when the previous render tick ran long or a master
//! encoder dropped a frame (decision 9). The encoders take turns on two partitions
//! of [`ISO_VIDEO_ENCODERS`] while they share the master's engine (see
//! `SHARED_ENGINE_PARTITIONS`); the files share [`ISO_OUTPUT`] with the audio files. The files are not [`super::MAX_OUTPUTS`] outputs.
//!
//!
//! A still source marked for video is an image track instead (ADR-0117): an
//! [`ImageTrackNode`] on the render partition reads its picture back once per change,
//! and a [`PngWriterNode`] on [`ISO_OUTPUT`] writes each as a PNG into
//! [`IsoVideoStart::image_dir`]. No encoder, no encoder partition, not counted in
//! [`crate::MAX_ISO_VIDEO`].
//!
//! Not yet (later steps): a source whose size changes during the take (the export's
//! input no longer fits and its frames are lost, counted), `size: master` and
//! `max_height`, a per-source filter choice, off-air sources, offsets from `t0`.

use std::path::PathBuf;
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
use rbs_nodes::{ImageTrackNode, ImageTrackStats};
use rbs_scene::model::SourceId;
use rbs_transport::{TransportConfig, TransportInput};

use super::Added;
use super::iso_audio::{extension, track_url};
use crate::iso::{self, IsoError, IsoRole};
use crate::png_writer::{ImageFiles, PngWriterNode};
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
    /// Records each source after its filters (ADR-0086 decision 3) instead of its
    /// own picture (ADR-0119 decision 1, the default).
    pub include_filters: bool,
    /// The take folder image tracks write their PNG files into (ADR-0117), written
    /// directly as the manifest is. Needed only when a still source is marked:
    /// without it such a start is [`IsoError::ImageFolder`].
    pub image_dir: Option<PathBuf>,
    /// `t0` on the master clock: each image's `at_ns` is its tick's time from here.
    pub t0: Timestamp,
    /// Most video files to open ([`crate::MAX_ISO_VIDEO`] by default). Marked sources
    /// past it, in source id order, are skipped ([`IsoSkip::Limit`]) and listed in
    /// [`IsoVideoStarted::skipped`]. Image tracks are not counted.
    pub max_video: usize,
    /// Skips a marked source that has no picture yet ([`IsoSkip::NoPicture`]) instead
    /// of refusing the start with [`IsoError::NoPicture`].
    pub skip_no_picture: bool,
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
            include_filters: false,
            image_dir: None,
            t0: Timestamp::ZERO,
            max_video: crate::iso::MAX_ISO_VIDEO,
            skip_no_picture: false,
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

/// One image track [`super::Studio::start_iso_video`] opened (ADR-0117).
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct IsoImageTrack {
    /// The source.
    pub source: SourceId,
    /// The name of its first image, `<stem>.png`; later ones are `<stem>-2.png`, ...
    pub file_name: String,
    /// The source's picture size at the start.
    pub size: (u32, u32),
    /// The track's counters: pictures taken, coalesced, over the cap, failures.
    pub stats: Arc<ImageTrackStats>,
    /// The images written so far, for the manifest at stop.
    pub files: Arc<ImageFiles>,
}

/// What [`super::Studio::start_iso_video`] opened. Empty when nothing is marked for
/// video: no node and no worker were added.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct IsoVideoStarted {
    /// The video files, in source id order.
    pub tracks: Vec<IsoVideoTrack>,
    /// The image tracks of still sources, in source id order.
    pub images: Vec<IsoImageTrack>,
    /// Marked sources left out, in source id order, with the reason.
    pub skipped: Vec<IsoSkipped>,
}

/// A marked picture source [`super::Studio::start_iso_video`] left out.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoSkipped {
    /// The source.
    pub source: SourceId,
    /// What it would have been: [`IsoRole::Video`] or [`IsoRole::Image`].
    pub role: IsoRole,
    /// Why it was left out.
    pub reason: IsoSkip,
}

/// Why a marked picture source was left out of a start.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsoSkip {
    /// More video files than [`IsoVideoStart::max_video`].
    Limit,
    /// No picture yet, with [`IsoVideoStart::skip_no_picture`].
    NoPicture,
}

pub(super) struct IsoVideoRun {
    nodes: Vec<Added>,
    /// The first node of each track after the source's chain, to follow the source
    /// when the binder replaces its node (a new colour or file builds a new still
    /// node; ADR-0117 decision 4).
    relinks: Vec<Relink>,
}

/// Where a track reads its source's picture from.
struct Relink {
    source: SourceId,
    /// Reads the chain tail (filters included) rather than the source node.
    filters: bool,
    /// Takes any producer (an image track). A video track's first node is a
    /// normalisation pass for a source node and the export for a filter tail, so it
    /// follows a producer of the same kind only.
    any_kind: bool,
    from_filter: bool,
    /// The track's first node; its input pin 0 takes the picture.
    to: NodeId,
    to_pin: &'static str,
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

/// The encoder partitions shared by files encoded on the master's engine. The files
/// take turns on the first two of [`ISO_VIDEO_ENCODERS`]: a partition per file sends
/// the frames to that engine side by side, and the master then missed about four
/// frames in 30 s with three 1080p60 files on an M4 Pro, where two shared partitions
/// missed none or one (ADR-0119 step (a) measurement, its `## Review` record). A
/// partition per file comes with encoders that do not share the master's engine
/// (ADR-0119 steps (c) and (d)).
pub(super) const SHARED_ENGINE_PARTITIONS: usize = 2;

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
        let stills: Vec<_> = planned
            .tracks
            .iter()
            .filter(|track| track.role == IsoRole::Image)
            .collect();
        if marked.is_empty() && stills.is_empty() {
            return Ok(IsoVideoStarted {
                tracks: Vec::new(),
                images: Vec::new(),
                skipped: Vec::new(),
            });
        }
        let mut skipped = Vec::new();
        if !marked.is_empty() && scheme_of(&start.folder_url).is_none() {
            return Err(RecordError::InvalidUrl(start.folder_url.clone()));
        }
        let image_dir = match (&start.image_dir, stills.is_empty()) {
            (_, true) => None,
            (Some(dir), false) => Some(dir.clone()),
            (None, false) => return Err(RecordError::Iso(IsoError::ImageFolder)),
        };
        let mut image_taps = Vec::with_capacity(stills.len());
        for track in stills {
            let no_picture = || {
                RecordError::Iso(IsoError::NoPicture {
                    source: track.source,
                    name: track.source_name.clone(),
                })
            };
            // ADR-0119 decision 1: the source's own picture (straight sRGB, copied as
            // it is) unless filters are asked for (converted once per change).
            let producer = if start.include_filters {
                self.binder.chain_tail(track.source).map(|(node, _)| node)
            } else {
                self.binder.source_node(track.source)
            };
            let picture = self
                .picture_size(track.source)
                .filter(|(w, h)| *w > 0 && *h > 0);
            let (Some(producer), Some(picture)) = (producer, picture) else {
                if start.skip_no_picture {
                    skipped.push(IsoSkipped {
                        source: track.source,
                        role: IsoRole::Image,
                        reason: IsoSkip::NoPicture,
                    });
                    continue;
                }
                return Err(no_picture());
            };
            image_taps.push(Tap {
                source: track.source,
                producer,
                pin: video_pin(&self.graph, producer)?,
                normalise: false,
                picture,
                file_name: track.file_name.clone(),
            });
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
            // ADR-0119 decision 1: the source's own picture unless filters are asked for.
            let producer = if start.include_filters {
                self.binder.chain_tail(track.source)
            } else {
                self.binder
                    .source_node(track.source)
                    .map(|node| (node, false))
            };
            let picture = self
                .picture_size(track.source)
                .filter(|(w, h)| *w > 0 && *h > 0);
            let (Some((producer, is_filter)), Some(picture)) = (producer, picture) else {
                if start.skip_no_picture {
                    skipped.push(IsoSkipped {
                        source: track.source,
                        role: IsoRole::Video,
                        reason: IsoSkip::NoPicture,
                    });
                    continue;
                }
                return Err(no_picture());
            };
            // In source id order: the first `max_video` that can open are kept
            // (ADR-0087 decision 5).
            if taps.len() >= start.max_video {
                skipped.push(IsoSkipped {
                    source: track.source,
                    role: IsoRole::Video,
                    reason: IsoSkip::Limit,
                });
                continue;
            }
            taps.push(Tap {
                source: track.source,
                producer,
                pin: video_pin(&self.graph, producer)?,
                normalise: !is_filter,
                picture,
                file_name: format!("{}.{}", track.file_name, ext),
            });
        }
        if taps.is_empty() && image_taps.is_empty() {
            skipped.sort_by_key(|s| s.source);
            return Ok(IsoVideoStarted {
                tracks: Vec::new(),
                images: Vec::new(),
                skipped,
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
        let images: Vec<_> = image_taps
            .iter()
            .map(|tap| {
                let node = ImageTrackNode::new(tap.picture.0, tap.picture.1)
                    .give_way_after(budget)
                    .yield_to(masters.clone());
                let writer = PngWriterNode::new(
                    image_dir.clone().unwrap_or_default(),
                    &tap.file_name,
                    start.t0,
                );
                (node, writer)
            })
            .collect();
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
        let sinks = if taps.is_empty() {
            None
        } else {
            let (muxer, _) = select::muxer(&adapters.muxers, start.container)?;
            let input = TransportInput::Container(start.container);
            let (transport, _) = select::transport(&adapters.transports, &start.folder_url, input)?;
            Some((transport, muxer))
        };
        self.ensure_iso_partitions()?;
        self.ensure_iso_video_partitions(taps.len())?;
        let (nodes, tracks, images, relinks) =
            self.install_video(&taps, chains, &image_taps, images, sinks.as_ref(), start)?;
        self.iso_video = Some(IsoVideoRun { nodes, relinks });
        skipped.sort_by_key(|s| s.source);
        Ok(IsoVideoStarted {
            tracks,
            images,
            skipped,
        })
    }

    /// Links each running track to its source's current producer, after the binder
    /// replaced a source or filter node. Called by an update before its apply.
    pub(super) fn relink_iso_video(&mut self) {
        let Some(run) = &self.iso_video else {
            return;
        };
        for r in &run.relinks {
            let producer = if r.filters {
                self.binder.chain_tail(r.source)
            } else {
                self.binder.source_node(r.source).map(|n| (n, false))
            };
            let Some((node, is_filter)) = producer else {
                continue;
            };
            if !r.any_kind && is_filter != r.from_filter {
                continue;
            }
            let current = self
                .graph
                .links()
                .iter()
                .find(|l| l.to.node == r.to && l.to.pin == 0)
                .copied();
            if current.is_some_and(|l| l.from.node == node) {
                continue;
            }
            if let Some(link) = current {
                self.graph.disconnect(link);
            }
            if let Ok(pin) = video_pin(&self.graph, node) {
                let _ = self.graph.connect(node, &pin, r.to, r.to_pin);
            }
        }
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

    /// Adds the encoder partitions `count` files use that are not there yet.
    fn ensure_iso_video_partitions(&mut self, count: usize) -> Result<(), RecordError> {
        // Paced like the master's encoders. An empty partition starts no thread
        // (ADR-0013); a worker runs while the apply below schedules nodes on it.
        let frames = Cadence::frames(Timestamp::ZERO, self.fps);
        for part in ISO_VIDEO_ENCODERS
            .iter()
            .take(count.min(SHARED_ENGINE_PARTITIONS))
            .copied()
        {
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

    #[allow(clippy::type_complexity)]
    fn install_video(
        &mut self,
        taps: &[Tap],
        chains: Vec<(VideoExportNode, VideoEncoderNode, (u32, u32))>,
        image_taps: &[Tap],
        images: Vec<(ImageTrackNode, PngWriterNode)>,
        sinks: Option<&(
            Arc<dyn rbs_transport::TransportFactory>,
            Arc<dyn rbs_mux::MuxerFactory>,
        )>,
        start: &IsoVideoStart,
    ) -> Result<
        (
            Vec<Added>,
            Vec<IsoVideoTrack>,
            Vec<IsoImageTrack>,
            Vec<Relink>,
        ),
        RecordError,
    > {
        let mut relinks = Vec::with_capacity(taps.len() + image_taps.len());
        let mut graph = self.graph.clone();
        let mut adopted: Vec<Added> = Vec::with_capacity(taps.len() * 4 + images.len() * 2);
        let mut tracks = Vec::with_capacity(taps.len());
        let mut image_tracks = Vec::with_capacity(images.len());
        let mut encoders = Vec::with_capacity(taps.len());
        for (tap, (node, writer)) in image_taps.iter().zip(images) {
            let stats = node.stats();
            let files = writer.files();
            match self.add_image_track(&mut graph, &mut adopted, tap, node, writer) {
                Ok(to) => relinks.push(Relink {
                    source: tap.source,
                    filters: start.include_filters,
                    any_kind: true,
                    from_filter: false,
                    to,
                    to_pin: "picture",
                }),
                Err(e) => {
                    self.abandon(adopted);
                    return Err(e);
                }
            }
            image_tracks.push(IsoImageTrack {
                source: tap.source,
                file_name: PngWriterNode::file_name(&tap.file_name, 1),
                size: tap.picture,
                stats,
                files,
            });
        }
        let Some((transport, muxer)) = sinks else {
            return self
                .apply_iso_video(graph, adopted, tracks, image_tracks, &encoders)
                .map(|(a, t, i)| (a, t, i, relinks));
        };
        for (i, (tap, (export, encoder, size))) in taps.iter().zip(chains).enumerate() {
            let encoder_part = ISO_VIDEO_ENCODERS[i % SHARED_ENGINE_PARTITIONS];
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
                Ok((node, first)) => {
                    relinks.push(Relink {
                        source: tap.source,
                        filters: start.include_filters,
                        any_kind: false,
                        from_filter: !tap.normalise,
                        to: first,
                        to_pin: if tap.normalise { "in" } else { "canvas" },
                    });
                    node
                }
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
        self.apply_iso_video(graph, adopted, tracks, image_tracks, &encoders)
            .map(|(a, t, i)| (a, t, i, relinks))
    }

    /// Applies `graph` with the nodes this start adopted, or takes them back.
    #[allow(clippy::type_complexity)]
    fn apply_iso_video(
        &mut self,
        graph: GraphDescription,
        adopted: Vec<Added>,
        mut tracks: Vec<IsoVideoTrack>,
        image_tracks: Vec<IsoImageTrack>,
        encoders: &[NodeId],
    ) -> Result<(Vec<Added>, Vec<IsoVideoTrack>, Vec<IsoImageTrack>), RecordError> {
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
                    for (track, node) in tracks.iter_mut().zip(encoders) {
                        track.queue = crossings
                            .iter()
                            .find(|c| c.link.to.node == *node)
                            .map(|c| Arc::clone(&c.stats));
                    }
                    Ok((adopted, tracks, image_tracks))
                } else {
                    self.remove_added(&adopted);
                    Err(RecordError::Start(failed))
                }
            }
        }
    }

    /// Adds an image track's two nodes to `graph` and adopts them: the track on
    /// `RENDER` after `tap`'s producer, the PNG writer on `ISO_OUTPUT`.
    fn add_image_track(
        &mut self,
        graph: &mut GraphDescription,
        adopted: &mut Vec<Added>,
        tap: &Tap,
        node: ImageTrackNode,
        writer: PngWriterNode,
    ) -> Result<NodeId, RecordError> {
        let track_id = add_active(graph, Node::<GpuRuntime>::descriptor(&node), RENDER)?;
        let writer_id = add_active(graph, Node::<()>::descriptor(&writer), ISO_OUTPUT)?;
        graph.connect(tap.producer, &tap.pin, track_id, "picture")?;
        graph.connect(track_id, "video", writer_id, "video")?;
        self.engine
            .adopt::<GpuRuntime>(RENDER, track_id, Box::new(node))?;
        adopted.push(Added {
            node: track_id,
            partition: RENDER,
            gpu: true,
        });
        self.engine
            .adopt::<()>(ISO_OUTPUT, writer_id, Box::new(writer))?;
        adopted.push(Added {
            node: writer_id,
            partition: ISO_OUTPUT,
            gpu: false,
        });
        Ok(track_id)
    }

    /// Adds one track's nodes to `graph` and adopts them, pushing each adopted node
    /// to `adopted` so a failure can take them back. Returns the encoder's node and
    /// the track's first node (the one that reads the source's picture).
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
    ) -> Result<(NodeId, NodeId), RecordError> {
        // The normalisation pass draws the source's picture into the canvas format
        // (decision 9, case b); a filter tail already is (case a).
        let mut from = (tap.producer, tap.pin.clone());
        let mut first = None;
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
            first = Some(id);
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
        Ok((encoder_id, first.unwrap_or(export_id)))
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
