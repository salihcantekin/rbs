//! The media sound node `org.rbs.media.sound` (ADR-0106 decisions 2, 3, and 9).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rbs_audio::AudioGraphConfig;
use rbs_core::media::MAX_CHANNELS;
use rbs_core::time::Timestamp;
use rbs_graph::node::NodeDescriptor;
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

use super::control::{Verdict, View};
use super::player::User;
use super::{LEAD, SOUND_PIN, Shared, SoundBlock, SoundInlet, media_sound_descriptor};

/// Writes one block of the media file's sound per audio tick.
///
/// While prepared it owns the sound ring and its return ring; the worker holds the
/// other ends. Preparing it plays the file even while the picture node is not
/// prepared (a source that is heard but not seen). `process` drops blocks older than the tick's block (returning their
/// buffers), copies the due block into the pin or writes silence, and never waits.
pub struct MediaSoundNode {
    descriptor: NodeDescriptor,
    config: AudioGraphConfig,
    shared: Arc<Shared>,
    ring: Option<Ring>,
    /// The control records as last read (ADR-0108 decision 10).
    view: View,
}

struct Ring {
    blocks: rtrb::Consumer<SoundBlock>,
    returns: rtrb::Producer<Box<[f32]>>,
    slots: usize,
}

impl MediaSoundNode {
    pub(crate) fn new(config: AudioGraphConfig, shared: Arc<Shared>) -> Self {
        Self {
            descriptor: media_sound_descriptor(),
            config,
            shared,
            ring: None,
            view: View::new(),
        }
    }

    /// Slots of the sound ring: the lead plus the graph latency plus 2 blocks
    /// (ADR-0108 decision 5).
    pub fn ring_slots(config: &AudioGraphConfig) -> usize {
        let period = config.period().as_nanos().max(1);
        let span = (LEAD + config.latency).as_nanos();
        usize::try_from(span.div_ceil(period)).unwrap_or(64) + 2
    }

    /// True while the ring exists (the node is prepared).
    pub fn ring_live(&self) -> bool {
        self.ring.is_some()
    }

    /// Counters, shared with the picture node.
    pub fn stats(&self) -> Arc<super::MediaStats> {
        Arc::clone(&self.shared.stats)
    }

    /// Allocates the rings, offers the producer side to the worker, and joins
    /// playback: with the picture node not prepared, this starts the worker. Control
    /// path. On an error the rings are freed again.
    pub fn attach(&mut self) -> Result<(), super::MediaNodeError> {
        if self.ring.is_some() {
            return Ok(());
        }
        let slots = Self::ring_slots(&self.config);
        let (blocks_tx, blocks) = rtrb::RingBuffer::new(slots);
        let (returns, returns_rx) = rtrb::RingBuffer::new(slots + 1);
        if let Ok(mut slot) = self.shared.inlet.lock() {
            *slot = Some(SoundInlet {
                blocks: blocks_tx,
                returns: returns_rx,
            });
        }
        self.ring = Some(Ring {
            blocks,
            returns,
            slots,
        });
        self.shared.sound_attached.store(true, Ordering::Release);
        let shared = Arc::clone(&self.shared);
        let joined = shared.player().acquire(User::Sound, &shared);
        if joined.is_err() {
            self.detach();
        }
        joined
    }

    /// Frees the rings and leaves playback; with the picture node not prepared either,
    /// this stops the worker. Control path. A worker that still holds the other ends
    /// sees them abandoned and stops writing sound.
    pub fn detach(&mut self) {
        self.shared.sound_attached.store(false, Ordering::Release);
        // Silence before the next attach's first block is not an underrun.
        self.shared.sound_from.store(0, Ordering::Release);
        drop(self.shared.take_inlet());
        self.ring = None;
        let shared = Arc::clone(&self.shared);
        shared.player().release(User::Sound, &shared);
    }

    /// Writes the block for `block_time` into `channels` (`frames` each). Returns
    /// whether the file's sound was written (false: silence). Never allocates or frees.
    pub fn fill(
        &mut self,
        block_time: Timestamp,
        channels: &mut [&mut [f32]],
        frames: usize,
    ) -> bool {
        if self.shared.grid.load(Ordering::Relaxed) == 0 {
            self.shared
                .grid
                .store(block_time.as_nanos().saturating_add(1), Ordering::Release);
        }
        let half = Duration::from_nanos(self.config.period().as_nanos() as u64 / 2);
        let epoch = self.shared.epoch.load(Ordering::Acquire);
        let stats = &self.shared.stats;
        if !self.view.refresh(&self.shared.controls) {
            stats.torn_controls.fetch_add(1, Ordering::Relaxed);
        }
        let records = *self.view.records();
        let mut written = false;
        if let Some(ring) = self.ring.as_mut() {
            // Bounded by the ring: every pass pops one block or stops.
            for _ in 0..=ring.slots {
                let Ok(front) = ring.blocks.peek() else {
                    break;
                };
                // A later command ended this block's generation before its stamp.
                let superseded = match records.verdict(front.generation, front.stamp) {
                    Verdict::Wait => break,
                    Verdict::Skip => true,
                    Verdict::Play => false,
                };
                let stale = front.epoch != epoch || superseded;
                let late = (front.stamp + half) < block_time;
                let due = !late && front.stamp <= (block_time + half);
                if !(stale || late || due) {
                    break;
                }
                let Ok(block) = ring.blocks.pop() else {
                    break;
                };
                if due && !stale {
                    let width = channels.len().max(1);
                    // Blocks are exactly `width * frames` long (the worker cuts them
                    // for this config); a short one leaves the rest silent.
                    let whole = (block.samples.len() / width).min(frames);
                    for (c, dest) in channels.iter_mut().enumerate() {
                        let (head, tail) = dest[..frames].split_at_mut(whole);
                        for (sample, frame) in
                            head.iter_mut().zip(block.samples.chunks_exact(width))
                        {
                            *sample = frame[c];
                        }
                        tail.fill(0.0);
                    }
                    written = true;
                } else if late && !stale {
                    stats.late_blocks.fetch_add(1, Ordering::Relaxed);
                } else if superseded {
                    stats.superseded.fetch_add(1, Ordering::Relaxed);
                }
                // The return ring holds one more than the ring, so this never fails;
                // the buffer is reused by the worker, never freed here.
                let _ = ring.returns.push(block.samples);
                if written {
                    break;
                }
            }
        }
        if !written {
            for dest in channels.iter_mut() {
                dest[..frames].fill(0.0);
            }
            // Silence is an underrun only while the record that applies plays and the
            // worker has already played sound for it: a pause, and the gap between a
            // command and the new generation's first block, are not underruns.
            let from = self.shared.sound_from.load(Ordering::Acquire);
            let generation = self.shared.sound_gen.load(Ordering::Acquire);
            let playing = records
                .at(block_time)
                .is_some_and(|r| !r.paused && r.generation == generation);
            if playing && from != 0 && block_time.as_nanos() >= from - 1 {
                stats.sound_underruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        written
    }
}

impl Drop for MediaSoundNode {
    fn drop(&mut self) {
        if self.ring.is_some() {
            self.detach();
        }
    }
}

impl<X: Runtime> Node<X> for MediaSoundNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        self.attach()
            .map_err(|error| NodeError::Failed(error.to_string()))
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, X>) {
        self.detach();
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let Some(block_time) = self.config.block_time(ctx.tick()) else {
            return Ok(());
        };
        let frames = self.config.block_frames as usize;
        let format = self.config.format;
        let n = format.channels.channels().min(MAX_CHANNELS);
        let mut out = ctx.outputs().audio(SOUND_PIN)?;
        let mut chans: [&mut [f32]; MAX_CHANNELS] = Default::default();
        for (slot, ch) in chans.iter_mut().zip(out.channels_mut(n)) {
            *slot = ch;
        }
        self.fill(block_time, &mut chans[..n], frames);
        out.commit(format, block_time, frames)?;
        Ok(())
    }
}
