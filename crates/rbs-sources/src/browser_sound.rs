//! Page sound of a browser source (ADR-0106, ADR-0109 decision 6, ADR-0132 decision 7).
//!
//! A browser source is a picture node ([`crate::BrowserSourceNode`]) and a sound node
//! ([`BrowserSoundNode`], `org.rbs.browser.sound`, audio partition) fed by one
//! producer: the helper process. The helper writes sound packets stamped with the OS
//! monotonic clock into a studio-allocated section; the composition root's supervisor
//! thread reads them and hands them to a [`BrowserSoundFeed`]. The feed maps OS time to
//! master time at that edge ([`EdgeClock`]), applies the audio input's time map and
//! steering, resamples and remaps to the graph format ([`InputAligner`]), cuts blocks
//! on the audio block grid and pushes them into the sound node's ring. The sound node
//! only copies the due block, or writes silence, and never waits (ADR-0106 decision 3).
//!
//! The helper runs while the picture node is prepared (ADR-0107: a hidden page has no
//! process). A sound node prepared without a running helper is a silent strip.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_audio::{AudioGraphConfig, InputAligner};
use rbs_audio_io::edge::EdgeClock;
use rbs_audio_io::{AudioInputStream, AudioIoError, AudioRead, InputStats as StreamStats};
use rbs_core::media::{AudioFormat, ChannelLayout, MAX_CHANNELS};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::SampleRate;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

/// Type id of the sound node (ADR-0106 decision 5).
pub const BROWSER_SOUND_NODE_TYPE: &str = "org.rbs.browser.sound";
/// The sound node's audio output.
pub const SOUND_PIN: usize = 0;
/// How long after its end a block's sound is taken to have reached the feed: one engine
/// packet (10 ms), one supervisor poll (5 ms) and a margin. The feed renders a block
/// that much after it ends; the graph's latency must cover it plus one block.
pub const ARRIVAL: Duration = Duration::from_millis(20);

/// Engine callbacks run a millisecond or two early or late. Packets that continue the
/// previous one within this are taken as continuous, and their OS time moves toward
/// the stamped one by at most [`SLEW`] per packet.
const CONTINUOUS: Duration = Duration::from_millis(20);
/// Largest correction of a continuous packet's OS time: 50 us per packet follows a
/// clock that runs off by up to 0.5 % with 10 ms packets, and removes callback jitter.
const SLEW: Duration = Duration::from_micros(50);

/// Most sound the feed keeps waiting for the aligner.
const PENDING_LIMIT: Duration = Duration::from_millis(500);

/// The descriptor of the sound node: one audio output.
pub fn browser_sound_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(BROWSER_SOUND_NODE_TYPE),
        NodeKind::Source,
        Vec::new(),
        vec![Pin::one("audio", LinkKind::Audio)],
    )
}

/// Counters of one browser source's sound. All relaxed.
#[derive(Debug, Default)]
pub struct BrowserSoundStats {
    /// Blocks the feed pushed into the sound ring.
    pub pushed_blocks: AtomicU64,
    /// Blocks the feed dropped: the ring was full or no buffer was free.
    pub dropped_blocks: AtomicU64,
    /// Blocks the feed skipped because the graph had already passed them.
    pub skipped_blocks: AtomicU64,
    /// Blocks the sound node dropped because they were older than its block.
    pub late_blocks: AtomicU64,
    /// Times the page's sound format changed (a new aligner).
    pub format_changes: AtomicU64,
}

/// One block of sound, interleaved at the graph's format, stamped at the master time
/// of its first frame on the block grid.
struct SoundBlock {
    stamp: Timestamp,
    samples: Box<[f32]>,
}

/// The feed's end of the sound node's rings.
struct SoundInlet {
    blocks: rtrb::Producer<SoundBlock>,
    returns: rtrb::Consumer<Box<[f32]>>,
}

/// State the sound node and the feed share.
pub struct BrowserSound {
    config: AudioGraphConfig,
    clock: Arc<dyn Clock>,
    stats: Arc<BrowserSoundStats>,
    /// The ring's producer ends while the node is prepared and the feed has not taken
    /// them. Locked on the control path and by the feed, never in `process`.
    inlet: Mutex<Option<SoundInlet>>,
    /// A block time the node has seen, in nanoseconds plus one; 0 before its first.
    grid: AtomicU64,
}

impl std::fmt::Debug for BrowserSound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserSound")
            .field("config", &self.config)
            .field("grid", &self.grid)
            .finish_non_exhaustive()
    }
}

impl BrowserSound {
    /// Shared state for one browser source in a graph of `config`.
    pub fn new(config: AudioGraphConfig, clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            config,
            clock,
            stats: Arc::new(BrowserSoundStats::default()),
            inlet: Mutex::new(None),
            grid: AtomicU64::new(0),
        })
    }

    /// The sample rate the helper should ask its engine for.
    pub fn sample_rate(&self) -> u32 {
        self.config.format.sample_rate.hz()
    }

    /// The master clock the page's sound is stamped on. The picture is stamped on the
    /// same clock ([`crate::BrowserPictureEdge`]).
    pub fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    /// Counters.
    pub fn stats(&self) -> Arc<BrowserSoundStats> {
        Arc::clone(&self.stats)
    }

    fn take_inlet(&self) -> Option<SoundInlet> {
        self.inlet
            .lock()
            .map(|mut slot| slot.take())
            .unwrap_or(None)
    }

    fn grid(&self) -> Option<Timestamp> {
        match self.grid.load(Ordering::Acquire) {
            0 => None,
            n => Some(Timestamp::from_nanos(n - 1)),
        }
    }
}

/// Writes one block of the page's sound per audio tick.
///
/// While prepared it owns the sound ring and its return ring; the feed holds the other
/// ends. `process` drops blocks older than the tick's block (returning their buffers),
/// copies the due block into the pin or writes silence, and never waits, allocates or
/// frees.
pub struct BrowserSoundNode {
    descriptor: NodeDescriptor,
    shared: Arc<BrowserSound>,
    ring: Option<Ring>,
}

struct Ring {
    blocks: rtrb::Consumer<SoundBlock>,
    returns: rtrb::Producer<Box<[f32]>>,
    slots: usize,
}

impl BrowserSoundNode {
    /// A sound node over `shared`. Standby: no ring.
    pub fn new(shared: Arc<BrowserSound>) -> Self {
        Self {
            descriptor: browser_sound_descriptor(),
            shared,
            ring: None,
        }
    }

    /// Slots of the sound ring: the blocks in flight between the feed and the node
    /// (the latency) plus 2.
    pub fn ring_slots(config: &AudioGraphConfig) -> usize {
        let period = config.period().as_nanos().max(1);
        usize::try_from(config.latency.as_nanos().div_ceil(period)).unwrap_or(64) + 2
    }

    /// True while the ring exists (the node is prepared).
    pub fn ring_live(&self) -> bool {
        self.ring.is_some()
    }

    /// Allocates the rings and offers the producer ends to the feed. Control path.
    pub fn attach(&mut self) {
        if self.ring.is_some() {
            return;
        }
        let slots = Self::ring_slots(&self.shared.config);
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
    }

    /// Frees the rings. Control path. A feed that still holds the other ends sees them
    /// abandoned and stops rendering.
    pub fn detach(&mut self) {
        drop(self.shared.take_inlet());
        self.ring = None;
    }

    /// Writes the block for `block_time` into `channels` (`frames` each). Returns
    /// whether page sound was written (false: silence). Never allocates or frees.
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
        let half = Duration::from_nanos(self.shared.config.period().as_nanos() as u64 / 2);
        let mut written = false;
        if let Some(ring) = self.ring.as_mut() {
            // Bounded by the ring: every pass pops one block or stops.
            for _ in 0..=ring.slots {
                let Ok(front) = ring.blocks.peek() else {
                    break;
                };
                let late = (front.stamp + half) < block_time;
                let due = !late && front.stamp <= (block_time + half);
                if !(late || due) {
                    break;
                }
                let Ok(block) = ring.blocks.pop() else {
                    break;
                };
                if due {
                    // Blocks are cut for this channel count. A short one leaves silence.
                    rbs_audio::deinterleave(&block.samples, channels, frames);
                    written = true;
                } else {
                    self.shared
                        .stats
                        .late_blocks
                        .fetch_add(1, Ordering::Relaxed);
                }
                // The return ring holds one more than the ring, so this never fails;
                // the buffer is reused by the feed, never freed here.
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
        }
        written
    }
}

impl Drop for BrowserSoundNode {
    fn drop(&mut self) {
        if self.ring.is_some() {
            self.detach();
        }
    }
}

impl<X: Runtime> Node<X> for BrowserSoundNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        self.attach();
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, X>) {
        self.detach();
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let config = self.shared.config;
        let Some(block_time) = config.block_time(ctx.tick()) else {
            return Ok(());
        };
        let frames = config.block_frames as usize;
        let format = config.format;
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

/// Packets stamped with master time, waiting for the aligner: the "device" side of
/// an [`AudioInputStream`] whose reads run on the feed's thread.
#[derive(Default)]
struct Pending {
    packets: VecDeque<(Timestamp, Vec<f32>)>,
    /// Frames of the front packet already read.
    offset: usize,
    discontinuity: bool,
    delivered: u64,
    /// Buffers whose packets were fully read. The feed takes one back as its spare.
    freed: Vec<Vec<f32>>,
}

impl Pending {
    /// Room for a few packets so a second packet does not grow the queue.
    fn with_room() -> Self {
        Self {
            packets: VecDeque::with_capacity(8),
            freed: Vec::with_capacity(2),
            ..Self::default()
        }
    }

    /// Keeps `buf` until the feed takes a spare back. At most two, the larger ones.
    fn recycle(&mut self, mut buf: Vec<f32>) {
        buf.clear();
        if self.freed.len() < 2 {
            self.freed.push(buf);
            return;
        }
        let Some(index) = self
            .freed
            .iter()
            .enumerate()
            .min_by_key(|(_, old)| old.capacity())
            .map(|(index, _)| index)
        else {
            return;
        };
        if buf.capacity() > self.freed[index].capacity() {
            self.freed[index] = buf;
        }
    }
}

/// The page's packets as an [`AudioInputStream`] of one fixed format.
struct PageStream {
    format: AudioFormat,
    pending: Arc<Mutex<Pending>>,
}

impl AudioInputStream for PageStream {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn start(&mut self) -> Result<(), AudioIoError> {
        if let Ok(mut pending) = self.pending.lock() {
            pending.discontinuity = true;
        }
        Ok(())
    }

    fn stop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            while let Some((_, buf)) = pending.packets.pop_front() {
                pending.recycle(buf);
            }
            pending.offset = 0;
        }
    }

    fn read(&mut self, out: &mut [&mut [f32]]) -> Result<AudioRead, AudioIoError> {
        let width = self.format.channels.channels();
        if out.len() != width {
            return Err(AudioIoError::BufferMismatch);
        }
        let room = out.iter().map(|c| c.len()).min().unwrap_or(0);
        let Ok(mut pending) = self.pending.lock() else {
            return Ok(AudioRead::EMPTY);
        };
        let offset = pending.offset;
        let Some((stamp, samples)) = pending.packets.front() else {
            return Ok(AudioRead::EMPTY);
        };
        let total = samples.len() / width;
        let frames = (total - offset).min(room);
        let rate = u64::from(self.format.sample_rate.hz().max(1));
        let at = *stamp + Duration::from_nanos(offset as u64 * 1_000_000_000 / rate);
        for (c, dest) in out.iter_mut().enumerate() {
            for (f, sample) in dest[..frames].iter_mut().enumerate() {
                *sample = samples[(offset + f) * width + c];
            }
        }
        if offset + frames >= total {
            if let Some((_, buf)) = pending.packets.pop_front() {
                pending.recycle(buf);
            }
            pending.offset = 0;
        } else {
            pending.offset = offset + frames;
        }
        let discontinuity = std::mem::take(&mut pending.discontinuity);
        pending.delivered += frames as u64;
        Ok(AudioRead::new(frames, at, discontinuity))
    }

    fn available(&self) -> usize {
        let width = self.format.channels.channels().max(1);
        self.pending.lock().map_or(0, |p| {
            p.packets
                .iter()
                .map(|(_, s)| s.len() / width)
                .sum::<usize>()
                - p.offset
        })
    }

    fn stats(&self) -> StreamStats {
        let delivered = self.pending.lock().map_or(0, |p| p.delivered);
        StreamStats::new(delivered, delivered, 0, 0)
    }
}

fn layout_of(channels: u16) -> ChannelLayout {
    match channels {
        1 => ChannelLayout::Mono,
        2 => ChannelLayout::Stereo,
        4 => ChannelLayout::Surround3_1,
        6 => ChannelLayout::Surround5_1,
        8 => ChannelLayout::Surround7_1,
        n => ChannelLayout::Discrete(n.clamp(1, MAX_CHANNELS as u16) as u8),
    }
}

/// The aligner for one page format.
struct Aligned {
    rate: u32,
    channels: u16,
    aligner: InputAligner,
    pending: Arc<Mutex<Pending>>,
}

/// The producer side of a browser source's sound: runs on the composition root's
/// supervisor thread, never on the render or audio thread (ADR-0109 decision 6).
///
/// Per poll: [`observe`](Self::observe) the OS clock against master time, hand over
/// each [`packet`](Self::packet), then [`pump`](Self::pump) the due blocks into the
/// sound node's ring. Without a prepared sound node, packets are dropped at once.
pub struct BrowserSoundFeed {
    shared: Arc<BrowserSound>,
    edge: EdgeClock,
    inlet: Option<SoundInlet>,
    aligned: Option<Aligned>,
    /// The grid index of the next block to render.
    next: Option<u64>,
    /// OS time where the last packet ended (after smoothing).
    expected_os: Option<u64>,
    /// Buffers made so far (at most the ring's return capacity).
    buffers: usize,
    planar: Vec<Vec<f32>>,
    /// Packet buffer that is not queued. The supervisor swaps it for the next
    /// copy, so a queued packet keeps the buffer the caller still holds.
    /// Supervisor thread only: never the audio thread.
    spare: Vec<f32>,
}

impl BrowserSoundFeed {
    /// A feed for `shared`. Allocates nothing until a sound node is prepared.
    pub fn new(shared: Arc<BrowserSound>) -> Self {
        Self {
            shared,
            edge: EdgeClock::default(),
            inlet: None,
            aligned: None,
            next: None,
            expected_os: None,
            buffers: 0,
            planar: Vec::new(),
            spare: Vec::new(),
        }
    }

    /// Records that OS monotonic time `os_now` is master time now. Call it right after
    /// reading the OS clock, before the packets of a poll.
    pub fn observe(&mut self, os_now: u64) {
        self.edge.observe(os_now, self.shared.clock.now());
    }

    /// One packet of interleaved `samples` (`channels` wide, at `rate`) whose first
    /// frame was made at OS time `os_nanos`. Copies into the spare. After that spare
    /// exists, a later packet of the same size does not allocate.
    pub fn packet(&mut self, os_nanos: u64, rate: u32, channels: u16, samples: &[f32]) {
        let Some((stamp, n)) = self.ready(os_nanos, rate, channels, samples.len()) else {
            return;
        };
        let copied = &samples[..n];
        let _ = self.with_pending(rate, channels, |feed, pending| {
            feed.reclaim(pending);
            feed.evict(pending, stamp);
            let mut dest = feed.take_spare(n);
            dest[..n].copy_from_slice(copied);
            Self::push(pending, stamp, dest);
        });
    }

    /// Queues `samples`, already the one copy out of shared memory, and returns a
    /// spare for the next copy. The spare is not `samples`. The first packet may
    /// allocate it; a later packet of the same size does not. A larger packet may
    /// grow it. Supervisor thread only.
    pub fn take_packet(
        &mut self,
        os_nanos: u64,
        rate: u32,
        channels: u16,
        mut samples: Vec<f32>,
    ) -> Vec<f32> {
        let Some((stamp, n)) = self.ready(os_nanos, rate, channels, samples.len()) else {
            return samples;
        };
        samples.truncate(n);
        let Some(slot) = self.pending_slot(rate, channels) else {
            return samples;
        };
        let Ok(mut pending) = slot.lock() else {
            return samples;
        };
        self.reclaim(&mut pending);
        self.evict(&mut pending, stamp);
        let spare = self.take_spare(n);
        Self::push(&mut pending, stamp, samples);
        spare
    }

    /// Stamp for one packet of `len` interleaved samples, or none when it is dropped.
    fn ready(
        &mut self,
        os_nanos: u64,
        rate: u32,
        channels: u16,
        len: usize,
    ) -> Option<(Timestamp, usize)> {
        if !self.connect() || channels == 0 || rate == 0 {
            return None;
        }
        let frames = len / usize::from(channels);
        if frames == 0 {
            return None;
        }
        let n = frames * usize::from(channels);
        let duration = Duration::from_nanos(frames as u64 * 1_000_000_000 / u64::from(rate));
        let os_nanos = self.smooth(os_nanos, duration);
        let stamp = self
            .edge
            .to_master(os_nanos, duration, self.shared.clock.now());
        Some((stamp, n))
    }

    fn pending_slot(&mut self, rate: u32, channels: u16) -> Option<Arc<Mutex<Pending>>> {
        self.aligned_for(rate, channels)
            .map(|aligned| Arc::clone(&aligned.pending))
    }

    fn with_pending<T>(
        &mut self,
        rate: u32,
        channels: u16,
        body: impl FnOnce(&mut Self, &mut Pending) -> T,
    ) -> Option<T> {
        let slot = self.pending_slot(rate, channels)?;
        let mut pending = slot.lock().ok()?;
        Some(body(self, &mut pending))
    }

    fn reclaim(&mut self, pending: &mut Pending) {
        while let Some(buf) = pending.freed.pop() {
            self.stash_spare(buf);
        }
    }

    /// Drops sound older than [`PENDING_LIMIT`] and keeps its buffers.
    fn evict(&mut self, pending: &mut Pending, stamp: Timestamp) {
        while pending
            .packets
            .front()
            .is_some_and(|(first, _)| stamp.saturating_since(*first) > PENDING_LIMIT)
        {
            if let Some((_, buf)) = pending.packets.pop_front() {
                self.stash_spare(buf);
            }
            pending.offset = 0;
            pending.discontinuity = true;
        }
    }

    fn stash_spare(&mut self, buf: Vec<f32>) {
        if buf.capacity() > self.spare.capacity() {
            self.spare = buf;
        }
    }

    /// A buffer of length `n` that is not the one a queued packet holds.
    ///
    /// The first time, or when `n` does not fit, this allocates the buffer it
    /// returns and leaves another of the same size in `spare`, so the next
    /// packet of this size can take that one without allocating.
    fn take_spare(&mut self, n: usize) -> Vec<f32> {
        if self.spare.capacity() < n {
            self.spare.resize(n, 0.0);
            let out = std::mem::take(&mut self.spare);
            self.spare.resize(n, 0.0);
            return out;
        }
        let mut out = std::mem::take(&mut self.spare);
        if out.len() < n {
            out.resize(n, 0.0);
        }
        out.truncate(n);
        out
    }

    fn push(pending: &mut Pending, stamp: Timestamp, samples: Vec<f32>) {
        pending.packets.push_back((stamp, samples));
    }

    /// The OS time of a packet stamped `os_nanos`: continuous with the last packet,
    /// moved toward its stamp by at most [`SLEW`]; or its stamp after a gap.
    fn smooth(&mut self, os_nanos: u64, duration: Duration) -> u64 {
        let slew = SLEW.as_nanos() as u64;
        let at = match self.expected_os {
            Some(expected) if os_nanos.abs_diff(expected) <= CONTINUOUS.as_nanos() as u64 => {
                if os_nanos >= expected {
                    expected + (os_nanos - expected).min(slew)
                } else {
                    expected - (expected - os_nanos).min(slew)
                }
            }
            _ => os_nanos,
        };
        self.expected_os = Some(at + duration.as_nanos() as u64);
        at
    }

    /// Renders and pushes every block that is due: its sound has arrived
    /// ([`ARRIVAL`] after its end) and the graph has not passed it.
    pub fn pump(&mut self) {
        if !self.connect() {
            return;
        }
        let Some(grid) = self.shared.grid() else {
            return;
        };
        let config = self.shared.config;
        let now = self.shared.clock.now();
        let rate = u128::from(config.format.sample_rate.hz().max(1));
        let frames = u128::from(config.block_frames.max(1));
        let at = |k: u64| {
            let ns = u128::from(k) * frames * 1_000_000_000 / rate;
            grid + Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX))
        };
        let index_after = |t: Timestamp| -> u64 {
            let since = t.as_nanos().saturating_sub(grid.as_nanos());
            let k = u128::from(since) * rate / (frames * 1_000_000_000);
            u64::try_from(k).unwrap_or(u64::MAX).saturating_add(1)
        };
        // The oldest block the node can still take: the one it plays next.
        let passed = now
            .as_nanos()
            .saturating_sub(config.latency.as_nanos() as u64);
        let first_open = index_after(Timestamp::from_nanos(passed));
        let mut next = match self.next {
            Some(next) if next >= first_open => next,
            Some(next) => {
                self.shared
                    .stats
                    .skipped_blocks
                    .fetch_add(first_open - next, Ordering::Relaxed);
                first_open
            }
            None => first_open,
        };
        let n_out = config.format.channels.channels().min(MAX_CHANNELS);
        let block_frames = config.block_frames as usize;
        if self.planar.len() != n_out {
            self.planar = vec![vec![0.0; block_frames]; n_out];
        }
        loop {
            let stamp = at(next);
            if stamp + config.period() + ARRIVAL > now {
                break;
            }
            self.render(stamp, n_out, block_frames);
            next += 1;
        }
        self.next = Some(next);
    }

    fn render(&mut self, stamp: Timestamp, n_out: usize, block_frames: usize) {
        let stats = &self.shared.stats;
        let mut chans: [&mut [f32]; MAX_CHANNELS] = Default::default();
        for (slot, ch) in chans.iter_mut().zip(self.planar.iter_mut()) {
            *slot = &mut ch[..block_frames];
        }
        let mut aligner = self.aligned.as_mut().map(|aligned| &mut aligned.aligner);
        if aligner
            .as_mut()
            .is_some_and(|aligner| aligner.ingest().is_err())
        {
            aligner = None;
        }
        match aligner {
            Some(aligner) => aligner.render(stamp, 0.0, &mut chans[..n_out]),
            None => {
                for ch in chans.iter_mut().take(n_out) {
                    ch.fill(0.0);
                }
            }
        }
        let Some(inlet) = self.inlet.as_mut() else {
            return;
        };
        let samples = match inlet.returns.pop() {
            Ok(buffer) => Some(buffer),
            Err(_) if self.buffers < inlet.returns.buffer().capacity() => {
                self.buffers += 1;
                Some(vec![0.0; n_out * block_frames].into_boxed_slice())
            }
            Err(_) => None,
        };
        let Some(mut samples) = samples else {
            stats.dropped_blocks.fetch_add(1, Ordering::Relaxed);
            return;
        };
        for (f, frame) in samples.chunks_exact_mut(n_out).enumerate() {
            for (c, sample) in frame.iter_mut().enumerate() {
                *sample = self.planar[c][f];
            }
        }
        match inlet.blocks.push(SoundBlock { stamp, samples }) {
            Ok(()) => {
                stats.pushed_blocks.fetch_add(1, Ordering::Relaxed);
            }
            Err(rtrb::PushError::Full(block)) => {
                stats.dropped_blocks.fetch_add(1, Ordering::Relaxed);
                // Keep the buffer: it was counted in `buffers`.
                drop(block);
                self.buffers -= 1;
            }
        }
    }

    /// Holds the ring while a sound node is prepared. Returns false (and frees the
    /// aligner) while none is.
    fn connect(&mut self) -> bool {
        if self
            .inlet
            .as_ref()
            .is_some_and(|inlet| inlet.blocks.is_abandoned())
        {
            self.inlet = None;
        }
        if self.inlet.is_none() {
            self.inlet = self.shared.take_inlet();
            if self.inlet.is_some() {
                self.next = None;
                self.expected_os = None;
                self.buffers = 0;
            }
        }
        if self.inlet.is_none() {
            if let Some(mut aligned) = self.aligned.take() {
                aligned.aligner.stop();
            }
            return false;
        }
        true
    }

    fn aligned_for(&mut self, rate: u32, channels: u16) -> Option<&mut Aligned> {
        let same = self
            .aligned
            .as_ref()
            .is_some_and(|a| a.rate == rate && a.channels == channels);
        if !same {
            if self.aligned.take().is_some() {
                self.shared
                    .stats
                    .format_changes
                    .fetch_add(1, Ordering::Relaxed);
            }
            let sample_rate = SampleRate::new(rate)?;
            let format = AudioFormat::new(sample_rate, layout_of(channels)).ok()?;
            let pending = Arc::new(Mutex::new(Pending::with_room()));
            let stream = PageStream {
                format,
                pending: Arc::clone(&pending),
            };
            let (mut aligner, _stats) =
                InputAligner::new(Box::new(stream), self.shared.config, Duration::ZERO);
            aligner.start().ok()?;
            self.aligned = Some(Aligned {
                rate,
                channels,
                aligner,
                pending,
            });
        }
        self.aligned.as_mut()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

    use rbs_core::time::TestClock;

    use super::*;

    /// The OS clock runs this far ahead of master time.
    const OS: u64 = 7_000_000_000_000;

    fn tone(t_ns: u64) -> f32 {
        (0.5 * (std::f64::consts::TAU * 440.0 * t_ns as f64 * 1e-9).sin()) as f32
    }

    /// A page making `rate` Hz sound in 10 ms packets, a supervisor polling every
    /// 5 ms, and an audio thread taking each block `latency` after its time.
    struct Rig {
        clock: TestClock,
        shared: Arc<BrowserSound>,
        node: BrowserSoundNode,
        feed: BrowserSoundFeed,
        rate: u32,
        channels: u16,
        /// OS-time error of each packet's stamp, in ns, by packet index.
        jitter: fn(u64) -> i64,
        made: u64,
        next_block: u64,
        written: Vec<bool>,
        out: Vec<(Timestamp, Vec<f32>)>,
    }

    impl Rig {
        fn new(rate: u32, channels: u16) -> Self {
            let clock = TestClock::new();
            let shared = BrowserSound::new(AudioGraphConfig::default(), Arc::new(clock.clone()));
            let mut node = BrowserSoundNode::new(Arc::clone(&shared));
            node.attach();
            let feed = BrowserSoundFeed::new(Arc::clone(&shared));
            Self {
                clock,
                shared,
                node,
                feed,
                rate,
                channels,
                jitter: |_| 0,
                made: 0,
                next_block: 0,
                written: Vec::new(),
                out: Vec::new(),
            }
        }

        /// Runs the rig for `ms` milliseconds of master time.
        fn run(&mut self, ms: u64) {
            let config = self.shared.config;
            let period = config.period().as_nanos() as u64;
            let latency = config.latency.as_nanos() as u64;
            for _ in 0..ms {
                self.clock.advance(Duration::from_millis(1));
                let now = self.clock.now().as_nanos();
                // The page: every 10 ms, the 10 ms just made.
                let packet = u64::from(self.rate) / 100;
                let made_until = now / 10_000_000 * 10_000_000;
                while self.made * 1_000_000_000 / u64::from(self.rate) + 10_000_000 <= made_until {
                    let first = self.made;
                    let mut samples = Vec::new();
                    for f in first..first + packet {
                        let t = f * 1_000_000_000 / u64::from(self.rate);
                        for _ in 0..self.channels {
                            samples.push(tone(t));
                        }
                    }
                    let exact = OS + first * 1_000_000_000 / u64::from(self.rate);
                    let os = exact.saturating_add_signed((self.jitter)(first / packet));
                    self.feed.observe(OS + now);
                    self.feed.packet(os, self.rate, self.channels, &samples);
                    self.made += packet;
                }
                // The supervisor pumps every 5 ms.
                if now.is_multiple_of(5_000_000) {
                    self.feed.observe(OS + now);
                    self.feed.pump();
                }
                // The audio thread.
                while self.next_block * period + latency <= now {
                    let t = Timestamp::from_nanos(self.next_block * period);
                    let frames = config.block_frames as usize;
                    let mut l = vec![9.0; frames];
                    let mut r = vec![9.0; frames];
                    let wrote = self.node.fill(t, &mut [&mut l, &mut r], frames);
                    self.written.push(wrote);
                    self.out.push((t, l));
                    self.next_block += 1;
                }
            }
        }
    }

    #[test]
    fn page_sound_reaches_the_block_of_its_master_time() {
        let mut rig = Rig::new(48_000, 2);
        rig.run(1_000);
        // After the first few blocks every block carries the page's sound.
        let first = rig.written.iter().position(|w| *w).unwrap();
        assert!(first <= 10, "first sound at block {first}");
        assert!(rig.written[first..].iter().all(|w| *w));
        // Each sample is the page's sample of the same master time.
        for (t, l) in &rig.out[first + 5..] {
            for (f, sample) in l.iter().enumerate() {
                let at = t.as_nanos() + f as u64 * 1_000_000_000 / 48_000;
                assert!(
                    (sample - tone(at)).abs() < 2e-3,
                    "{t:?} {f}: {sample} {}",
                    tone(at)
                );
            }
        }
        let stats = rig.shared.stats();
        assert_eq!(stats.late_blocks.load(Ordering::Relaxed), 0);
        assert_eq!(stats.dropped_blocks.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn callback_jitter_does_not_move_the_sound() {
        let mut rig = Rig::new(48_000, 2);
        // Up to 1.8 ms early or late, as engine callbacks are.
        rig.jitter = |i| {
            [
                0, 1_800_000, -1_700_000, 900_000, -400_000, 1_200_000, -1_800_000,
            ][(i % 7) as usize]
        };
        rig.run(2_000);
        let first = rig.written.iter().position(|w| *w).unwrap();
        assert!(rig.written[first..].iter().all(|w| *w));
        let mut worst = 0.0f32;
        for (t, l) in &rig.out[first + 50..] {
            for (f, sample) in l.iter().enumerate() {
                let at = t.as_nanos() + f as u64 * 1_000_000_000 / 48_000;
                worst = worst.max((sample - tone(at)).abs());
            }
        }
        // Without the slew limit the worst sample is off by about 0.45.
        assert!(worst < 2e-3, "worst {worst}");
        assert_eq!(rig.shared.stats().late_blocks.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_44_1_khz_mono_page_is_resampled_and_spread_to_stereo() {
        let mut rig = Rig::new(44_100, 1);
        rig.run(1_000);
        let first = rig.written.iter().position(|w| *w).unwrap();
        assert!(rig.written[first..].iter().all(|w| *w));
        for (t, l) in &rig.out[first + 10..] {
            for (f, sample) in l.iter().enumerate() {
                let at = t.as_nanos() + f as u64 * 1_000_000_000 / 48_000;
                assert!((sample - tone(at)).abs() < 2e-2, "{t:?} {f}");
            }
        }
    }

    #[test]
    fn without_a_prepared_node_the_page_is_not_buffered() {
        let mut rig = Rig::new(48_000, 2);
        rig.node.detach();
        rig.run(200);
        assert!(rig.written.iter().all(|w| !*w));
        assert!(rig.feed.aligned.is_none());
        // Prepared again: sound resumes with the page's present, not its past.
        rig.node.attach();
        rig.run(300);
        assert!(rig.written.iter().rev().take(10).all(|w| *w));
        let (t, l) = rig.out.last().unwrap();
        assert!((l[0] - tone(t.as_nanos())).abs() < 2e-3);
    }

    #[test]
    fn the_node_block_path_does_not_allocate() {
        let mut rig = Rig::new(48_000, 2);
        rig.run(300);
        let frames = 480;
        let mut l = vec![0.0; frames];
        let mut r = vec![0.0; frames];
        // Blocks in the ring and blocks due: copy, drop late ones, return buffers.
        rig.clock.advance(Duration::from_millis(30));
        rig.feed.observe(OS + rig.clock.now().as_nanos());
        rig.feed.pump();
        let t = Timestamp::from_nanos(rig.next_block * 10_000_000);
        let info = allocation_counter::measure(|| {
            for k in 0..4 {
                let at = t + Duration::from_millis(10 * k);
                rig.node.fill(at, &mut [&mut l, &mut r], frames);
            }
        });
        assert_eq!(info.count_total, 0, "{info:?}");
    }

    #[test]
    fn a_later_packet_reuses_the_spare() {
        let mut rig = Rig::new(48_000, 2);
        rig.clock.advance(Duration::from_millis(100));
        let now = OS + rig.clock.now().as_nanos();
        rig.feed.observe(now);
        let small = vec![0.25f32; 960];
        rig.feed.packet(now, 48_000, 2, &small);
        let second = allocation_counter::measure(|| {
            rig.feed.packet(now + 10_000_000, 48_000, 2, &small);
        });
        assert_eq!(second.count_total, 0, "{second:?}");
        {
            let pending = rig.feed.aligned.as_ref().unwrap().pending.lock().unwrap();
            assert_eq!(pending.packets.len(), 2);
            assert!(pending.packets[0].1.iter().all(|sample| *sample == 0.25));
            assert!(pending.packets[1].1.iter().all(|sample| *sample == 0.25));
            assert_ne!(pending.packets[0].1.as_ptr(), pending.packets[1].1.as_ptr());
        }
        let big = vec![0.5f32; 1920];
        let grew = allocation_counter::measure(|| {
            rig.feed.packet(now + 20_000_000, 48_000, 2, &big);
        });
        assert!(grew.count_total > 0, "{grew:?}");
        let again = allocation_counter::measure(|| {
            rig.feed.packet(now + 40_000_000, 48_000, 2, &big);
        });
        assert_eq!(again.count_total, 0, "{again:?}");
        let pending = rig.feed.aligned.as_ref().unwrap().pending.lock().unwrap();
        assert!(pending.packets[0].1.iter().all(|sample| *sample == 0.25));
        assert!(pending.packets[2].1.iter().all(|sample| *sample == 0.5));
        drop(pending);

        // The supervisor path moves the section copy in. The queued buffer is that
        // one; the spare returned for the next copy is a different allocation.
        let owned = vec![0.75f32; 960];
        let owned_ptr = owned.as_ptr();
        let spare = rig.feed.take_packet(now + 50_000_000, 48_000, 2, owned);
        assert_ne!(spare.as_ptr(), owned_ptr);
        let pending = rig.feed.aligned.as_ref().unwrap().pending.lock().unwrap();
        let queued = &pending.packets.back().unwrap().1;
        assert_eq!(queued.as_ptr(), owned_ptr);
        assert!(queued.iter().all(|sample| *sample == 0.75));
    }

    #[test]
    fn the_node_writes_silence_and_never_waits_when_nothing_came() {
        let clock = TestClock::new();
        let shared = BrowserSound::new(AudioGraphConfig::default(), Arc::new(clock));
        let mut node = BrowserSoundNode::new(shared);
        node.attach();
        let mut l = vec![1.0; 480];
        let mut r = vec![1.0; 480];
        assert!(!node.fill(Timestamp::from_nanos(0), &mut [&mut l, &mut r], 480));
        assert!(l.iter().chain(r.iter()).all(|s| *s == 0.0));
    }

    /// ADR-0109 decision 12, ADR-0106 decision 4: a picture and the sound the page
    /// made at the same instant reach the program at the same master time, within one
    /// audio block, though the supervisor sees each picture a few milliseconds late.
    #[test]
    fn a_picture_and_its_sound_reach_the_program_at_one_master_time() {
        let mut rig = Rig::new(48_000, 2);
        let period = rig.shared.config.period().as_nanos() as u64;
        let mut edge = crate::BrowserPictureEdge::new(rig.shared.clock());
        let sink = crate::BrowserFrameSink::new();
        let rgba = [255u8; 2 * 2 * 4];
        let mut seen = VecDeque::new();
        let mut checked = 0;
        for ms in 1..=1_500u64 {
            rig.run(1);
            let now = ms * 1_000_000;
            // The page paints every 16 ms; the supervisor copies it 3 to 12 ms later.
            if ms.is_multiple_of(16) {
                seen.push_back((now, now + (3 + ms % 10) * 1_000_000));
            }
            while let Some(&(paint, at)) = seen.front() {
                if at > now {
                    break;
                }
                seen.pop_front();
                let stamp = edge.stamp(OS + paint, OS + now);
                sink.publish_at(&rgba, 2, 2, Some(stamp));
                // Not on air before its time; on air from it.
                assert!(sink.take_due(stamp - Duration::from_nanos(1)).is_none());
                let shown = sink.take_due(stamp).expect("due at its stamp");
                assert_eq!(shown.timestamp(), stamp);
                // Once the edge has learned the clocks (one second of OS time), the
                // picture sits in the block that plays the sound made with it.
                if paint > 1_100_000_000 {
                    let error = stamp.as_nanos().abs_diff(paint);
                    assert!(error < period, "picture {paint} stamped {stamp:?}");
                    checked += 1;
                }
            }
        }
        assert!(checked > 10, "{checked}");
        // The sound of those instants is in the block of the same master time.
        let first = rig.written.iter().position(|w| *w).unwrap();
        for (t, l) in &rig.out[first + 5..] {
            let at = t.as_nanos();
            assert!((l[0] - tone(at)).abs() < 2e-3, "{t:?}");
        }
    }
}
