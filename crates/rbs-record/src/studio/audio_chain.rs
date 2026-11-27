//! The audio half of an output: one encoder, shared or new, or one encoder per track
//! of a multi-track recording (ADR-0170 decisions 4 to 8). A chain is one
//! `MAX_CHAINS` slot and one partition whatever its track count; a chain with more
//! than one track is never shared.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_audio::{AudioGraphConfig, EncoderNode};
use rbs_core::media::StreamDescriptor;
use rbs_core::pause::{HoldSwitch, PauseWindow};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::runtime::Node;
use rbs_graph::runtime::queue::QueueStats;
use rbs_mux::caps as mux_caps;
use rbs_scene::model::MAIN_BUS_NAME;

use super::{
    AUDIO_ENCODER_PACKETS, Added, Chains, OutputSettings, buses, free_chain, never_paused,
};
use crate::recording::{
    AudioSettings, Partitions, RecordAdapters, RecordError, TrackProgress, TrackReport, add_active,
};
use crate::select::{self, Chosen};

/// A new chain's encoder nodes with where each was added, track 1 first.
pub(super) type ChainNodes = Vec<(Added, Box<dyn Node<()>>)>;

/// The audio half of a prepared output.
// One per output being started, on the control thread; boxing `New` buys nothing.
#[allow(clippy::large_enum_variant)]
pub(super) enum AudioPlan {
    None,
    New {
        node: Box<dyn Node<()>>,
        /// The node's packet losses, end-of-stream flushes and tail losses.
        counters: AudioCounters,
        /// The chain's own pause window, for a pausable recording's new chain; every
        /// track's encoder follows the same one, so they cut on the same block.
        chain_pause: Option<Arc<PauseWindow>>,
        /// The chain's hold switch, as for the video chain.
        chain_hold: Option<Arc<HoldSwitch>>,
        chosen: Chosen,
        settings: AudioSettings,
        stream: StreamDescriptor,
        /// The mixer bus slot it reads, `None` for `main`, and that bus's name.
        bus: Option<usize>,
        name: String,
        /// Tracks 2 and later of a multi-track recording, in order.
        more: Vec<TrackPlan>,
    },
    Shared(usize),
}

/// A later track's encoder, nothing in the graph yet.
pub(super) struct TrackPlan {
    node: Box<dyn Node<()>>,
    counters: AudioCounters,
    stream: StreamDescriptor,
    bus: Option<usize>,
    name: String,
    chosen: Chosen,
}

/// A running audio encoder, used by one or more outputs.
pub(super) struct AudioChain {
    pub(super) settings: AudioSettings,
    pub(super) chosen: Chosen,
    pub(super) stream: StreamDescriptor,
    pub(super) encoder: Added,
    pub(super) dropped: Arc<AtomicU64>,
    /// End-of-stream flushes and tail losses (ADR-0045).
    pub(super) flushes: Arc<AtomicU64>,
    pub(super) tail_lost: Arc<AtomicU64>,
    pub(super) queue: Option<Arc<QueueStats>>,
    /// The pause window its encoder nodes follow, for a pausable recording's new
    /// chain (ADR-0124); written only while that recording is its only user.
    pub(super) pause: Option<Arc<PauseWindow>>,
    /// Its hold switch, as for the video chain.
    pub(super) hold: Option<Arc<HoldSwitch>>,
    /// The mixer bus slot track 1 reads (ADR-0170), `None` for `main`.
    pub(super) bus: Option<usize>,
    /// Track 1's bus name, kept for the reports of a multi-track chain.
    pub(super) name: String,
    /// Tracks 2 and later; empty for every chain but a multi-track recording's.
    pub(super) more: Vec<Track>,
}

/// A running later track of a multi-track chain.
pub(super) struct Track {
    pub(super) encoder: Added,
    bus: Option<usize>,
    name: String,
    chosen: Chosen,
    dropped: Arc<AtomicU64>,
    flushes: Arc<AtomicU64>,
    tail_lost: Arc<AtomicU64>,
    pub(super) queue: Option<Arc<QueueStats>>,
}

/// An audio encoder node's counters, kept when the node is boxed.
pub(super) struct AudioCounters {
    dropped: Arc<AtomicU64>,
    flushes: Arc<AtomicU64>,
    tail_lost: Arc<AtomicU64>,
}

impl AudioCounters {
    fn of<G: rbs_audio::AudioGate>(node: &EncoderNode<G>) -> Self {
        Self {
            dropped: node.dropped(),
            flushes: node.flushes(),
            tail_lost: node.tail_lost(),
        }
    }
}

/// The new audio nodes of an output added to the graph: track 1, then the others.
pub(super) struct NewAudio {
    first: Added,
    more: Vec<Added>,
}

impl NewAudio {
    /// Every encoder node, track 1 first.
    pub(super) fn all(&self) -> impl Iterator<Item = Added> + '_ {
        std::iter::once(self.first).chain(self.more.iter().copied())
    }
}

/// An audio encoder node that follows `pause` and `hold` (ADR-0124), boxed, and its
/// counters.
fn gated(
    node: EncoderNode,
    pause: Option<&Arc<PauseWindow>>,
    hold: Option<&Arc<HoldSwitch>>,
    frames: usize,
) -> (Box<dyn Node<()>>, AudioCounters) {
    match (pause, hold) {
        (Some(p), hold) => {
            let mut node = node.with_pause(Arc::clone(p), frames);
            if let Some(h) = hold {
                node = node.and_hold(Arc::clone(h));
            }
            let counters = AudioCounters::of(&node);
            (Box::new(node), counters)
        }
        (None, Some(h)) => {
            let node = node.with_hold(Arc::clone(h), frames);
            let counters = AudioCounters::of(&node);
            (Box::new(node), counters)
        }
        (None, None) => {
            let counters = AudioCounters::of(&node);
            (Box::new(node), counters)
        }
    }
}

/// The bus names of `out`'s tracks, track 1 first.
fn track_names(out: &OutputSettings) -> impl Iterator<Item = &str> {
    std::iter::once(out.audio_bus.as_deref().unwrap_or(MAIN_BUS_NAME))
        .chain(out.more_tracks.iter().map(String::as_str))
}

impl super::Studio {
    /// Audio tracks one destination of `out` carries: its muxer's `mux.audio_streams`,
    /// 1 for a transport that takes packets or a muxer that declares none (ADR-0170
    /// decision 7). A start with more is refused with `TracksUnsupported`; this lets a
    /// caller say so before anything opens. `NoAdapter` when no muxer takes `out`'s
    /// container.
    pub fn audio_tracks_allowed(
        adapters: &RecordAdapters,
        out: &OutputSettings,
    ) -> Result<usize, RecordError> {
        allowed_tracks(adapters, out)
    }
}

/// Audio streams one destination of `out` carries: its muxer's `mux.audio_streams`,
/// 1 for a transport that takes packets (ADR-0170 decision 7).
fn allowed_tracks(adapters: &RecordAdapters, out: &OutputSettings) -> Result<usize, RecordError> {
    let Some(container) = out.container() else {
        return Ok(1);
    };
    let (_, chosen) = select::muxer(&adapters.muxers, container)?;
    Ok(adapters
        .muxers
        .capabilities(&chosen.adapter)
        .map_or(1, mux_caps::audio_streams))
}

impl AudioPlan {
    /// The audio half of `out`, whose tracks read the bus slots `tracks` (track 1
    /// first; empty without audio). One track shares a running chain with the same
    /// settings and bus (ADR-0047 decision 3); more open one encoder each, all in one
    /// new chain, refused with `TracksUnsupported` past what the destination carries.
    /// Each stream it opens is pushed on `streams`.
    // Each argument is distinct; `Prepared::choose` passes its own.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn choose(
        adapters: &RecordAdapters,
        out: &OutputSettings,
        audio_graph: AudioGraphConfig,
        tracks: &[Option<usize>],
        running: &Chains,
        pause: bool,
        holds: bool,
        streams: &mut Vec<StreamDescriptor>,
    ) -> Result<Self, RecordError> {
        let (Some(a), Some(&bus)) = (&out.audio, tracks.first()) else {
            return Ok(AudioPlan::None);
        };
        let several = tracks.len() > 1;
        if several {
            let allowed = allowed_tracks(adapters, out)?;
            if tracks.len() > allowed {
                return Err(RecordError::TracksUnsupported { allowed });
            }
        }
        let config = a.encoder_config(audio_graph.format);
        let shared = running.audio.iter().enumerate().find_map(|(i, c)| {
            let c = c.as_ref()?;
            (!several
                && c.more.is_empty()
                && c.settings == *a
                && c.bus == bus
                && never_paused(c.pause.as_ref())
                && select::is_audio_candidate(&adapters.audio_encoders, &config, &c.chosen.adapter))
            .then_some((i, c))
        });
        if let Some((i, chain)) = shared {
            streams.push(chain.stream.clone());
            return Ok(AudioPlan::Shared(i));
        }
        if running.audio.iter().all(Option::is_some) {
            return Err(RecordError::NoEncoderSlot);
        }
        let chain_pause = pause.then(|| Arc::new(PauseWindow::default()));
        let chain_hold = holds.then(|| Arc::new(HoldSwitch::default()));
        let frames = audio_graph.block_frames as usize;
        let mut opened = Vec::with_capacity(tracks.len());
        for (k, (&bus, name)) in tracks.iter().zip(track_names(out)).enumerate() {
            let (session, chosen) = select::audio_encoder(&adapters.audio_encoders, &config)?;
            let mut stream = session.stream().clone();
            if several {
                // Named after its bus; only track 1 plays by default (decision 6).
                stream = stream.with_name(name).with_default(k == 0);
            }
            let node = EncoderNode::new(session, AUDIO_ENCODER_PACKETS);
            let (node, counters) = gated(node, chain_pause.as_ref(), chain_hold.as_ref(), frames);
            streams.push(stream.clone());
            opened.push(TrackPlan {
                node,
                counters,
                stream,
                bus,
                name: name.to_owned(),
                chosen,
            });
        }
        let mut opened = opened.into_iter();
        let first = opened.next().ok_or(RecordError::NotBound("audio track"))?;
        Ok(AudioPlan::New {
            node: first.node,
            counters: first.counters,
            chain_pause,
            chain_hold,
            chosen: first.chosen,
            settings: a.clone(),
            stream: first.stream,
            bus: first.bus,
            name: first.name,
            more: opened.collect(),
        })
    }

    /// Adds this plan's links, and a new chain's encoders, to `graph`, feeding the
    /// output `output` from stream index `first_pin` on. Returns the chain slot and
    /// the new nodes; nothing is adopted yet.
    // Each argument is distinct; `Studio::add_output` passes its own state.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn add_to(
        &self,
        graph: &mut GraphDescription,
        mixer: Option<NodeId>,
        running: &Chains,
        parts: &Partitions,
        pending: impl Fn(usize) -> bool,
        output: NodeId,
        first_pin: usize,
    ) -> Result<(Option<usize>, Option<NewAudio>), RecordError> {
        let pin = |k: usize| format!("s{}", first_pin + k);
        match self {
            AudioPlan::None => Ok((None, None)),
            AudioPlan::Shared(i) => {
                let c = running.audio[*i]
                    .as_ref()
                    .ok_or(RecordError::NotBound("shared audio encoder"))?;
                graph.connect(c.encoder.node, "packets", output, &pin(0))?;
                Ok((Some(*i), None))
            }
            AudioPlan::New {
                node, bus, more, ..
            } => {
                let mixer = mixer.ok_or(RecordError::NotBound("mixer"))?;
                let i = free_chain(&running.audio, pending)?;
                let part = parts.audio_encoder_of(i);
                let nodes =
                    std::iter::once((&**node, *bus)).chain(more.iter().map(|t| (&*t.node, t.bus)));
                let mut added = Vec::with_capacity(1 + more.len());
                for (k, (node, bus)) in nodes.enumerate() {
                    let id = add_active(graph, node.descriptor(), part)?;
                    graph.connect(mixer, buses::pin(bus), id, "in")?;
                    graph.connect(id, "packets", output, &pin(k))?;
                    added.push(Added {
                        node: id,
                        partition: part,
                        gpu: false,
                    });
                }
                let mut added = added.into_iter();
                let first = added.next().ok_or(RecordError::NotBound("audio track"))?;
                Ok((
                    Some(i),
                    Some(NewAudio {
                        first,
                        more: added.collect(),
                    }),
                ))
            }
        }
    }

    /// A new chain's nodes with the nodes `new` added for them, and its chain.
    pub(super) fn into_chain(self, new: NewAudio) -> Option<(ChainNodes, AudioChain)> {
        let AudioPlan::New {
            node,
            counters,
            chain_pause,
            chain_hold,
            chosen,
            settings,
            stream,
            bus,
            name,
            more,
        } = self
        else {
            return None;
        };
        let mut nodes = vec![(new.first, node)];
        let mut tracks = Vec::with_capacity(more.len());
        for (t, a) in more.into_iter().zip(new.more) {
            tracks.push(Track {
                encoder: a,
                bus: t.bus,
                name: t.name,
                chosen: t.chosen,
                dropped: t.counters.dropped,
                flushes: t.counters.flushes,
                tail_lost: t.counters.tail_lost,
                queue: None,
            });
            nodes.push((a, t.node));
        }
        let chain = AudioChain {
            settings,
            chosen,
            stream,
            encoder: new.first,
            dropped: counters.dropped,
            flushes: counters.flushes,
            tail_lost: counters.tail_lost,
            queue: None,
            pause: chain_pause,
            hold: chain_hold,
            bus,
            name,
            more: tracks,
        };
        Some((nodes, chain))
    }
}

impl AudioChain {
    /// Whether one of its tracks reads bus slot `slot`.
    pub(super) fn reads(&self, slot: usize) -> bool {
        self.bus == Some(slot) || self.more.iter().any(|t| t.bus == Some(slot))
    }

    /// Every encoder node, track 1 first.
    pub(super) fn encoders(&self) -> impl Iterator<Item = Added> + '_ {
        std::iter::once(self.encoder).chain(self.more.iter().map(|t| t.encoder))
    }

    /// The tracks of a multi-track chain for its start report; empty for one track.
    pub(super) fn track_reports(&self) -> Vec<TrackReport> {
        if self.more.is_empty() {
            return Vec::new();
        }
        std::iter::once(TrackReport::new(self.name.clone(), self.chosen.clone()))
            .chain(
                self.more
                    .iter()
                    .map(|t| TrackReport::new(t.name.clone(), t.chosen.clone())),
            )
            .collect()
    }

    /// Each track's counters for a multi-track chain, track 1 first; empty for one
    /// track. Their queues are known once the chain's first apply is done.
    pub(super) fn track_counters(&self) -> Vec<TrackCounters> {
        if self.more.is_empty() {
            return Vec::new();
        }
        let first = TrackCounters {
            bus: self.name.clone(),
            dropped: Arc::clone(&self.dropped),
            flushes: Arc::clone(&self.flushes),
            tail_lost: Arc::clone(&self.tail_lost),
            queue: self.queue.clone(),
        };
        let more = self.more.iter().map(|t| TrackCounters {
            bus: t.name.clone(),
            dropped: Arc::clone(&t.dropped),
            flushes: Arc::clone(&t.flushes),
            tail_lost: Arc::clone(&t.tail_lost),
            queue: t.queue.clone(),
        });
        std::iter::once(first).chain(more).collect()
    }
}

/// One track's counters, kept by the output so its finish report has them after
/// the chain is gone.
pub(super) struct TrackCounters {
    bus: String,
    dropped: Arc<AtomicU64>,
    flushes: Arc<AtomicU64>,
    tail_lost: Arc<AtomicU64>,
    queue: Option<Arc<QueueStats>>,
}

impl TrackCounters {
    /// The track's progress, with `start_drops` its aligned-start drops.
    pub(super) fn progress(&self, start_drops: u64) -> TrackProgress {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        TrackProgress {
            bus: self.bus.clone(),
            packet_drops: load(&self.dropped),
            queue_drops: self.queue.as_ref().map_or(0, |q| q.overflows()),
            flushes: load(&self.flushes),
            tail_lost: load(&self.tail_lost),
            start_drops,
        }
    }
}
