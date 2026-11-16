//! Media-file source nodes (ADR-0106, ADR-0108).
//!
//! One media source is two nodes fed by one decode worker:
//!
//! - the picture node [`MediaPictureNode`] (`org.rbs.media`, render partition, one
//!   video output). It owns the worker's lifecycle on the control path;
//! - the sound node [`MediaSoundNode`] (`org.rbs.media.sound`, audio partition, one
//!   audio output). It owns the sound ring while it is prepared.
//!
//! The worker stamps every picture and every sound block with master time (ADR-0106
//! decision 4). It paces each stream by master time, never waits on a ring, drops and
//! counts what does not fit, resamples and remaps sound to the audio graph's format and
//! cuts it on the audio block grid (ADR-0106 decision 3). Both `process` methods only
//! dequeue, copy, and hand buffers back on return rings: they do not allocate, free,
//! block, resample, or read the file.
//!
//! Not in this step (ADR-0108 decisions 3, 5, 7, 10): the probe and the open on a
//! worker thread instead of the control thread, the reaper, the control handle with
//! generations, native surfaces, and NV12/P010 upload.

mod pcm;
mod picture;
mod sound;
mod worker;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};

pub use picture::{MediaNodeError, MediaPhase, MediaPictureNode};
pub use sound::MediaSoundNode;

/// Type id of the picture node.
pub const MEDIA_NODE_TYPE: &str = "org.rbs.media";
/// Type id of the sound node (ADR-0106 decision 5).
pub const MEDIA_SOUND_NODE_TYPE: &str = "org.rbs.media.sound";
/// The picture node's video output.
pub const VIDEO_PIN: usize = 0;
/// The sound node's audio output.
pub const SOUND_PIN: usize = 0;
/// How far ahead of master time the worker decodes, and how long after the first
/// picture motion and sound start (ADR-0108 decision 5).
pub const LEAD: Duration = Duration::from_millis(150);
/// Most picture items in flight (ADR-0108 decision 5). The byte cap and the lead can
/// stop the worker earlier.
pub const PICTURE_SLOTS: usize = 16;

/// The descriptor of the picture node: one video output.
pub fn media_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(MEDIA_NODE_TYPE),
        NodeKind::Source,
        Vec::new(),
        vec![Pin::one("video", LinkKind::Video)],
    )
}

/// The descriptor of the sound node: one audio output.
pub fn media_sound_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(MEDIA_SOUND_NODE_TYPE),
        NodeKind::Source,
        Vec::new(),
        vec![Pin::one("audio", LinkKind::Audio)],
    )
}

/// The picture node and sound node of one media source, sharing one worker.
pub struct MediaPair {
    /// Adopt into the render partition.
    pub picture: MediaPictureNode,
    /// Adopt into the audio partition at the binder's sound node, when there is one.
    /// Dropped otherwise: the worker then decodes no sound.
    pub sound: MediaSoundNode,
}

/// Counters for one media source. All relaxed.
#[derive(Debug, Default)]
pub struct MediaStats {
    /// Pictures the worker handed to the picture ring.
    pub uploaded: AtomicU64,
    /// Pictures the picture node skipped because a later one was already due.
    pub dropped_video: AtomicU64,
    /// Ticks that showed the same picture again.
    pub repeated_video: AtomicU64,
    /// Pictures the worker dropped because the picture ring was full.
    pub worker_dropped_pictures: AtomicU64,
    /// Sound blocks the worker dropped: the ring was full, no buffer was free, or the
    /// block was already too late for the graph.
    pub worker_dropped_blocks: AtomicU64,
    /// Sound blocks the sound node dropped because they were older than its block.
    pub late_blocks: AtomicU64,
    /// Sound node blocks written as silence while the file was playing sound. A file
    /// with no audio stream, a pause, and the end hold are not underruns.
    pub sound_underruns: AtomicU64,
}

/// One block of sound, interleaved at the graph's format, stamped at the master time
/// of its first frame on the block grid.
pub(crate) struct SoundBlock {
    pub(crate) stamp: Timestamp,
    pub(crate) epoch: u32,
    pub(crate) samples: Box<[f32]>,
}

/// The worker's end of the sound node's rings.
pub(crate) struct SoundInlet {
    pub(crate) blocks: rtrb::Producer<SoundBlock>,
    pub(crate) returns: rtrb::Consumer<Box<[f32]>>,
}

/// State the picture node, the sound node, and the worker share.
pub(crate) struct Shared {
    pub(crate) stats: Arc<MediaStats>,
    /// The sound ring's producer side while the sound node is prepared and no worker
    /// holds it. Locked on the control path and by the worker, never in `process`.
    pub(crate) inlet: Mutex<Option<SoundInlet>>,
    /// The sound node is prepared: the worker decodes sound.
    pub(crate) sound_attached: AtomicBool,
    /// A block time the sound node has seen, in nanoseconds plus one; 0 before its
    /// first block. Every block time is on the same grid.
    pub(crate) grid: AtomicU64,
    /// The epoch of the worker that may write sound now; older blocks are skipped.
    pub(crate) epoch: AtomicU32,
    /// Master time, in nanoseconds plus one, from which the sound node counts a block
    /// of silence as an underrun; 0 while no worker plays sound.
    pub(crate) sound_from: AtomicU64,
    /// The file has an audio stream (known after the first open).
    pub(crate) has_audio: AtomicBool,
}

impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stats: Arc::new(MediaStats::default()),
            inlet: Mutex::new(None),
            sound_attached: AtomicBool::new(false),
            grid: AtomicU64::new(0),
            epoch: AtomicU32::new(0),
            sound_from: AtomicU64::new(0),
            has_audio: AtomicBool::new(false),
        })
    }

    /// Takes the inlet. The lock is held only to move an `Option`.
    pub(crate) fn take_inlet(&self) -> Option<SoundInlet> {
        self.inlet
            .lock()
            .map(|mut slot| slot.take())
            .unwrap_or(None)
    }

    /// Puts an inlet back unless a newer one is already there or its ring is gone.
    pub(crate) fn return_inlet(&self, inlet: SoundInlet) {
        if inlet.blocks.is_abandoned() {
            return;
        }
        if let Ok(mut slot) = self.inlet.lock() {
            if slot.is_none() {
                *slot = Some(inlet);
            }
        }
    }

    pub(crate) fn grid(&self) -> Option<Timestamp> {
        match self.grid.load(Ordering::Acquire) {
            0 => None,
            n => Some(Timestamp::from_nanos(n - 1)),
        }
    }
}

/// The two nodes of one media source (ADR-0106 decision 1). Nothing is opened and no
/// ring is allocated until a node is prepared.
// Each argument is a distinct source setting; a builder would only move them.
#[allow(clippy::too_many_arguments)]
pub fn media_pair(
    factory: Arc<dyn rbs_media_decode::MediaDecoderFactory>,
    path: std::path::PathBuf,
    loop_playback: bool,
    restart_when_shown: bool,
    clock: Arc<dyn rbs_core::time::Clock>,
    audio: rbs_audio::AudioGraphConfig,
) -> MediaPair {
    let shared = Shared::new();
    MediaPair {
        picture: MediaPictureNode::new(
            factory,
            path,
            loop_playback,
            restart_when_shown,
            clock,
            audio,
            Arc::clone(&shared),
        ),
        sound: MediaSoundNode::new(audio, shared),
    }
}
