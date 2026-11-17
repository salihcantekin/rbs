//! Media-file source nodes (ADR-0106, ADR-0108).
//!
//! One media source is two nodes fed by one decode worker:
//!
//! - the picture node [`MediaPictureNode`] (`org.rbs.media`, render partition, one
//!   video output). It holds the picture rings while it is prepared;
//! - the sound node [`MediaSoundNode`] (`org.rbs.media.sound`, audio partition, one
//!   audio output). It owns the sound ring while it is prepared.
//!
//! Playback (the worker and its decoder) runs while either node is prepared, so a
//! source that is heard but not seen still plays.
//!
//! The worker stamps every picture and every sound block with master time (ADR-0106
//! decision 4). It paces each stream by master time, never waits on a ring, drops and
//! counts what does not fit, resamples and remaps sound to the audio graph's format and
//! cuts it on the audio block grid (ADR-0106 decision 3). Both `process` methods only
//! dequeue, copy, and hand buffers back on return rings: they do not allocate, free,
//! block, resample, or read the file.
//!
//! Playback is controlled through [`MediaControl`] (ADR-0108 decision 10): play,
//! pause, seek, and restart write records that every item's generation is checked
//! against.
//!
//! The host writes whether a program or hold scene draws the source
//! ([`MediaControl::set_on_air`]); a source only a preview scene draws is cued. The
//! host calls [`MediaControl::service`] on the control path: it releases the worker
//! and the decoder of a source paused or cued for `pause_release_s`.
//!
//! Not in this step (ADR-0108 decisions 3, 5, 7, 11, 12): the probe and the open on a
//! worker thread instead of the control thread, the reaper, the play range, live
//! keys, native surfaces, and NV12/P010 upload.

mod control;
mod pcm;
mod picture;
mod player;
mod sound;
mod worker;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};

pub use control::{MediaControl, PlayState, RECORDS as MEDIA_CONTROL_RECORDS};
pub use picture::{MediaNodeError, MediaPhase, MediaPictureNode};
pub use player::PAUSE_RELEASE;
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

/// Native pictures a source may hold, decoded and not yet imported, in `unseen` and on
/// the ring. With the one the worker has just decoded and two the importer holds (the
/// shown and the held picture, while their leases wait for the importer's GPU work to
/// retire), that is the surface budget of ADR-0108 decision 5 (18), counted against the
/// importer's shared `MAX_SURFACES` of 64. An RGB surface is aliased, so the shown and
/// held textures are the surfaces themselves; a 4:2:0 surface is decoded into a pool
/// texture and its lease goes back once that decode retired.
pub const NATIVE_WAITING: u32 = PICTURE_SLOTS as u32 - 1;

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

/// The picture node and sound node of one media source, sharing one player.
pub struct MediaPair {
    /// Adopt into the render partition.
    pub picture: MediaPictureNode,
    /// Adopt into the audio partition at the binder's sound node, when there is one.
    /// Dropped otherwise: the worker then decodes no sound. Preparing it alone plays
    /// the file too.
    pub sound: MediaSoundNode,
    /// Play, pause, seek, and restart (ADR-0108 decision 10). For the host.
    pub control: MediaControl,
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
    /// Reads of the control records that a write tore; the node kept its previous
    /// read for that tick or block (ADR-0108 decision 10).
    pub torn_controls: AtomicU64,
    /// Pictures and sound blocks skipped because a later command ended their
    /// generation before their stamp.
    pub superseded: AtomicU64,
    /// Native pictures (`IoSurface`, `D3dSharedHandle`) the picture node imported
    /// zero-copy.
    pub imported: AtomicU64,
    /// Native pictures the worker or the picture node holds now and has not imported
    /// yet, at most [`NATIVE_WAITING`] (ADR-0108 decision 5).
    pub native_waiting: AtomicU32,
    /// Native pictures dropped because the import failed (the last texture repeats).
    pub import_failures: AtomicU64,
}

/// One block of sound, interleaved at the graph's format, stamped at the master time
/// of its first frame on the block grid.
pub(crate) struct SoundBlock {
    pub(crate) stamp: Timestamp,
    pub(crate) epoch: u32,
    pub(crate) generation: u32,
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
    /// The picture node is prepared: the worker queues pictures. Without it, pictures
    /// are decoded for the pass timing only, not copied or queued.
    pub(crate) picture_attached: AtomicBool,
    /// A block time the sound node has seen, in nanoseconds plus one; 0 before its
    /// first block. Every block time is on the same grid.
    pub(crate) grid: AtomicU64,
    /// The epoch of the worker that may write sound now; older blocks are skipped.
    pub(crate) epoch: AtomicU32,
    /// Master time, in nanoseconds plus one, from which the sound node counts a block
    /// of silence as an underrun; 0 while no worker plays sound.
    pub(crate) sound_from: AtomicU64,
    /// The generation of the blocks `sound_from` belongs to.
    pub(crate) sound_gen: AtomicU32,
    /// The file has an audio stream (known after the first open).
    pub(crate) has_audio: AtomicBool,
    /// The stated duration in nanoseconds plus one; 0 while unknown.
    pub(crate) duration: AtomicU64,
    /// The render adapter's LUID plus one, recorded by the picture node's `prepare` on
    /// Direct3D 12; 0 when unknown. The worker opens with it (port invariant 11).
    pub(crate) import_luid: AtomicU64,
    /// The audio graph's block period.
    pub(crate) block_period: Duration,
    /// The playback control records (ADR-0108 decision 10).
    pub(crate) controls: control::Controls,
    /// The picture rings of a run the picture node does not hold yet, and the rings it
    /// let go of, for the control path to free. The picture node takes them in
    /// `dequeue` with `try_lock` when `handoff_ready` is set; it never blocks.
    pub(crate) handoff: Mutex<Handoff>,
    pub(crate) handoff_ready: AtomicBool,
    /// Media cursor in nanoseconds: the picture last shown, kept across Standby.
    cursor: AtomicU64,
    /// Start of the play range in nanoseconds (ADR-0108 decision 11; 0 by default).
    range_start: AtomicU64,
    /// End of the play range in nanoseconds plus one; 0 for the end of the file.
    range_end: AtomicU64,
    /// Where the worker's pause stopped, in nanoseconds plus one; 0 while playing.
    paused_cursor: AtomicU64,
    /// The next worker opens keeping alpha: set while the source is armed as the
    /// stinger (ADR-0140 decisions 2 and 4).
    pub(crate) keep_alpha: AtomicBool,
    /// The newest generation, plus one, whose first picture the worker queued; 0
    /// before any (ADR-0140 decision 4). Set by the worker, read on the control path.
    pub(crate) queued_generation: AtomicU32,
    /// Further adapters to try, in order, when the source's adapter cannot keep alpha
    /// (ADR-0140 decision 2). Set once by the host after the instance is made.
    pub(crate) alpha_alternates: Mutex<Vec<Arc<dyn rbs_media_decode::MediaDecoderFactory>>>,
    /// Playback. Locked on the control path only.
    player: Mutex<player::Player>,
}

/// Picture rings in transit between the player and the picture node.
#[derive(Default)]
pub(crate) struct Handoff {
    /// The current run's rings and progress counters.
    pub(crate) rings: Option<(player::Rings, Arc<worker::Progress>)>,
    /// Rings the picture node replaced; freed on the control path.
    pub(crate) retired: Option<(player::Rings, Arc<worker::Progress>)>,
}

impl Shared {
    fn new(player: player::Player, block_period: Duration) -> Arc<Self> {
        Arc::new(Self {
            stats: Arc::new(MediaStats::default()),
            inlet: Mutex::new(None),
            sound_attached: AtomicBool::new(false),
            picture_attached: AtomicBool::new(false),
            grid: AtomicU64::new(0),
            epoch: AtomicU32::new(0),
            sound_from: AtomicU64::new(0),
            sound_gen: AtomicU32::new(0),
            has_audio: AtomicBool::new(false),
            duration: AtomicU64::new(0),
            import_luid: AtomicU64::new(0),
            block_period,
            controls: control::Controls::new(),
            handoff: Mutex::new(Handoff::default()),
            handoff_ready: AtomicBool::new(false),
            cursor: AtomicU64::new(0),
            range_start: AtomicU64::new(0),
            range_end: AtomicU64::new(0),
            paused_cursor: AtomicU64::new(0),
            keep_alpha: AtomicBool::new(false),
            queued_generation: AtomicU32::new(0),
            alpha_alternates: Mutex::new(Vec::new()),
            player: Mutex::new(player),
        })
    }

    /// The player. Control path only; a poisoned lock is taken over, since every
    /// field stays valid between statements.
    pub(crate) fn player(&self) -> MutexGuard<'_, player::Player> {
        self.player
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The cursor: where a pause stopped while paused, else the picture shown last.
    pub(crate) fn cursor(&self) -> Duration {
        match self.paused_cursor.load(Ordering::Acquire) {
            0 => Duration::from_nanos(self.cursor.load(Ordering::Relaxed)),
            n => Duration::from_nanos(n - 1),
        }
    }

    /// Records where a pause stopped (`None` when playing again). Worker only.
    pub(crate) fn set_paused_cursor(&self, at: Option<Duration>) {
        let nanos = at.map_or(0, |d| {
            u64::try_from(d.as_nanos())
                .unwrap_or(u64::MAX - 1)
                .saturating_add(1)
        });
        self.paused_cursor.store(nanos, Ordering::Release);
    }

    /// Moves the cursor while no worker follows it (asleep, released, or about to
    /// start): the stored pause point is replaced too. Control path.
    pub(crate) fn move_cursor(&self, cursor: Duration) {
        self.set_paused_cursor(None);
        self.set_cursor(cursor);
    }

    /// Where the play range starts (ADR-0108 decision 11).
    pub(crate) fn range_start(&self) -> Duration {
        Duration::from_nanos(self.range_start.load(Ordering::Acquire))
    }

    /// Where the play range ends; `None` for the end of the file.
    pub(crate) fn range_end(&self) -> Option<Duration> {
        match self.range_end.load(Ordering::Acquire) {
            0 => None,
            end => Some(Duration::from_nanos(end - 1)),
        }
    }

    /// Stores the play range. The caller checked `start < end`.
    pub(crate) fn set_range(&self, start: Duration, end: Option<Duration>) {
        let nanos = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX - 1);
        self.range_start.store(nanos(start), Ordering::Release);
        self.range_end
            .store(end.map_or(0, |e| nanos(e) + 1), Ordering::Release);
    }

    /// `position` moved into the play range: before the start or at or after the end
    /// it is the start (decision 12).
    pub(crate) fn in_range(&self, position: Duration) -> Duration {
        let start = self.range_start();
        if position < start || self.range_end().is_some_and(|end| position >= end) {
            start
        } else {
            position
        }
    }

    pub(crate) fn set_cursor(&self, cursor: Duration) {
        self.cursor.store(
            u64::try_from(cursor.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// The handoff slot. Control path only (the picture node uses `try_lock`).
    pub(crate) fn handoff(&self) -> MutexGuard<'_, Handoff> {
        self.handoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Offers a run's picture rings to the picture node and frees rings it let go of.
    /// Control path.
    pub(crate) fn offer_rings(&self, rings: Option<(player::Rings, Arc<worker::Progress>)>) {
        let mut slot = self.handoff();
        slot.retired = None;
        slot.rings = rings;
        self.handoff_ready
            .store(slot.rings.is_some(), Ordering::Release);
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
    let shared = Shared::new(
        player::Player::new(
            factory,
            path,
            loop_playback,
            restart_when_shown,
            clock,
            audio,
        ),
        audio.period(),
    );
    MediaPair {
        picture: MediaPictureNode::new(Arc::clone(&shared)),
        control: MediaControl::new(Arc::clone(&shared)),
        sound: MediaSoundNode::new(audio, shared),
    }
}

/// The render adapter's LUID as stored in `Shared::import_luid`: the LUID plus one, or
/// 0 when there is none (not Direct3D 12, or never prepared).
pub(crate) fn luid_slot(luid: Option<u64>) -> u64 {
    luid.map_or(0, |l| l.wrapping_add(1))
}

/// The decode target of a stored `import_luid` value.
fn target_of(slot: u64) -> rbs_media_decode::DecodeTarget {
    match slot {
        0 => rbs_media_decode::DecodeTarget::none(),
        luid => rbs_media_decode::DecodeTarget::adapter(luid.wrapping_sub(1)),
    }
}

impl Shared {
    /// Where the worker's native pictures will be imported (port invariant 11).
    pub(crate) fn decode_target(&self) -> rbs_media_decode::DecodeTarget {
        let target = target_of(self.import_luid.load(Ordering::Acquire));
        if self.keep_alpha.load(Ordering::Acquire) {
            target.with_keep_alpha()
        } else {
            target
        }
    }

    /// The adapters to try after the source's own refused to keep alpha.
    pub(crate) fn alternates(&self) -> Vec<Arc<dyn rbs_media_decode::MediaDecoderFactory>> {
        self.alpha_alternates
            .lock()
            .map(|a| a.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod luid_tests {
    use super::{luid_slot, target_of};
    use rbs_media_decode::DecodeTarget;

    #[test]
    fn the_stored_luid_gives_the_decode_target() {
        assert_eq!(target_of(luid_slot(None)), DecodeTarget::none());
        assert_eq!(target_of(luid_slot(Some(0))), DecodeTarget::adapter(0));
        let luid = 0x0000_0001_0000_d2a5;
        assert_eq!(
            target_of(luid_slot(Some(luid))),
            DecodeTarget::adapter(luid)
        );
    }
}
