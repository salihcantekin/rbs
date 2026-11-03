//! The input node: one audio input stream aligned to master time, drift-corrected and
//! converted to the graph format (ADR-0037 decision 5, audio graph spec "Input node").

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_audio_io::{AudioInputStream, AudioIoError};
use rbs_core::media::{AudioFormat, ChannelLayout, MAX_CHANNELS};
use rbs_core::time::Timestamp;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::{ParamReceiver, ParamSender, channel};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

use crate::config::AudioGraphConfig;
use crate::resample::{AFTER, BEFORE, Kernel, TAPS, apply};

/// Node type id of [`InputNode`].
pub const INPUT_NODE_TYPE: &str = "org.rbs.audio.input";

/// A read whose timestamp is within this of where the time map expects it continues
/// the map (and nudges it); further off, it is a gap or an overlap. The audio I/O port
/// promises timestamps within 2 ms of the truth.
pub const MAP_TOLERANCE: Duration = Duration::from_millis(2);

/// Errors between the resampler's position and the wanted time beyond this make it
/// jump (a resync, audible) instead of slewing.
pub const RESYNC_THRESHOLD: Duration = Duration::from_millis(5);

/// Proportional gain of the steering loop, per second of error per second.
pub const STEER_GAIN: f64 = 0.5;

/// Largest rate correction of the steering loop (2000 ppm, inaudible as pitch).
pub const MAX_CORRECTION: f64 = 0.002;

/// Longest OS packet the FIFO makes room for beyond the latency and offset.
pub const MAX_PACKET: Duration = Duration::from_millis(100);

/// Margin kept between an advanced source and real time (audio of that age must have
/// arrived).
const ADVANCE_MARGIN: Duration = Duration::from_millis(5);

/// A run further back than this from the map's end restarts the map instead of being
/// dropped as an overlap.
const RESTART_OVERLAP: Duration = Duration::from_secs(1);

/// The map moves this fraction of the mean timestamp error per block.
const MAP_GAIN: f64 = 1.0 / 16.0;

/// Parameters of a running input node (ADR-0023 channel).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputParams {
    /// Sync offset in ns: positive delays the source (its audio of time `t - offset`
    /// plays at `t`), negative advances it.
    pub offset_ns: i64,
}

impl InputParams {
    /// No offset.
    pub const NONE: Self = Self { offset_ns: 0 };

    /// Delays the source by `d` (for audio that is earlier than its video).
    pub fn delayed(d: Duration) -> Self {
        Self {
            offset_ns: i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        }
    }

    /// Advances the source by `d` (for audio that is later than its video, such as a
    /// Bluetooth microphone).
    pub fn advanced(d: Duration) -> Self {
        Self {
            offset_ns: -i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        }
    }
}

/// Counters of one input node, readable from any thread.
#[derive(Debug, Default)]
pub struct InputStats {
    frames_in: AtomicU64,
    resyncs: AtomicU64,
    late_frames: AtomicU64,
    discarded_frames: AtomicU64,
    gaps_filled: AtomicU64,
    overlaps_dropped: AtomicU64,
    read_errors: AtomicU64,
    error_ppm: AtomicU64,
    presentation_ns: AtomicU64,
}

/// A snapshot of [`InputStats`]. All counts only grow.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputCounters {
    /// Frames read from the stream.
    pub frames_in: u64,
    /// Times the resampler's position jumped instead of slewing, in either direction,
    /// including the new lock after a map restart (the first lock is excluded).
    pub resyncs: u64,
    /// Output frames that were silence because the source's audio had not arrived (or
    /// never will: it is older than what the FIFO holds).
    pub late_frames: u64,
    /// Source frames thrown away: skipped by a resync, or pushed out of a full FIFO.
    pub discarded_frames: u64,
    /// Silent frames inserted where a run had a gap.
    pub gaps_filled: u64,
    /// Source frames dropped because a run overlapped audio already in the FIFO, or
    /// arrived after the position had passed their time (late audio).
    pub overlaps_dropped: u64,
    /// Failed reads.
    pub read_errors: u64,
    /// The last block's rate correction in ppm, as two's complement bits of an `i64`.
    pub correction_ppm: i64,
    /// The stream's presentation latency added to the sync offset since the last
    /// start, in ns (0 when the stream reports none; ADR-0042).
    pub presentation_latency_ns: u64,
}

impl InputStats {
    /// The counters now.
    pub fn read(&self) -> InputCounters {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        InputCounters {
            frames_in: l(&self.frames_in),
            resyncs: l(&self.resyncs),
            late_frames: l(&self.late_frames),
            discarded_frames: l(&self.discarded_frames),
            gaps_filled: l(&self.gaps_filled),
            overlaps_dropped: l(&self.overlaps_dropped),
            read_errors: l(&self.read_errors),
            correction_ppm: l(&self.error_ppm) as i64,
            presentation_latency_ns: l(&self.presentation_ns),
        }
    }

    fn add(counter: &AtomicU64, n: u64) {
        if n > 0 {
            counter.fetch_add(n, Ordering::Relaxed);
        }
    }
}

/// The channel matrix from a source layout to the graph layout (spec "Channels").
pub(crate) fn matrix(
    from: ChannelLayout,
    to: ChannelLayout,
) -> [[f32; MAX_CHANNELS]; MAX_CHANNELS] {
    let mut m = [[0.0f32; MAX_CHANNELS]; MAX_CHANNELS];
    let (n_in, n_out) = (from.channels(), to.channels());
    const H: f32 = std::f32::consts::FRAC_1_SQRT_2;
    if from == to {
        for (i, row) in m.iter_mut().enumerate().take(n_out) {
            row[i] = 1.0;
        }
        return m;
    }
    match (from, to) {
        (ChannelLayout::Mono, _) => {
            // Mono feeds the front left and right channels; the centre stays silent so
            // a mono source is not louder in surround layouts than in stereo.
            m[0][0] = 1.0;
            if n_out >= 2 {
                m[1][0] = 1.0;
            }
        }
        (_, ChannelLayout::Mono) => {
            for x in m[0].iter_mut().take(n_in) {
                *x = 1.0 / n_in as f32;
            }
        }
        (
            ChannelLayout::Surround3_1 | ChannelLayout::Surround5_1 | ChannelLayout::Surround7_1,
            ChannelLayout::Stereo,
        ) => {
            // L, R, C, LFE, Ls, Rs, Lb, Rb: centre and surrounds at -3 dB, LFE dropped,
            // normalised so full scale on every channel stays at full scale.
            let (mut l, mut r) = ([0.0f32; MAX_CHANNELS], [0.0f32; MAX_CHANNELS]);
            l[0] = 1.0;
            r[1] = 1.0;
            l[2] = H;
            r[2] = H;
            for (left, right) in [(4, 5), (6, 7)] {
                if right < n_in {
                    l[left] = H;
                    r[right] = H;
                }
            }
            let norm: f32 = l.iter().sum();
            for c in 0..MAX_CHANNELS {
                m[0][c] = l[c] / norm;
                m[1][c] = r[c] / norm;
            }
        }
        _ => {
            for (i, row) in m.iter_mut().enumerate().take(n_in.min(n_out)) {
                row[i] = 1.0;
            }
        }
    }
    m
}

/// Everything `prepare` allocates.
struct State {
    src_channels: usize,
    /// Frames per channel the FIFO holds.
    cap: usize,
    /// Planar FIFO: channel `c` is `fifo[c * cap..(c + 1) * cap]`; frames `0..len` are
    /// valid, frame 0 the oldest.
    fifo: Box<[f32]>,
    len: usize,
    /// Time map: master ns of the anchor, and the time of FIFO frame 0 relative to it.
    base: u64,
    head: f64,
    /// Master ns per source frame (nominal).
    dt: f64,
    /// The map has an anchor (some audio arrived since `prepare` or a restart).
    mapped: bool,
    /// Sum and count of in-tolerance timestamp errors this block (ns).
    map_err: f64,
    map_n: u32,
    /// Resampler position in FIFO frames, and whether it is locked to the wanted time.
    p: f64,
    locked: bool,
    /// The position ran past the FIFO and `head` was moved to it: the next run may be
    /// older than the map's end, and is then dropped up to the head, not mapped.
    ahead: bool,
    /// The map restarted while locked: the next lock is counted as a resync.
    relock: bool,
    /// Source frames per graph frame, nominal.
    nominal: f64,
    kernel: Kernel,
    matrix: [[f32; MAX_CHANNELS]; MAX_CHANNELS],
}

impl State {
    fn new(src: AudioFormat, config: &AudioGraphConfig, max_offset: Duration) -> Self {
        let rate = f64::from(src.sample_rate.hz());
        let span = config.latency + max_offset + MAX_PACKET + config.period() * 2;
        let cap = (span.as_secs_f64() * rate).ceil() as usize + 2 * TAPS + 1024;
        let src_channels = src.channels.channels().clamp(1, MAX_CHANNELS);
        Self {
            src_channels,
            cap,
            fifo: vec![0.0; cap * src_channels].into_boxed_slice(),
            len: 0,
            base: 0,
            head: 0.0,
            dt: 1e9 / rate,
            mapped: false,
            map_err: 0.0,
            map_n: 0,
            p: 0.0,
            locked: false,
            ahead: false,
            relock: false,
            nominal: rate / f64::from(config.format.sample_rate.hz()),
            kernel: Kernel::new(src.sample_rate, config.format.sample_rate),
            matrix: matrix(src.channels, config.format.channels),
        }
    }

    /// Moves FIFO frames `from..from + n` to `to` in every channel.
    fn shift(&mut self, from: usize, n: usize, to: usize) {
        if n == 0 || from == to {
            return;
        }
        for c in 0..self.src_channels {
            let base = c * self.cap;
            self.fifo
                .copy_within(base + from..base + from + n, base + to);
        }
    }

    /// Drops the oldest `n` frames (`n <= len`), keeping the map and position.
    fn drop_front(&mut self, n: usize) {
        let n = n.min(self.len);
        if n == 0 {
            return;
        }
        self.shift(n, self.len - n, 0);
        self.len -= n;
        self.head += n as f64 * self.dt;
        self.p -= n as f64;
    }

    /// Master time of `t` relative to the anchor, in ns.
    fn rel(&self, t: Timestamp) -> f64 {
        (i128::from(t.as_nanos()) - i128::from(self.base)) as f64
    }

    /// Places a run of `n` frames just read at FIFO frames `len..len + n`, with master
    /// time `ts` of its first frame.
    fn place(&mut self, n: usize, ts: Timestamp, discontinuity: bool, stats: &InputStats) {
        if !self.mapped {
            self.shift(self.len, n, 0);
            self.len = n;
            self.base = ts.as_nanos();
            self.head = 0.0;
            self.mapped = true;
            self.locked = false;
            self.map_err = 0.0;
            self.map_n = 0;
            return;
        }
        let expected = self.head + self.len as f64 * self.dt;
        let diff = self.rel(ts) - expected;
        let tol = MAP_TOLERANCE.as_nanos() as f64;
        let ahead = std::mem::replace(&mut self.ahead, false);
        // In tolerance: continuous, and the error steers the map. Not for a flagged
        // discontinuity a frame or more off (placed by its timestamp), nor for a run
        // behind a head the position moved (late audio: what the position passed is
        // dropped below, not mapped).
        let behind = ahead && diff <= -0.5 * self.dt;
        if diff.abs() <= tol && !behind && (!discontinuity || diff.abs() < self.dt) {
            self.map_err += diff;
            self.map_n += 1;
            self.len += n;
            return;
        }
        if diff > 0.0 {
            let gap = (diff / self.dt).round() as usize;
            if self.len + gap + n <= self.cap {
                self.shift(self.len, n, self.len + gap);
                for c in 0..self.src_channels {
                    let at = c * self.cap + self.len;
                    self.fifo[at..at + gap].fill(0.0);
                }
                self.len += gap + n;
                InputStats::add(&stats.gaps_filled, gap as u64);
                return;
            }
        } else if -diff <= RESTART_OVERLAP.as_nanos() as f64 {
            // Audio older than the map's end (late audio the position already passed,
            // or a run that repeats some): keep only what is new.
            let overlap = (-diff / self.dt).round() as usize;
            let keep = n.saturating_sub(overlap);
            self.shift(self.len + n - keep, keep, self.len);
            self.len += keep;
            InputStats::add(&stats.overlaps_dropped, (n - keep) as u64);
            if keep == 0 {
                // Wholly behind the head: the next run may be too, so stay on guard.
                self.ahead = ahead;
            }
            return;
        }
        // Too far off to bridge: start the map again at this run.
        InputStats::add(&stats.discarded_frames, self.len as u64);
        self.shift(self.len, n, 0);
        self.len = n;
        self.base = ts.as_nanos();
        self.head = 0.0;
        self.relock |= self.locked;
        self.locked = false;
        self.map_err = 0.0;
        self.map_n = 0;
    }
}

/// One audio input in the graph: reads its stream, aligns it to the block's master
/// time with the sync offset, corrects drift and converts to the graph format.
pub struct InputNode {
    desc: NodeDescriptor,
    stream: Box<dyn AudioInputStream>,
    config: AudioGraphConfig,
    max_offset: Duration,
    params: ParamReceiver<InputParams>,
    /// The stream's presentation latency in ns, read once after each `start` and added
    /// to the sync offset (ADR-0042).
    presentation_ns: i64,
    stats: Arc<InputStats>,
    state: Option<State>,
}

/// The descriptor of input nodes.
pub fn input_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new(INPUT_NODE_TYPE),
        NodeKind::Source,
        vec![],
        vec![Pin::one("out", LinkKind::Audio)],
    )
}

impl InputNode {
    /// A node for `stream` (opened, Standby) in a graph of `config`. `max_offset` is
    /// the largest delay the sync offset may ask for; the FIFO is sized for it.
    pub fn new(
        stream: Box<dyn AudioInputStream>,
        config: AudioGraphConfig,
        max_offset: Duration,
    ) -> (Self, ParamSender<InputParams>, Arc<InputStats>) {
        let (tx, rx) = channel(InputParams::NONE);
        let stats = Arc::new(InputStats::default());
        (
            Self {
                desc: input_descriptor(),
                stream,
                config,
                max_offset,
                params: rx,
                presentation_ns: 0,
                stats: Arc::clone(&stats),
                state: None,
            },
            tx,
            stats,
        )
    }

    /// The offset in ns (the sync offset plus the stream's presentation latency),
    /// clamped to what the FIFO and the latency allow.
    fn offset(&self) -> f64 {
        let max = i64::try_from(self.max_offset.as_nanos()).unwrap_or(i64::MAX);
        let advance = self
            .config
            .latency
            .saturating_sub(self.config.period() + ADVANCE_MARGIN);
        let min = -i64::try_from(advance.as_nanos()).unwrap_or(i64::MAX);
        self.params
            .get()
            .offset_ns
            .saturating_add(self.presentation_ns)
            .clamp(min, max) as f64
    }
}

fn start_error(e: AudioIoError) -> NodeError {
    match e {
        AudioIoError::EndpointLost
        | AudioIoError::EndpointNotFound
        | AudioIoError::PermissionDenied
        | AudioIoError::FormatChanged
        | AudioIoError::FormatUnsupported => NodeError::Permanent("audio input cannot start"),
        other => NodeError::Failed(format!("audio input start failed: {other}")),
    }
}

impl<X: Runtime> Node<X> for InputNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        self.state = Some(State::new(
            self.stream.format(),
            &self.config,
            self.max_offset,
        ));
        self.stream.start().map_err(start_error)?;
        // Loopback audio is stamped when the OS mixes it; delay it by the time the
        // device takes to play it, so it lines up with what was on screen (ADR-0042).
        let latency = self.stream.presentation_latency().unwrap_or(Duration::ZERO);
        self.presentation_ns = i64::try_from(latency.as_nanos()).unwrap_or(i64::MAX);
        self.stats.presentation_ns.store(
            latency.as_nanos().try_into().unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        Ok(())
    }

    fn release(&mut self, _: &mut PrepareCtx<'_, X>) {
        self.stream.stop();
        self.state = None;
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        self.params.update();
        let offset = self.offset();
        let Some(st) = self.state.as_mut() else {
            return Err(NodeError::NotReady);
        };
        let stats = &*self.stats;

        // 1. Everything the stream has, into the FIFO.
        st.map_err = 0.0;
        st.map_n = 0;
        loop {
            const MIN_ROOM: usize = 256;
            if st.cap - st.len < MIN_ROOM {
                let n = st.len.min(st.cap / 4);
                InputStats::add(&stats.discarded_frames, n as u64);
                st.drop_front(n);
            }
            let (len, cap, n_ch) = (st.len, st.cap, st.src_channels);
            let mut views: [&mut [f32]; MAX_CHANNELS] = Default::default();
            for (view, chunk) in views.iter_mut().zip(st.fifo.chunks_exact_mut(cap)) {
                *view = &mut chunk[len..];
            }
            let read = match self.stream.read(&mut views[..n_ch]) {
                Ok(r) => r,
                Err(e) => {
                    let e = match e {
                        AudioIoError::EndpointLost => {
                            NodeError::Permanent("audio input endpoint lost")
                        }
                        AudioIoError::FormatChanged => {
                            NodeError::Permanent("audio input format changed")
                        }
                        AudioIoError::PermissionDenied => {
                            NodeError::Permanent("audio input permission denied")
                        }
                        _ => NodeError::Backend("audio input read failed"),
                    };
                    stats.read_errors.fetch_add(1, Ordering::Relaxed);
                    return Err(e);
                }
            };
            if read.frames == 0 {
                break;
            }
            InputStats::add(&stats.frames_in, read.frames as u64);
            st.place(read.frames, read.timestamp, read.discontinuity, stats);
        }
        if st.map_n > 0 {
            st.head += MAP_GAIN * st.map_err / f64::from(st.map_n);
        }

        // 2. The block's time; ticks before the latency produce nothing.
        let Some(block_time) = self.config.block_time(ctx.tick()) else {
            return Ok(());
        };
        let frames = self.config.block_frames as usize;
        let format = self.config.format;
        let n_out = format.channels.channels();
        let mut out = ctx.outputs().audio(0)?;
        let mut chans: [&mut [f32]; MAX_CHANNELS] = Default::default();
        for (slot, ch) in chans.iter_mut().zip(out.channels_mut(n_out)) {
            *slot = ch;
        }
        if !st.mapped {
            for ch in chans.iter_mut().take(n_out) {
                ch[..frames].fill(0.0);
            }
            InputStats::add(&stats.late_frames, frames as u64);
            out.commit(format, block_time, frames)?;
            return Ok(());
        }

        // 3. Steering: where the resampler is against where the block wants it.
        let want = st.rel(block_time) - offset;
        let wanted_p = (want - st.head) / st.dt;
        if !st.locked {
            st.p = wanted_p;
            st.locked = true;
            if std::mem::replace(&mut st.relock, false) {
                stats.resyncs.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            let err = st.head + st.p * st.dt - want;
            if err.abs() > RESYNC_THRESHOLD.as_nanos() as f64 {
                if wanted_p > st.p {
                    let skipped = (wanted_p.floor() - st.p.floor()).max(0.0) as u64;
                    InputStats::add(&stats.discarded_frames, skipped);
                }
                st.p = wanted_p;
                stats.resyncs.fetch_add(1, Ordering::Relaxed);
            }
        }
        let err_s = (st.head + st.p * st.dt - want) * 1e-9;
        let correction = (err_s * STEER_GAIN).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        stats
            .error_ppm
            .store((correction * 1e6).round() as i64 as u64, Ordering::Relaxed);
        let step = st.nominal * (1.0 - correction);

        // 4. Resample and map channels.
        let mut weights = [0.0f32; TAPS];
        let mut late = 0u64;
        for f in 0..frames {
            let i = st.p.floor();
            let ok = i >= BEFORE as f64 && (i as usize) + AFTER < st.len;
            if ok {
                let i = i as usize;
                st.kernel.coefficients(st.p - i as f64, &mut weights);
                let mut v = [0.0f32; MAX_CHANNELS];
                for (c, v) in v.iter_mut().enumerate().take(st.src_channels) {
                    let at = c * st.cap + i - BEFORE;
                    *v = apply(&weights, &st.fifo[at..at + TAPS]);
                }
                for (o, ch) in chans.iter_mut().enumerate().take(n_out) {
                    let row = &st.matrix[o];
                    let mut acc = 0.0;
                    for c in 0..st.src_channels {
                        acc += row[c] * v[c];
                    }
                    ch[f] = acc;
                }
            } else {
                for ch in chans.iter_mut().take(n_out) {
                    ch[f] = 0.0;
                }
                late += 1;
            }
            st.p += step;
        }
        InputStats::add(&stats.late_frames, late);

        // 5. Forget what no later output frame can reach.
        let keep_from = st.p.floor() - BEFORE as f64;
        if keep_from > 0.0 {
            let n = (keep_from as usize).min(st.len);
            st.drop_front(n);
            if (keep_from as usize) > n {
                // The position ran past the FIFO (late audio): keep the map and move the
                // position with the time it stands for.
                let extra = keep_from as usize - n;
                st.head += extra as f64 * st.dt;
                st.p -= extra as f64;
                st.ahead = true;
            }
        }
        out.commit(format, block_time, frames)?;
        Ok(())
    }
}
