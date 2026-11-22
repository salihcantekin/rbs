//! The mixer: per-strip gain and mute with ramps, post-fader meters (ADR-0037 decision
//! 7, audio graph spec "Mixer"), the monitor mix (ADR-0097 decisions 5 and 6) and the
//! bus outputs (ADR-0097 decisions 6 and 10, ADR-0170 decisions 1 and 2).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_core::media::MAX_CHANNELS;
use rbs_core::time::Timestamp;
use rbs_graph::description::NodeId;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::{ParamReceiver, ParamSender, channel};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

use crate::config::AudioGraphConfig;
use crate::meter::LevelMeter;

/// Node type id of [`MixerNode`].
pub const MIXER_NODE_TYPE: &str = "org.rbs.audio.mixer";

/// How long a gain change takes by default (one 10 ms block).
pub const DEFAULT_RAMP: Duration = Duration::from_millis(10);

/// Bus outputs besides `main` (ADR-0170 decision 1: six buses with `main`). Output pin
/// `2 + k` is bus slot `k`; the binder maps a collection's buses to slots.
pub const MAX_BUS_OUTPUTS: usize = 5;

/// Names of the bus output pins, by slot.
pub const BUS_PINS: [&str; MAX_BUS_OUTPUTS] = ["bus1", "bus2", "bus3", "bus4", "bus5"];

/// Output pin of the first bus slot (after `main` and `monitor`).
const FIRST_BUS_PIN: usize = 2;

/// Whether a strip is heard on the monitor (ADR-0097 decision 5).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitorMode {
    /// Not on the monitor.
    #[default]
    Off,
    /// On the monitor and off the mix, while the monitor output is read; otherwise
    /// the same as `Off`.
    Only,
    /// On the monitor and on the mix.
    Also,
}

/// One input's settings, keyed by the node that produces it.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct Strip {
    /// The producing node (an input node or filter in the same partition).
    pub producer: NodeId,
    /// Linear amplitude gain; negative and non-finite values count as 0.
    pub gain: f32,
    /// Muted strips have gain 0.
    pub muted: bool,
    /// Post-fader meter.
    pub meter: Option<Arc<LevelMeter>>,
    /// Monitor mode.
    pub monitor: MonitorMode,
    /// Soloed: while any strip is, the monitor plays only soloed strips (the mix is
    /// unchanged).
    pub solo: bool,
    /// A fader ramp of its own (ADR-0128 decision 10): start and length on the engine
    /// clock, so a scene transition's sound follows its picture. `None` uses the
    /// mixer's default ramp. Also applies when the input joins.
    pub ramp_override: Option<(Timestamp, Duration)>,
    /// Send level to `main`, post-fader (ADR-0097 decision 9); default 1.
    pub main_send: f32,
    /// Send level to each bus slot, post-fader; default 0 (no send).
    pub sends: [f32; MAX_BUS_OUTPUTS],
}

impl Strip {
    /// A strip at `gain`, not muted, without a meter.
    pub fn new(producer: NodeId, gain: f32) -> Self {
        Self {
            producer,
            gain,
            muted: false,
            meter: None,
            monitor: MonitorMode::Off,
            solo: false,
            ramp_override: None,
            main_send: 1.0,
            sends: [0.0; MAX_BUS_OUTPUTS],
        }
    }

    /// Sets the send level to `main`.
    pub fn with_main_send(mut self, level: f32) -> Self {
        self.main_send = level;
        self
    }

    /// Sets the send level to bus slot `slot`; a slot past [`MAX_BUS_OUTPUTS`] is
    /// ignored.
    pub fn with_send(mut self, slot: usize, level: f32) -> Self {
        if let Some(s) = self.sends.get_mut(slot) {
            *s = level;
        }
        self
    }

    /// Mutes or unmutes.
    pub fn with_muted(mut self, muted: bool) -> Self {
        self.muted = muted;
        self
    }

    /// Attaches a meter.
    pub fn with_meter(mut self, meter: Arc<LevelMeter>) -> Self {
        self.meter = Some(meter);
        self
    }

    /// Sets the monitor mode.
    pub fn with_monitor(mut self, monitor: MonitorMode) -> Self {
        self.monitor = monitor;
        self
    }

    /// Solos or unsolos.
    pub fn with_solo(mut self, solo: bool) -> Self {
        self.solo = solo;
        self
    }

    /// Gives the fader its own ramp: from `start` over `length` (ADR-0128 decision 10).
    pub fn with_ramp_override(mut self, ramp: Option<(Timestamp, Duration)>) -> Self {
        self.ramp_override = ramp;
        self
    }

    fn target(&self) -> f32 {
        if self.muted { 0.0 } else { sanitize(self.gain) }
    }
}

fn sanitize(g: f32) -> f32 {
    if g.is_finite() && g > 0.0 { g } else { 0.0 }
}

/// One bus output's fader and meter (ADR-0170): applied after the bus's sum, like the
/// master on `main`.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct BusOutput {
    /// Linear gain (the bus's volume, 0 while muted).
    pub gain: f32,
    /// Meter of the bus output.
    pub meter: Option<Arc<LevelMeter>>,
}

impl Default for BusOutput {
    fn default() -> Self {
        Self {
            gain: 1.0,
            meter: None,
        }
    }
}

/// The mixer's parameters (ADR-0023 channel).
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct MixerParams {
    /// Strips; inputs whose producer has none are not heard.
    pub strips: Vec<Strip>,
    /// Master gain (linear).
    pub master: f32,
    /// Meter of the output.
    pub master_meter: Option<Arc<LevelMeter>>,
    /// How long gain and mute changes, and inputs joining, take to reach their level.
    pub ramp: Duration,
    /// The bus outputs' faders and meters, by slot.
    pub buses: [BusOutput; MAX_BUS_OUTPUTS],
}

impl Default for MixerParams {
    fn default() -> Self {
        Self {
            strips: Vec::new(),
            master: 1.0,
            master_meter: None,
            ramp: DEFAULT_RAMP,
            buses: Default::default(),
        }
    }
}

impl MixerParams {
    /// Parameters with these strips, unity master gain and the default ramp.
    pub fn new(strips: Vec<Strip>) -> Self {
        Self {
            strips,
            ..Self::default()
        }
    }

    /// Sets the master gain.
    pub fn with_master(mut self, gain: f32) -> Self {
        self.master = gain;
        self
    }

    /// Attaches the output meter.
    pub fn with_master_meter(mut self, meter: Arc<LevelMeter>) -> Self {
        self.master_meter = Some(meter);
        self
    }

    /// Sets the ramp time.
    pub fn with_ramp(mut self, ramp: Duration) -> Self {
        self.ramp = ramp;
        self
    }

    /// Sets bus slot `slot`'s gain and meter; a slot past [`MAX_BUS_OUTPUTS`] is
    /// ignored.
    pub fn with_bus(mut self, slot: usize, gain: f32, meter: Option<Arc<LevelMeter>>) -> Self {
        if let Some(b) = self.buses.get_mut(slot) {
            *b = BusOutput { gain, meter };
        }
        self
    }
}

/// Counters of one mixer.
#[derive(Debug, Default)]
pub struct MixerStats {
    invalid_inputs: AtomicU64,
    unknown_inputs: AtomicU64,
}

impl MixerStats {
    /// Input blocks skipped because their format or length was not the graph's.
    pub fn invalid_inputs(&self) -> u64 {
        self.invalid_inputs.load(Ordering::Relaxed)
    }

    /// Input blocks not heard because their producer had no strip.
    pub fn unknown_inputs(&self) -> u64 {
        self.unknown_inputs.load(Ordering::Relaxed)
    }
}

/// A gain moving linearly from `from` to `to` over `[start, start + length]`.
#[derive(Debug, Clone, Copy)]
struct Ramp {
    from: f32,
    to: f32,
    start: Timestamp,
    length: Duration,
}

impl Ramp {
    const fn steady(g: f32) -> Self {
        Self {
            from: g,
            to: g,
            start: Timestamp::ZERO,
            length: Duration::ZERO,
        }
    }

    fn at(&self, t: Timestamp) -> f32 {
        if t <= self.start {
            return self.from;
        }
        let since = t.saturating_since(self.start);
        if since >= self.length || self.length.is_zero() {
            return self.to;
        }
        let x = since.as_secs_f64() / self.length.as_secs_f64();
        self.from + (self.to - self.from) * x as f32
    }

    /// A ramp to `to` that ends at `start + length` (ADR-0128 decision 10): it holds
    /// where this one is at `t` until `start`, then moves; a start in the past leaves
    /// only the rest of the length. This one, unchanged, when it already heads for `to`.
    fn over(&self, to: f32, t: Timestamp, start: Timestamp, length: Duration) -> Self {
        if to == self.to {
            return *self;
        }
        let from = self.at(t);
        if t < start {
            Self {
                from,
                to,
                start,
                length,
            }
        } else {
            Self {
                from,
                to,
                start: t,
                length: (start + length).saturating_since(t),
            }
        }
    }

    /// A ramp to `to` starting where this one is at `t`; this one, unchanged, when it
    /// already heads for `to` (so taking the same target again does not restart it).
    fn towards(&self, to: f32, t: Timestamp, length: Duration) -> Self {
        if to == self.to {
            return *self;
        }
        Self {
            from: self.at(t),
            to,
            start: t,
            length,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct StripState {
    producer: NodeId,
    /// Fader: gain and mute.
    ramp: Ramp,
    /// Send to `main`: its send level, 0 for `Only` while the monitor is read.
    send: Ramp,
    /// Send to each bus slot, likewise.
    bus: [Ramp; MAX_BUS_OUTPUTS],
    /// Send to the monitor, 0 or 1.
    monitor: Ramp,
    /// The strip's own ramp, also used when the input joins (ADR-0128).
    join: Option<(Timestamp, Duration)>,
    /// The input had a block last tick (a returning input fades in).
    present: bool,
    /// The input has a block this tick.
    seen: bool,
    /// The strip's index in the params, where its meter is (no search per block).
    index: usize,
}

/// Sums its inputs with per-strip gain and mute and a master gain.
pub struct MixerNode {
    desc: NodeDescriptor,
    config: AudioGraphConfig,
    params: ParamReceiver<MixerParams>,
    capacity: usize,
    /// One per honoured strip, parallel to the first `capacity` of `params.strips`.
    strips: Vec<StripState>,
    /// Producer to strip index, sorted by producer. Filled in `apply_params`.
    by_producer: Vec<(NodeId, usize)>,
    scratch: Vec<StripState>,
    master: Ramp,
    /// The parameters have not been applied yet.
    fresh: bool,
    /// The monitor output was read in the last block.
    monitoring: bool,
    /// Planar monitor mix, `MAX_CHANNELS` channels of `block_frames`; allocated in
    /// `new` so linking the monitor output allocates nothing.
    monitor_mix: Box<[f32]>,
    /// Bus faders, by slot.
    bus_masters: [Ramp; MAX_BUS_OUTPUTS],
    /// Planar bus mixes: slot, then the graph's channels, then `block_frames`;
    /// allocated in `new` so linking a bus output allocates nothing.
    bus_mix: Box<[f32]>,
    stats: Arc<MixerStats>,
}

/// The descriptor of mixer nodes.
pub fn mixer_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new(MIXER_NODE_TYPE),
        NodeKind::AudioMixer,
        vec![Pin::many("in", LinkKind::Audio)],
        vec![
            Pin::one("main", LinkKind::Audio),
            Pin::one("monitor", LinkKind::Audio),
            Pin::one(BUS_PINS[0], LinkKind::Audio),
            Pin::one(BUS_PINS[1], LinkKind::Audio),
            Pin::one(BUS_PINS[2], LinkKind::Audio),
            Pin::one(BUS_PINS[3], LinkKind::Audio),
            Pin::one(BUS_PINS[4], LinkKind::Audio),
        ],
    )
}

impl MixerNode {
    /// A mixer for a graph of `config` honouring up to `strips` strips, starting with
    /// `params`.
    pub fn new(
        config: AudioGraphConfig,
        strips: usize,
        params: MixerParams,
    ) -> (Self, ParamSender<MixerParams>, Arc<MixerStats>) {
        let (tx, rx) = channel(params);
        let stats = Arc::new(MixerStats::default());
        (
            Self {
                desc: mixer_descriptor(),
                config,
                params: rx,
                capacity: strips,
                strips: Vec::with_capacity(strips),
                by_producer: Vec::with_capacity(strips),
                scratch: Vec::with_capacity(strips),
                master: Ramp::steady(1.0),
                fresh: true,
                monitoring: false,
                monitor_mix: vec![0.0; config.block_frames as usize * MAX_CHANNELS]
                    .into_boxed_slice(),
                bus_masters: [Ramp::steady(1.0); MAX_BUS_OUTPUTS],
                bus_mix: vec![
                    0.0;
                    MAX_BUS_OUTPUTS
                        * config.format.channels.channels()
                        * config.block_frames as usize
                ]
                .into_boxed_slice(),
                stats: Arc::clone(&stats),
            },
            tx,
            stats,
        )
    }

    /// Takes new parameters (or a change of whether the monitor is read) into the
    /// ramp state, without allocating (the strip vectors and the producer table
    /// hold `capacity`).
    fn apply_params(&mut self, t: Timestamp) {
        let p = self.params.get();
        let ramp = p.ramp;
        let monitoring = self.monitoring;
        let solo = p.strips.iter().take(self.capacity).any(|s| s.solo);
        self.scratch.clear();
        for (index, strip) in p.strips.iter().take(self.capacity).enumerate() {
            let target = strip.target();
            // `Only` leaves every mix while the monitor is read (ADR-0097 decision 5).
            let on_mix = if monitoring && strip.monitor == MonitorMode::Only {
                0.0
            } else {
                1.0
            };
            let send = sanitize(strip.main_send) * on_mix;
            let bus = strip.sends.map(|l| sanitize(l) * on_mix);
            let heard = if solo {
                strip.solo
            } else {
                strip.monitor != MonitorMode::Off
            };
            let monitor = if monitoring && heard { 1.0 } else { 0.0 };
            let old = self.strips.iter().find(|s| s.producer == strip.producer);
            let state = match old {
                Some(old) if !self.fresh => StripState {
                    producer: strip.producer,
                    ramp: match strip.ramp_override {
                        Some((start, length)) => old.ramp.over(target, t, start, length),
                        None => old.ramp.towards(target, t, ramp),
                    },
                    join: strip.ramp_override,
                    send: old.send.towards(send, t, ramp),
                    bus: std::array::from_fn(|k| old.bus[k].towards(bus[k], t, ramp)),
                    monitor: old.monitor.towards(monitor, t, ramp),
                    present: old.present,
                    seen: false,
                    index,
                },
                Some(old) => StripState {
                    producer: strip.producer,
                    ramp: Ramp::steady(target),
                    join: strip.ramp_override,
                    send: Ramp::steady(send),
                    bus: bus.map(Ramp::steady),
                    monitor: Ramp::steady(monitor),
                    present: old.present,
                    seen: false,
                    index,
                },
                None => StripState {
                    producer: strip.producer,
                    ramp: Ramp::steady(target),
                    join: strip.ramp_override,
                    send: Ramp::steady(send),
                    bus: bus.map(Ramp::steady),
                    monitor: Ramp::steady(monitor),
                    present: false,
                    seen: false,
                    index,
                },
            };
            self.scratch.push(state);
        }
        std::mem::swap(&mut self.strips, &mut self.scratch);
        self.by_producer.clear();
        for (index, strip) in self.strips.iter().enumerate() {
            self.by_producer.push((strip.producer, index));
        }
        self.by_producer
            .sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let master = sanitize(p.master);
        self.master = if self.fresh {
            Ramp::steady(master)
        } else {
            self.master.towards(master, t, ramp)
        };
        for (state, bus) in self.bus_masters.iter_mut().zip(&p.buses) {
            let gain = sanitize(bus.gain);
            *state = if self.fresh {
                Ramp::steady(gain)
            } else {
                state.towards(gain, t, ramp)
            };
        }
        self.fresh = false;
    }

    /// The strip for `producer`, from the table `apply_params` sorted.
    /// A field borrow, so the caller can still write the mix buffers.
    fn strip_mut<'s>(
        strips: &'s mut [StripState],
        by_producer: &[(NodeId, usize)],
        producer: NodeId,
    ) -> Option<&'s mut StripState> {
        let at = by_producer.partition_point(|entry| entry.0 < producer);
        let index = by_producer
            .get(at)
            .filter(|entry| entry.0 == producer)
            .map(|entry| entry.1)?;
        strips.get_mut(index)
    }
}

impl<X: Runtime> Node<X> for MixerNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    fn release(&mut self, _: &mut PrepareCtx<'_, X>) {
        // A restart starts at the target levels again.
        self.fresh = true;
        for s in &mut self.strips {
            s.present = false;
        }
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let Some(t0) = self.config.block_time(ctx.tick()) else {
            return Ok(());
        };
        let period = self.config.period();
        let t1 = t0 + period;
        let monitoring = ctx.outputs().is_linked(1);
        // Bus slots whose output is read: only those are summed (ADR-0170 decision 1).
        let mut buses = 0u8;
        for k in 0..MAX_BUS_OUTPUTS {
            if ctx.outputs().is_linked(FIRST_BUS_PIN + k) {
                buses |= 1 << k;
            }
        }
        let flipped = monitoring != self.monitoring;
        self.monitoring = monitoring;
        if self.params.update() || self.fresh || flipped {
            self.apply_params(t0);
        }
        let format = self.config.format;
        let frames = self.config.block_frames as usize;
        let n = format.channels.channels();
        let ramp = self.params.get().ramp;
        let inputs = ctx.inputs();
        let mut out = ctx.outputs().audio(0)?;
        let mut chans: [&mut [f32]; MAX_CHANNELS] = Default::default();
        for (slot, ch) in chans.iter_mut().zip(out.channels_mut(n)) {
            *slot = ch;
            slot[..frames].fill(0.0);
        }
        let inv = 1.0 / frames as f32;
        if monitoring {
            self.monitor_mix[..frames * n].fill(0.0);
        }
        let bus_len = n * frames;
        if buses != 0 {
            for k in bits(buses) {
                self.bus_mix[k * bus_len..(k + 1) * bus_len].fill(0.0);
            }
        }

        for s in &mut self.strips {
            s.seen = false;
        }
        for (link, block) in inputs.audios(0).enumerate() {
            let Some(block) = block else { continue };
            let Some(producer) = inputs.source(0, link) else {
                continue;
            };
            let Some(strip) = Self::strip_mut(&mut self.strips, &self.by_producer, producer) else {
                self.stats.unknown_inputs.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            if block.format() != format || block.frames() != frames {
                self.stats.invalid_inputs.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if !strip.present {
                // An input that (re)joins fades in from silence: over the strip's own
                // ramp when it has one (a transition, ADR-0128), else the default.
                let (start, length) = match strip.join {
                    Some((start, length)) if t0 < start => (start, length),
                    Some((start, length)) => (t0, (start + length).saturating_since(t0).max(ramp)),
                    None => (t0, ramp),
                };
                strip.ramp = Ramp {
                    from: 0.0,
                    to: strip.ramp.to,
                    start,
                    length,
                };
            }
            strip.seen = true;
            let (g0, g1) = (strip.ramp.at(t0), strip.ramp.at(t1));
            let (s0, s1) = (strip.send.at(t0), strip.send.at(t1));
            let (m0, m1) = if monitoring {
                (strip.monitor.at(t0), strip.monitor.at(t1))
            } else {
                (0.0, 0.0)
            };
            // The common case (every send at 1, nothing to the monitor) is the plain
            // sum; anything else takes the general loop.
            let plain = s0 == 1.0 && s1 == 1.0 && m0 == 0.0 && m1 == 0.0;
            let meter_index = strip.index;
            let meter = self
                .params
                .get()
                .strips
                .get(meter_index)
                .and_then(|s| s.meter.as_deref());
            let gains = StripGains {
                g0,
                dg: (g1 - g0) * inv,
                s0,
                ds: (s1 - s0) * inv,
                m0,
                dm: (m1 - m0) * inv,
                plain,
                silent: plain && g0 == 0.0 && g1 == 0.0,
                steady: g0 == g1,
                off_monitor: m0 == 0.0 && m1 == 0.0,
            };
            for (c, ch) in chans.iter_mut().enumerate().take(n) {
                let Some(src) = block.channel(c) else {
                    continue;
                };
                let out = &mut ch[..frames];
                // The `Option` is the only test: with no meter the add-only loops run
                // and nothing is recorded (F-15a).
                if let Some(m) = meter {
                    let (peak, sum_sq) = gains.mix::<true>(out, src, &mut self.monitor_mix, c);
                    m.record_block(c, peak, sum_sq, frames as u32);
                } else {
                    gains.mix::<false>(out, src, &mut self.monitor_mix, c);
                }
            }
            if buses != 0 && !(g0 == 0.0 && g1 == 0.0) {
                // Post-fader sends to the read buses: one pass per bus with a send.
                let dg = gains.dg;
                for k in bits(buses) {
                    let (b0, b1) = (strip.bus[k].at(t0), strip.bus[k].at(t1));
                    if b0 == 0.0 && b1 == 0.0 {
                        continue;
                    }
                    let db = (b1 - b0) * inv;
                    let mix = &mut self.bus_mix[k * bus_len..(k + 1) * bus_len];
                    for (c, dst) in mix.chunks_exact_mut(frames).enumerate() {
                        let Some(src) = block.channel(c) else {
                            continue;
                        };
                        if gains.steady && b0 == b1 {
                            // Settled fader and send: one constant factor.
                            let f = g0 * b0;
                            mix_lanes::<false>(dst, src, |_| f, |_| 1.0);
                        } else {
                            mix_lanes::<false>(dst, src, |w| g0 + dg * w, |w| b0 + db * w);
                        }
                    }
                }
            }
        }
        for s in &mut self.strips {
            s.present = s.seen;
        }

        let (m0, m1) = (self.master.at(t0), self.master.at(t1));
        let dm = (m1 - m0) * inv;
        let meter = self.params.get().master_meter.as_deref();
        for (c, ch) in chans.iter_mut().enumerate().take(n) {
            if let Some(m) = meter {
                let (peak, sum_sq) = scale_lanes::<true>(&mut ch[..frames], |w| m0 + dm * w);
                m.record_block(c, peak, sum_sq, frames as u32);
            } else {
                scale_lanes::<false>(&mut ch[..frames], |w| m0 + dm * w);
            }
        }
        out.commit(format, t0, frames)?;
        if monitoring {
            // The monitor mix: post strip fader and mute, before the master.
            let mut mon = ctx.outputs().audio(1)?;
            for (c, ch) in mon.channels_mut(n).enumerate() {
                ch[..frames].copy_from_slice(&self.monitor_mix[c * frames..(c + 1) * frames]);
            }
            mon.commit(format, t0, frames)?;
        }
        for k in bits(buses) {
            // Each read bus after its own fader, like `main` after the master.
            let (a0, a1) = (self.bus_masters[k].at(t0), self.bus_masters[k].at(t1));
            let da = (a1 - a0) * inv;
            let meter = self.params.get().buses[k].meter.as_deref();
            let mix = &self.bus_mix[k * bus_len..(k + 1) * bus_len];
            let mut bus = ctx.outputs().audio(FIRST_BUS_PIN + k)?;
            for (c, (ch, sum)) in bus
                .channels_mut(n)
                .zip(mix.chunks_exact(frames))
                .enumerate()
            {
                // One pass: the bus fader applied while writing the pin.
                let out = &mut ch[..frames];
                if let Some(m) = meter {
                    let (peak, sum_sq) = scale_into_lanes::<true>(out, sum, |w| a0 + da * w);
                    m.record_block(c, peak, sum_sq, frames as u32);
                } else {
                    scale_into_lanes::<false>(out, sum, |w| a0 + da * w);
                }
            }
            bus.commit(format, t0, frames)?;
        }
        Ok(())
    }
}

/// The slots set in `mask`, lowest first: only the set bits are visited (ADR-0170
/// decision 1), never every slot.
fn bits(mask: u8) -> impl Iterator<Item = usize> {
    let mut m = mask;
    std::iter::from_fn(move || {
        if m == 0 {
            return None;
        }
        let k = m.trailing_zeros() as usize;
        m &= m - 1;
        Some(k)
    })
}

/// Accumulator lanes of the mix loops. The loops keep this many partial peaks and
/// sums, so the reductions vectorise (A-01 of the 2026-11 performance audit).
const LANES: usize = 8;

/// One strip's gains over a block: start values and steps per frame of the fader
/// ramp (`g`), the send to the main output (`s`) and the monitor send (`m`).
struct StripGains {
    g0: f32,
    dg: f32,
    s0: f32,
    ds: f32,
    m0: f32,
    dm: f32,
    /// Every send at 1 and nothing to the monitor.
    plain: bool,
    /// `plain`, and the fader is at 0 for the whole block.
    silent: bool,
    /// The fader does not move in this block.
    steady: bool,
    /// Nothing goes to the monitor in this block.
    off_monitor: bool,
}

impl StripGains {
    /// Adds one channel `src` of the strip to `out` (and to channel `c` of `monitor`
    /// when it is on the monitor) and returns the post-fader peak and sum of squares.
    /// Without `METER` the sums are not kept and `(0.0, 0.0)` comes back; the samples
    /// written are the same either way.
    #[inline(always)]
    fn mix<const METER: bool>(
        &self,
        out: &mut [f32],
        src: &[f32],
        monitor: &mut [f32],
        c: usize,
    ) -> (f32, f32) {
        let Self {
            g0,
            dg,
            s0,
            ds,
            m0,
            dm,
            ..
        } = *self;
        if self.silent {
            // Muted and settled: adds nothing, and its meter reads silence.
            (0.0, 0.0)
        } else if self.plain && self.steady {
            mix_lanes::<METER>(out, src, |_| g0, |_| 1.0)
        } else if self.plain {
            mix_lanes::<METER>(out, src, |w| g0 + dg * w, |_| 1.0)
        } else if self.off_monitor {
            // Off the monitor, with a send that is not 1 (`Only` ramping).
            mix_lanes::<METER>(out, src, |w| g0 + dg * w, |w| s0 + ds * w)
        } else {
            let frames = out.len();
            let mon = &mut monitor[c * frames..(c + 1) * frames];
            let mut peak = 0.0f32;
            let mut sum_sq = 0.0f32;
            for (f, ((o, mo), &x)) in out.iter_mut().zip(mon).zip(src).enumerate() {
                let w = (f + 1) as f32;
                let y = x * (g0 + dg * w);
                if METER {
                    peak = peak.max(y.abs());
                    sum_sq += y * y;
                }
                *o += y * (s0 + ds * w);
                *mo += y * (m0 + dm * w);
            }
            (peak, sum_sq)
        }
    }
}

/// Adds `src * gain(w) * send(w)` to `out` and returns the peak and the sum of
/// squares of `src * gain(w)`, where `w` is the frame number plus one as `f32` (the
/// ramp position at the frame's end). The samples written are exactly those of a
/// plain per-frame loop; only the meter sums add up in another order. Without
/// `METER` it only adds (the sums are compiled out) and returns `(0.0, 0.0)`.
#[inline(always)]
fn mix_lanes<const METER: bool>(
    out: &mut [f32],
    src: &[f32],
    gain: impl Fn(f32) -> f32,
    send: impl Fn(f32) -> f32,
) -> (f32, f32) {
    let mut peak = [0.0f32; LANES];
    let mut sum = [0.0f32; LANES];
    let mut out_chunks = out.chunks_exact_mut(LANES);
    let mut src_chunks = src.chunks_exact(LANES);
    let mut base = 0.0f32;
    for (o, x) in (&mut out_chunks).zip(&mut src_chunks) {
        for l in 0..LANES {
            // Frame numbers stay far below 2^24, so this is exact.
            let w = base + (l + 1) as f32;
            let y = x[l] * gain(w);
            if METER {
                let a = y.abs();
                peak[l] = if a > peak[l] { a } else { peak[l] };
                sum[l] += y * y;
            }
            o[l] += y * send(w);
        }
        base += LANES as f32;
    }
    let mut p = 0.0f32;
    let mut q = 0.0f32;
    for (o, &x) in out_chunks
        .into_remainder()
        .iter_mut()
        .zip(src_chunks.remainder())
    {
        base += 1.0;
        let y = x * gain(base);
        if METER {
            p = p.max(y.abs());
            q += y * y;
        }
        *o += y * send(base);
    }
    (fold_peak(peak, p), fold_sum(sum, q))
}

/// Scales `out` by `gain(w)` in place (as [`mix_lanes`]) and returns the peak and the
/// sum of squares of the result (`(0.0, 0.0)` without `METER`).
#[inline(always)]
fn scale_lanes<const METER: bool>(out: &mut [f32], gain: impl Fn(f32) -> f32) -> (f32, f32) {
    let mut peak = [0.0f32; LANES];
    let mut sum = [0.0f32; LANES];
    let mut chunks = out.chunks_exact_mut(LANES);
    let mut base = 0.0f32;
    for o in &mut chunks {
        for l in 0..LANES {
            let y = o[l] * gain(base + (l + 1) as f32);
            o[l] = y;
            if METER {
                let a = y.abs();
                peak[l] = if a > peak[l] { a } else { peak[l] };
                sum[l] += y * y;
            }
        }
        base += LANES as f32;
    }
    let mut p = 0.0f32;
    let mut q = 0.0f32;
    for o in chunks.into_remainder() {
        base += 1.0;
        *o *= gain(base);
        if METER {
            p = p.max(o.abs());
            q += *o * *o;
        }
    }
    (fold_peak(peak, p), fold_sum(sum, q))
}

/// Writes `src * gain(w)` to `out` (as [`scale_lanes`], without the in-place pass and a
/// copy) and returns the peak and the sum of squares of what it wrote.
#[inline(always)]
fn scale_into_lanes<const METER: bool>(
    out: &mut [f32],
    src: &[f32],
    gain: impl Fn(f32) -> f32,
) -> (f32, f32) {
    let mut peak = [0.0f32; LANES];
    let mut sum = [0.0f32; LANES];
    let mut out_chunks = out.chunks_exact_mut(LANES);
    let mut src_chunks = src.chunks_exact(LANES);
    let mut base = 0.0f32;
    for (o, x) in (&mut out_chunks).zip(&mut src_chunks) {
        for l in 0..LANES {
            let y = x[l] * gain(base + (l + 1) as f32);
            o[l] = y;
            if METER {
                let a = y.abs();
                peak[l] = if a > peak[l] { a } else { peak[l] };
                sum[l] += y * y;
            }
        }
        base += LANES as f32;
    }
    let mut p = 0.0f32;
    let mut q = 0.0f32;
    for (o, &x) in out_chunks
        .into_remainder()
        .iter_mut()
        .zip(src_chunks.remainder())
    {
        base += 1.0;
        let y = x * gain(base);
        *o = y;
        if METER {
            p = p.max(y.abs());
            q += y * y;
        }
    }
    (fold_peak(peak, p), fold_sum(sum, q))
}

#[inline(always)]
fn fold_peak(lanes: [f32; LANES], rest: f32) -> f32 {
    lanes.into_iter().fold(rest, f32::max)
}

#[inline(always)]
fn fold_sum(lanes: [f32; LANES], rest: f32) -> f32 {
    lanes.into_iter().sum::<f32>() + rest
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-frame loops the lane loops replace.
    fn mix_plain(out: &mut [f32], src: &[f32], g0: f32, dg: f32, s0: f32, ds: f32) -> (f32, f32) {
        let (mut peak, mut sum) = (0.0f32, 0.0f32);
        for (f, (o, &x)) in out.iter_mut().zip(src).enumerate() {
            let w = (f + 1) as f32;
            let y = x * (g0 + dg * w);
            peak = peak.max(y.abs());
            sum += y * y;
            *o += y * (s0 + ds * w);
        }
        (peak, sum)
    }

    #[test]
    fn the_lane_loops_write_the_same_samples_as_a_plain_loop() {
        for frames in [0, 1, 7, 8, 9, 480, 483] {
            let src: Vec<f32> = (0..frames)
                .map(|i| ((i * 37 % 101) as f32 - 50.0) / 50.0)
                .collect();
            let start: Vec<f32> = (0..frames).map(|i| (i % 5) as f32 * 0.1).collect();
            let (g0, dg, s0, ds) = (0.25, 0.75 / 480.0, 1.0, -1.0 / 480.0);
            let mut want = start.clone();
            let (wp, ws) = mix_plain(&mut want, &src, g0, dg, s0, ds);
            let mut got = start.clone();
            let (gp, gs) = mix_lanes::<true>(&mut got, &src, |w| g0 + dg * w, |w| s0 + ds * w);
            assert_eq!(got, want, "{frames} frames");
            assert_eq!(gp, wp, "{frames} frames");
            assert!(
                (gs - ws).abs() <= ws * 1e-5,
                "{frames} frames: {gs} vs {ws}"
            );

            let mut want = src.clone();
            let mut wsum = 0.0f32;
            let mut wpeak = 0.0f32;
            for (f, y) in want.iter_mut().enumerate() {
                *y *= g0 + dg * (f + 1) as f32;
                wpeak = wpeak.max(y.abs());
                wsum += *y * *y;
            }
            let mut got = src.clone();
            let (gp, gs) = scale_lanes::<true>(&mut got, |w| g0 + dg * w);
            assert_eq!(got, want, "{frames} frames");
            assert_eq!(gp, wpeak);
            assert!(
                (gs - wsum).abs() <= wsum * 1e-5,
                "{frames} frames: {gs} vs {wsum}"
            );
        }
    }

    /// The lane loop as it was before the add-only variant (F-15a), to check that
    /// the metered loop still gives bit-identical meter sums.
    fn mix_lanes_before(
        out: &mut [f32],
        src: &[f32],
        gain: impl Fn(f32) -> f32,
        send: impl Fn(f32) -> f32,
    ) -> (f32, f32) {
        let mut peak = [0.0f32; LANES];
        let mut sum = [0.0f32; LANES];
        let mut out_chunks = out.chunks_exact_mut(LANES);
        let mut src_chunks = src.chunks_exact(LANES);
        let mut base = 0.0f32;
        for (o, x) in (&mut out_chunks).zip(&mut src_chunks) {
            for l in 0..LANES {
                let w = base + (l + 1) as f32;
                let y = x[l] * gain(w);
                let a = y.abs();
                peak[l] = if a > peak[l] { a } else { peak[l] };
                sum[l] += y * y;
                o[l] += y * send(w);
            }
            base += LANES as f32;
        }
        let mut p = 0.0f32;
        let mut q = 0.0f32;
        for (o, &x) in out_chunks
            .into_remainder()
            .iter_mut()
            .zip(src_chunks.remainder())
        {
            base += 1.0;
            let y = x * gain(base);
            p = p.max(y.abs());
            q += y * y;
            *o += y * send(base);
        }
        (fold_peak(peak, p), fold_sum(sum, q))
    }

    /// Gains for each branch of [`StripGains::mix`]: silent, steady, fader ramp, send
    /// ramp, and on the monitor.
    fn every_branch() -> [StripGains; 5] {
        let g = |g0: f32, g1: f32, s0: f32, s1: f32, m0: f32, m1: f32| {
            let plain = s0 == 1.0 && s1 == 1.0 && m0 == 0.0 && m1 == 0.0;
            StripGains {
                g0,
                dg: (g1 - g0) / 480.0,
                s0,
                ds: (s1 - s0) / 480.0,
                m0,
                dm: (m1 - m0) / 480.0,
                plain,
                silent: plain && g0 == 0.0 && g1 == 0.0,
                steady: g0 == g1,
                off_monitor: m0 == 0.0 && m1 == 0.0,
            }
        };
        [
            g(0.0, 0.0, 1.0, 1.0, 0.0, 0.0),
            g(0.7, 0.7, 1.0, 1.0, 0.0, 0.0),
            g(0.2, 0.9, 1.0, 1.0, 0.0, 0.0),
            g(0.5, 0.5, 1.0, 0.0, 0.0, 0.0),
            g(0.3, 0.6, 1.0, 0.0, 0.0, 1.0),
        ]
    }

    #[test]
    fn without_a_meter_the_mix_writes_the_same_samples_and_with_one_the_same_sums() {
        for frames in [0, 1, 7, 8, 9, 480, 483] {
            let src: Vec<f32> = (0..frames)
                .map(|i| ((i * 37 % 101) as f32 - 50.0) / 50.0)
                .collect();
            let start: Vec<f32> = (0..frames).map(|i| (i % 5) as f32 * 0.1).collect();
            for (k, gains) in every_branch().iter().enumerate() {
                let mut mon_on = vec![0.25f32; frames * 2];
                let mut mon_off = mon_on.clone();
                let mut on = start.clone();
                let mut off = start.clone();
                let sums = gains.mix::<true>(&mut on, &src, &mut mon_on, 1);
                assert_eq!(
                    gains.mix::<false>(&mut off, &src, &mut mon_off, 1),
                    (0.0, 0.0)
                );
                assert_eq!(on, off, "branch {k}, {frames} frames");
                assert_eq!(mon_on, mon_off, "branch {k}, {frames} frames");
                if k == 1 || k == 2 || k == 3 {
                    let StripGains { g0, dg, s0, ds, .. } = *gains;
                    let mut before = start.clone();
                    let want = if k == 1 {
                        mix_lanes_before(&mut before, &src, |_| g0, |_| 1.0)
                    } else if k == 2 {
                        mix_lanes_before(&mut before, &src, |w| g0 + dg * w, |_| 1.0)
                    } else {
                        mix_lanes_before(&mut before, &src, |w| g0 + dg * w, |w| s0 + ds * w)
                    };
                    assert_eq!(on, before, "branch {k}, {frames} frames");
                    assert_eq!(sums.0.to_bits(), want.0.to_bits(), "branch {k}");
                    assert_eq!(sums.1.to_bits(), want.1.to_bits(), "branch {k}");
                }
            }
            let mut on = src.clone();
            let mut off = src.clone();
            let _ = scale_lanes::<true>(&mut on, |w| 0.5 + 0.001 * w);
            assert_eq!(
                scale_lanes::<false>(&mut off, |w| 0.5 + 0.001 * w),
                (0.0, 0.0)
            );
            assert_eq!(on, off, "{frames} frames");
        }
    }

    #[test]
    fn taking_the_same_target_again_keeps_a_ramp_running() {
        let t0 = Timestamp::ZERO;
        let ramp = Ramp::steady(1.0).towards(0.0, t0, Duration::from_millis(100));
        let mid = t0 + Duration::from_millis(50);
        let again = ramp.towards(0.0, mid, Duration::from_millis(100));
        assert_eq!(again.at(t0 + Duration::from_millis(100)), 0.0);
        assert_eq!(again.at(mid), 0.5);
        // A new target re-ramps from the current value.
        let back = ramp.towards(1.0, mid, Duration::from_millis(100));
        assert_eq!(back.at(mid), 0.5);
        assert_eq!(back.at(mid + Duration::from_millis(100)), 1.0);
    }
}
