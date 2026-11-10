//! The output node: plays the blocks of one audio partition on an output device,
//! converted to the device's rate and channels and steered against the device's clock
//! (ADR-0097 decision 3, audio graph spec "Output node").

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_audio_io::{AudioIoError, AudioOutput, OutputProducer};
use rbs_core::media::MAX_CHANNELS;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::{ParamReceiver, ParamSender, channel};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

use crate::config::AudioGraphConfig;
use crate::input::{MAX_CORRECTION, STEER_GAIN, matrix};
use crate::resample::{AFTER, BEFORE, Kernel, TAPS, apply};

/// Node type id of [`OutputNode`].
pub const OUTPUT_NODE_TYPE: &str = "org.rbs.audio.output";

/// Default fill of the output ring the node steers towards, measured before each
/// block's push. It covers the device's period and the block length with room for
/// scheduling jitter; it adds to the monitoring latency.
pub const DEFAULT_OUTPUT_TARGET: Duration = Duration::from_millis(30);

/// Highest monitor volume (linear gain).
pub const MAX_OUTPUT_VOLUME: f32 = 1.0;

/// Time a volume change takes from 0 to full scale (declick).
pub const OUTPUT_VOLUME_RAMP: Duration = Duration::from_millis(20);

/// The smoothed fill moves this fraction of the measured fill's distance per block,
/// so the device's burst-wise pulls do not steer the rate.
const FILL_SMOOTHING: f64 = 1.0 / 32.0;

/// Blocks the node's input FIFO holds.
const FIFO_BLOCKS: usize = 8;

/// Parameters of a running output node (ADR-0023 channel).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputParams {
    /// Linear gain applied before the device, `0.0..=MAX_OUTPUT_VOLUME`; changes ramp
    /// over [`OUTPUT_VOLUME_RAMP`].
    pub volume: f32,
}

impl OutputParams {
    /// Full volume.
    pub const UNITY: Self = Self { volume: 1.0 };

    /// The given volume (clamped when applied).
    pub fn with_volume(volume: f32) -> Self {
        Self { volume }
    }
}

impl Default for OutputParams {
    fn default() -> Self {
        Self::UNITY
    }
}

/// Counters of one output node, readable from any thread.
#[derive(Debug, Default)]
pub struct OutputNodeStats {
    blocks: AtomicU64,
    frames_out: AtomicU64,
    primes: AtomicU64,
    skipped_frames: AtomicU64,
    overflow_frames: AtomicU64,
    invalid_inputs: AtomicU64,
    fill: AtomicU64,
    correction_ppm: AtomicU64,
}

/// A snapshot of [`OutputNodeStats`]. Counts only grow.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutputCounters {
    /// Blocks taken from the input.
    pub blocks: u64,
    /// Device frames pushed into the ring (audio, not priming silence).
    pub frames_out: u64,
    /// Times the ring was filled with silence up to the target: the first audio, and
    /// every time the device had played the ring dry (an underrun).
    pub primes: u64,
    /// Graph frames skipped because the ring held far more than the target (the device
    /// stalled, or the graph delivered a burst).
    pub skipped_frames: u64,
    /// Graph frames dropped because the node's FIFO was full.
    pub overflow_frames: u64,
    /// Input blocks of a format other than the graph's, ignored.
    pub invalid_inputs: u64,
    /// The ring's fill in device frames, measured before the last push.
    pub fill: u64,
    /// The last block's rate correction in ppm (positive: the device gets fewer
    /// frames), as two's complement bits of an `i64`.
    pub correction_ppm: i64,
}

impl OutputNodeStats {
    /// The counters now.
    pub fn read(&self) -> OutputCounters {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        OutputCounters {
            blocks: l(&self.blocks),
            frames_out: l(&self.frames_out),
            primes: l(&self.primes),
            skipped_frames: l(&self.skipped_frames),
            overflow_frames: l(&self.overflow_frames),
            invalid_inputs: l(&self.invalid_inputs),
            fill: l(&self.fill),
            correction_ppm: l(&self.correction_ppm) as i64,
        }
    }

    fn add(counter: &AtomicU64, n: u64) {
        if n > 0 {
            counter.fetch_add(n, Ordering::Relaxed);
        }
    }
}

/// Everything `prepare` allocates.
struct State {
    /// Graph channels.
    channels: usize,
    /// Device channels.
    dev_channels: usize,
    /// Frames per channel the FIFO holds.
    cap: usize,
    /// Planar FIFO of graph frames: channel `c` is `fifo[c * cap..(c + 1) * cap]`,
    /// frames `0..len` valid, frame 0 the oldest.
    fifo: Box<[f32]>,
    len: usize,
    /// Resampler position in FIFO frames; always at least `BEFORE`.
    p: f64,
    /// Graph frames per device frame, nominal.
    nominal: f64,
    /// Device frames per second, nominal.
    rate: f64,
    kernel: Kernel,
    matrix: [[f32; MAX_CHANNELS]; MAX_CHANNELS],
    /// Interleaved device frames of one push.
    scratch: Box<[f32]>,
    /// Ring fill the node steers towards, device frames.
    target: usize,
    /// Smoothed ring fill, device frames.
    fill: f64,
    /// Silence has been queued ahead of the first audio.
    primed: bool,
    /// Current gain, and its largest change per device frame.
    gain: f32,
    gain_step: f32,
}

impl State {
    fn new(
        config: &AudioGraphConfig,
        device: rbs_core::media::AudioFormat,
        dev_channels: usize,
        target: Duration,
        ring: usize,
    ) -> Self {
        let rate = f64::from(device.sample_rate.hz());
        let graph_rate = f64::from(config.format.sample_rate.hz());
        let nominal = graph_rate / rate;
        let channels = config.format.channels.channels().clamp(1, MAX_CHANNELS);
        let block = config.block_frames as usize;
        let cap = FIFO_BLOCKS * block + 2 * TAPS;
        let scratch_frames = (cap as f64 / (nominal * (1.0 - MAX_CORRECTION))).ceil() as usize + 2;
        // At least two blocks' worth, at most half the ring so an overrun stays rare.
        let wanted = (target.as_secs_f64() * rate).round() as usize;
        let two_blocks = ((2 * block) as f64 / nominal).ceil() as usize;
        let target = wanted.max(two_blocks).min(ring / 2).max(1);
        let ramp = (OUTPUT_VOLUME_RAMP.as_secs_f64() * rate).max(1.0);
        Self {
            channels,
            dev_channels,
            cap,
            fifo: vec![0.0; cap * channels].into_boxed_slice(),
            // The first `BEFORE` frames are silence, so the window has its history.
            len: BEFORE,
            p: BEFORE as f64,
            nominal,
            rate,
            kernel: Kernel::new(config.format.sample_rate, device.sample_rate),
            matrix: matrix(config.format.channels, device.channels),
            scratch: vec![0.0; scratch_frames * dev_channels].into_boxed_slice(),
            target,
            fill: target as f64,
            primed: false,
            gain: 0.0,
            gain_step: (1.0 / ramp) as f32,
        }
    }

    fn scratch_frames(&self) -> usize {
        self.scratch.len() / self.dev_channels
    }

    /// Drops the oldest `n` frames, keeping the position on the same audio.
    fn drop_front(&mut self, n: usize) {
        let n = n.min(self.len);
        if n == 0 {
            return;
        }
        for c in 0..self.channels {
            let base = c * self.cap;
            self.fifo.copy_within(base + n..base + self.len, base);
        }
        self.len -= n;
        self.p -= n as f64;
    }

    /// Graph frames the window can still reach past the position.
    fn ready(&self) -> usize {
        self.len.saturating_sub(self.p.floor() as usize + AFTER + 1)
    }

    /// Pushes `frames` frames of silence.
    fn push_silence(&mut self, producer: &mut OutputProducer, frames: usize) {
        let mut left = frames;
        while left > 0 {
            let n = left.min(self.scratch_frames());
            self.scratch[..n * self.dev_channels].fill(0.0);
            if producer.push(&self.scratch[..n * self.dev_channels]) < n {
                return;
            }
            left -= n;
        }
    }
}

/// Plays one audio partition's blocks on an output device (ADR-0097 decision 3).
///
/// Each block goes into a FIFO; a variable-ratio resampler converts it to the
/// device's rate and channels, and its rate is steered so the output ring stays near
/// a target fill: the device's clock drifts from the master clock like an input's. An
/// empty ring (first audio, or an underrun) is filled with silence up to the target;
/// a ring far above it skips input. Monitoring latency is the graph latency plus the
/// target plus the device's own latency ([`OutputNode::latency`]).
pub struct OutputNode {
    desc: NodeDescriptor,
    output: Box<dyn AudioOutput>,
    producer: Option<OutputProducer>,
    config: AudioGraphConfig,
    target: Duration,
    params: ParamReceiver<OutputParams>,
    stats: Arc<OutputNodeStats>,
    state: Option<State>,
}

/// The descriptor of output nodes: kind `Output`, one audio input "in".
pub fn output_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(OUTPUT_NODE_TYPE),
        NodeKind::Output,
        vec![Pin::one("in", LinkKind::Audio)],
        vec![],
    )
}

impl OutputNode {
    /// A node for `output` (opened, Standby) in a graph of `config`, steering its ring
    /// towards `target` ([`DEFAULT_OUTPUT_TARGET`]; clamped to at least two blocks and
    /// at most half the ring).
    pub fn new(
        output: Box<dyn AudioOutput>,
        config: AudioGraphConfig,
        target: Duration,
    ) -> (Self, ParamSender<OutputParams>, Arc<OutputNodeStats>) {
        let (tx, rx) = channel(OutputParams::UNITY);
        let stats = Arc::new(OutputNodeStats::default());
        (
            Self {
                desc: output_descriptor(),
                output,
                producer: None,
                config,
                target,
                params: rx,
                stats: Arc::clone(&stats),
                state: None,
            },
            tx,
            stats,
        )
    }

    /// How far the device plays behind the graph's real time: graph latency, ring
    /// target and the device's reported latency.
    pub fn latency(&self) -> Duration {
        self.config.latency + self.target + Duration::from_nanos(self.output.latency_ns())
    }
}

fn device_error(e: AudioIoError) -> NodeError {
    match e {
        AudioIoError::EndpointLost => NodeError::Permanent("audio output endpoint lost"),
        AudioIoError::FormatChanged => NodeError::Permanent("audio output format changed"),
        AudioIoError::EndpointNotFound | AudioIoError::PermissionDenied => {
            NodeError::Permanent("audio output cannot start")
        }
        _ => NodeError::Backend("audio output failed"),
    }
}

impl<X: Runtime> Node<X> for OutputNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        if self.producer.is_none() {
            self.producer = self.output.producer();
        }
        let Some(producer) = self.producer.as_ref() else {
            return Err(NodeError::Permanent("audio output ring already taken"));
        };
        self.state = Some(State::new(
            &self.config,
            self.output.format(),
            producer.channels(),
            self.target,
            producer.capacity(),
        ));
        self.output.start().map_err(|e| match e {
            AudioIoError::DeviceBusy => NodeError::Failed(format!("audio output busy: {e}")),
            other => match device_error(other) {
                NodeError::Backend(_) => {
                    NodeError::Failed(format!("audio output start failed: {other}"))
                }
                permanent => permanent,
            },
        })
    }

    fn release(&mut self, _: &mut PrepareCtx<'_, X>) {
        self.output.stop();
        self.state = None;
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        self.params.update();
        let volume = self.params.get().volume.clamp(0.0, MAX_OUTPUT_VOLUME);
        let (Some(st), Some(producer)) = (self.state.as_mut(), self.producer.as_mut()) else {
            return Err(NodeError::NotReady);
        };
        self.output.status().map_err(device_error)?;
        let stats = &*self.stats;

        // 1. The block, into the FIFO.
        if let Some(block) = ctx.inputs().audio(0, 0) {
            let n = block.frames();
            if block.format() != self.config.format || n == 0 || n > st.cap - BEFORE {
                stats.invalid_inputs.fetch_add(1, Ordering::Relaxed);
            } else {
                if st.len + n > st.cap {
                    let over = st.len + n - st.cap;
                    st.drop_front(over);
                    OutputNodeStats::add(&stats.overflow_frames, over as u64);
                    if st.p < BEFORE as f64 {
                        st.p = BEFORE as f64;
                    }
                }
                for c in 0..st.channels {
                    let at = c * st.cap + st.len;
                    match block.channel(c) {
                        Some(src) => st.fifo[at..at + n].copy_from_slice(&src[..n]),
                        None => st.fifo[at..at + n].fill(0.0),
                    }
                }
                st.len += n;
                stats.blocks.fetch_add(1, Ordering::Relaxed);
            }
        }
        if st.ready() == 0 {
            return Ok(());
        }

        // 2. Ring fill: prime it, skip input, or steer the rate.
        let queued = producer.queued();
        stats.fill.store(queued as u64, Ordering::Relaxed);
        if !st.primed || queued == 0 {
            let missing = st.target.saturating_sub(queued);
            st.push_silence(producer, missing);
            st.primed = true;
            st.fill = st.target as f64;
            stats.primes.fetch_add(1, Ordering::Relaxed);
        } else if queued > 2 * st.target {
            let excess = ((queued - st.target) as f64 * st.nominal) as usize;
            let skip = excess.min(st.ready());
            st.p += skip as f64;
            OutputNodeStats::add(&stats.skipped_frames, skip as u64);
            st.fill = queued as f64;
        } else {
            st.fill += (queued as f64 - st.fill) * FILL_SMOOTHING;
        }
        let err_s = (st.fill - st.target as f64) / st.rate;
        let correction = (err_s * STEER_GAIN).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        stats
            .correction_ppm
            .store((correction * 1e6).round() as i64 as u64, Ordering::Relaxed);
        let step = st.nominal * (1.0 + correction);

        // 3. Resample, map channels, apply the volume.
        let mut weights = [0.0f32; TAPS];
        let (n_in, n_dev) = (st.channels, st.dev_channels);
        let limit = st.scratch_frames();
        let mut n = 0;
        while n < limit {
            let i = st.p.floor() as usize;
            if i + AFTER >= st.len {
                break;
            }
            st.kernel.coefficients(st.p - i as f64, &mut weights);
            let mut v = [0.0f32; MAX_CHANNELS];
            for (c, v) in v.iter_mut().enumerate().take(n_in) {
                let at = c * st.cap + i - BEFORE;
                *v = apply(&weights, &st.fifo[at..at + TAPS]);
            }
            if st.gain != volume {
                st.gain = if st.gain < volume {
                    (st.gain + st.gain_step).min(volume)
                } else {
                    (st.gain - st.gain_step).max(volume)
                };
            }
            let frame = &mut st.scratch[n * n_dev..(n + 1) * n_dev];
            for (o, out) in frame.iter_mut().enumerate() {
                let mut acc = 0.0;
                if let Some(row) = st.matrix.get(o) {
                    for c in 0..n_in {
                        acc += row[c] * v[c];
                    }
                }
                *out = acc * st.gain;
            }
            n += 1;
            st.p += step;
        }
        producer.push(&st.scratch[..n * n_dev]);
        OutputNodeStats::add(&stats.frames_out, n as u64);

        // 4. Forget what no later frame can reach.
        let keep_from = (st.p.floor() as usize).saturating_sub(BEFORE);
        st.drop_front(keep_from);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_is_a_valid_audio_output() {
        let d = output_descriptor();
        assert_eq!(d.kind, NodeKind::Output);
        assert!(d.validate().is_ok(), "{:?}", d.validate());
    }
}
