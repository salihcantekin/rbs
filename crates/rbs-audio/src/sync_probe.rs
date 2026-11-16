//! The sync probe: a sink that logs the energy of every 1 ms of the sound it reads,
//! while a sync offset measurement runs (ADR-0126 decision 3).
//!
//! The log is written once, front to back, by the audio thread and read by the
//! control thread: a fixed array of atomics and a published length, so neither side
//! allocates, blocks or locks. When the log is full, later sub-blocks are counted as
//! dropped; a measurement is shorter than the log.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use rbs_core::media::{AudioFormat, MAX_CHANNELS};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

use crate::config::AudioGraphConfig;

/// Node type id of [`SyncProbeNode`].
pub const SYNC_PROBE_NODE_TYPE: &str = "org.rbs.audio.sync-probe";

/// Length of one logged sub-block.
pub const SYNC_PROBE_STEP: Duration = Duration::from_millis(1);

/// The descriptor of sync probes: kind `Output`, one audio input "in", no outputs.
pub fn sync_probe_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(SYNC_PROBE_NODE_TYPE),
        NodeKind::Output,
        vec![Pin::one("in", LinkKind::Audio)],
        vec![],
    )
}

/// What a probe logged: `(start of the sub-block in master ns, mean square)` pairs in
/// time order, written by the audio thread, read by any thread without locks.
#[derive(Debug)]
pub struct SyncProbeLog {
    times: Box<[AtomicU64]>,
    energies: Box<[AtomicU32]>,
    len: AtomicUsize,
    dropped: AtomicU64,
    invalid_inputs: AtomicU64,
}

impl SyncProbeLog {
    fn new(capacity: usize) -> Self {
        Self {
            times: (0..capacity).map(|_| AtomicU64::new(0)).collect(),
            energies: (0..capacity).map(|_| AtomicU32::new(0)).collect(),
            len: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            invalid_inputs: AtomicU64::new(0),
        }
    }

    /// Entries the log can hold.
    pub fn capacity(&self) -> usize {
        self.times.len()
    }

    /// Entries written so far.
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    /// True before the first entry.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sub-blocks not logged because the log was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Blocks refused because their format or length was not the graph's.
    pub fn invalid_inputs(&self) -> u64 {
        self.invalid_inputs.load(Ordering::Relaxed)
    }

    /// Appends the entries from index `from` to the current length to `out` and
    /// returns the new length, the `from` of the next call. Off the audio thread.
    pub fn read_from(&self, from: usize, out: &mut Vec<(i64, f32)>) -> usize {
        let len = self.len();
        for i in from.min(len)..len {
            let t = self.times[i].load(Ordering::Relaxed);
            let e = f32::from_bits(self.energies[i].load(Ordering::Relaxed));
            out.push((i64::try_from(t).unwrap_or(i64::MAX), e));
        }
        len
    }

    fn push(&self, time_ns: u64, energy: f32) {
        // Single writer: the probe's process.
        let i = self.len.load(Ordering::Relaxed);
        if i >= self.times.len() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.times[i].store(time_ns, Ordering::Relaxed);
        self.energies[i].store(energy.to_bits(), Ordering::Relaxed);
        self.len.store(i + 1, Ordering::Release);
    }
}

/// Sums samples into sub-blocks of [`SYNC_PROBE_STEP`] across block boundaries.
/// Pure arithmetic, no allocation.
#[derive(Debug, Clone)]
pub struct SubBlockEnergy {
    rate: u64,
    channels: usize,
    /// Frames per sub-block (`rate / 1000`, at least 1).
    step: usize,
    /// Master time of the next frame (ns), `None` before the first block.
    next_ns: Option<u64>,
    start_ns: u64,
    sum: f64,
    frames: usize,
}

impl SubBlockEnergy {
    /// An accumulator for blocks of `format`.
    pub fn new(format: AudioFormat) -> Self {
        let rate = u64::from(format.sample_rate.hz()).max(1);
        Self {
            rate,
            channels: format.channels.channels().clamp(1, MAX_CHANNELS),
            step: usize::try_from(rate / 1000).unwrap_or(1).max(1),
            next_ns: None,
            start_ns: 0,
            sum: 0.0,
            frames: 0,
        }
    }

    /// Adds a block of `frames` frames whose first frame is at `time_ns`, calling
    /// `emit(start_ns, mean_square)` for each completed sub-block. A block that does
    /// not continue the previous one (a gap or a jump of more than one frame) drops
    /// the partial sub-block and starts again at `time_ns`.
    pub fn add(
        &mut self,
        channels: &[&[f32]],
        frames: usize,
        time_ns: u64,
        mut emit: impl FnMut(u64, f32),
    ) {
        let frame_ns = 1_000_000_000 / self.rate;
        let continues = self
            .next_ns
            .is_some_and(|n| n.abs_diff(time_ns) <= frame_ns);
        if !continues {
            self.sum = 0.0;
            self.frames = 0;
        }
        let chans = &channels[..channels.len().min(self.channels)];
        // The block's first frame: where the previous block ended when it continues
        // (so per-block rounding of `time_ns` does not jitter the sub-blocks).
        let base = match self.next_ns {
            Some(n) if continues => n,
            _ => time_ns,
        };
        let at = |f: usize| base + u64::try_from(f).unwrap_or(0) * 1_000_000_000 / self.rate;
        for f in 0..frames {
            if self.frames == 0 {
                self.start_ns = at(f);
            }
            for c in chans {
                let v = f64::from(c.get(f).copied().unwrap_or(0.0));
                self.sum += v * v;
            }
            self.frames += 1;
            if self.frames == self.step {
                let n = (self.frames * self.channels) as f64;
                #[allow(clippy::cast_possible_truncation)] // an energy fits an f32
                emit(self.start_ns, (self.sum / n) as f32);
                self.sum = 0.0;
                self.frames = 0;
            }
        }
        self.next_ns = Some(at(frames));
    }
}

/// A sink that logs the 1 ms energy of the block on its input (ADR-0126 decision 3).
///
/// It is a parallel tap: it reads its producer's block and changes nothing. It exists
/// only while a sync offset measurement runs (ADR-0013).
pub struct SyncProbeNode {
    desc: NodeDescriptor,
    config: AudioGraphConfig,
    acc: SubBlockEnergy,
    log: Arc<SyncProbeLog>,
}

impl SyncProbeNode {
    /// A probe for blocks of `config` whose log holds `window` of sound (plus one
    /// block). Allocates; call off the audio thread.
    pub fn new(config: AudioGraphConfig, window: Duration) -> (Self, Arc<SyncProbeLog>) {
        let steps = window.as_millis() + config.period().as_millis() + 1;
        let log = Arc::new(SyncProbeLog::new(
            usize::try_from(steps).unwrap_or(usize::MAX),
        ));
        let node = Self {
            desc: sync_probe_descriptor(),
            config,
            acc: SubBlockEnergy::new(config.format),
            log: Arc::clone(&log),
        };
        (node, log)
    }
}

impl<X: Runtime> Node<X> for SyncProbeNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let Some(time) = self.config.block_time(ctx.tick()) else {
            return Ok(());
        };
        let Some(block) = ctx.inputs().audio(0, 0) else {
            return Ok(());
        };
        let frames = block.frames();
        if block.format() != self.config.format || frames > self.config.block_frames as usize {
            self.log.invalid_inputs.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let n = self.config.format.channels.channels().min(MAX_CHANNELS);
        let mut chans: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
        for (c, slot) in chans.iter_mut().enumerate().take(n) {
            *slot = block.channel(c).unwrap_or(&[]);
        }
        let log = &self.log;
        self.acc
            .add(&chans[..n], frames, time.as_nanos(), |t, e| log.push(t, e));
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, X>) {}
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rbs_core::media::ChannelLayout;
    use rbs_core::timing::SampleRate;

    fn format(rate: u32) -> AudioFormat {
        AudioFormat::new(SampleRate::new(rate).unwrap(), ChannelLayout::Stereo).unwrap()
    }

    #[test]
    fn the_descriptor_is_a_valid_audio_output() {
        let d = sync_probe_descriptor();
        assert_eq!(d.kind, NodeKind::Output);
        assert!(d.validate().is_ok(), "{:?}", d.validate());
        let (node, log) = SyncProbeNode::new(AudioGraphConfig::default(), Duration::from_secs(10));
        assert_eq!(Node::<()>::descriptor(&node), &d);
        assert!(log.capacity() >= 10_000);
        assert!(log.is_empty());
    }

    #[test]
    fn sub_blocks_are_one_millisecond_and_span_blocks() {
        let mut acc = SubBlockEnergy::new(format(48_000));
        let block = vec![0.5f32; 480 + 24]; // 10.5 ms: sub-blocks straddle blocks
        let mut got = Vec::new();
        let mut t = 1_000_000_000u64;
        for _ in 0..4 {
            acc.add(&[&block, &block], block.len(), t, |s, e| got.push((s, e)));
            t += 10_500_000;
        }
        assert_eq!(got.len(), 42);
        for (i, (s, e)) in got.iter().enumerate() {
            assert_eq!(*s, 1_000_000_000 + i as u64 * 1_000_000, "sub-block {i}");
            assert!((e - 0.25).abs() < 1e-6);
        }
    }

    #[test]
    fn a_gap_restarts_the_sub_block_at_the_new_time() {
        let mut acc = SubBlockEnergy::new(format(48_000));
        let block = vec![0.1f32; 72]; // 1.5 ms
        let mut got = Vec::new();
        acc.add(&[&block], 72, 0, |s, e| got.push((s, e)));
        acc.add(&[&block], 72, 50_000_000, |s, e| got.push((s, e)));
        assert_eq!(
            got.iter().map(|g| g.0).collect::<Vec<_>>(),
            vec![0, 50_000_000]
        );
    }

    #[test]
    fn a_click_lands_in_its_millisecond() {
        let mut acc = SubBlockEnergy::new(format(48_000));
        let mut block = vec![0.0f32; 480];
        block[48 * 7 + 3] = 1.0; // inside the 8th millisecond
        let mut got = Vec::new();
        acc.add(&[&block, &block], 480, 0, |s, e| got.push((s, e)));
        let loud: Vec<u64> = got.iter().filter(|g| g.1 > 0.0).map(|g| g.0).collect();
        assert_eq!(loud, vec![7_000_000]);
    }

    #[test]
    fn adding_blocks_and_logging_do_not_allocate() {
        let mut acc = SubBlockEnergy::new(format(48_000));
        let log = SyncProbeLog::new(20_000);
        let block = vec![0.25f32; 480];
        let mut t = 0u64;
        let c = allocation_counter::measure(|| {
            for _ in 0..1000 {
                acc.add(&[&block, &block], 480, t, |s, e| log.push(s, e));
                t += 10_000_000;
            }
        });
        assert_eq!(c.count_total, 0);
        assert_eq!(log.len(), 10_000);
    }

    #[test]
    fn the_log_reads_in_order_and_counts_what_did_not_fit() {
        let log = SyncProbeLog::new(3);
        for i in 0..5u64 {
            log.push(i * 1_000_000, i as f32);
        }
        assert_eq!((log.len(), log.dropped()), (3, 2));
        let mut out = Vec::new();
        let next = log.read_from(0, &mut out);
        assert_eq!(next, 3);
        assert_eq!(out, vec![(0, 0.0), (1_000_000, 1.0), (2_000_000, 2.0)]);
        assert_eq!(log.read_from(next, &mut out), 3);
        assert_eq!(out.len(), 3);
    }
}
