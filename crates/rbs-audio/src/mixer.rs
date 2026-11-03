//! The mixer: per-strip gain and mute with ramps, post-fader meters (ADR-0037 decision
//! 7, audio graph spec "Mixer").

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
}

impl Strip {
    /// A strip at `gain`, not muted, without a meter.
    pub fn new(producer: NodeId, gain: f32) -> Self {
        Self {
            producer,
            gain,
            muted: false,
            meter: None,
        }
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

    fn target(&self) -> f32 {
        if self.muted { 0.0 } else { sanitize(self.gain) }
    }
}

fn sanitize(g: f32) -> f32 {
    if g.is_finite() && g > 0.0 { g } else { 0.0 }
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
}

impl Default for MixerParams {
    fn default() -> Self {
        Self {
            strips: Vec::new(),
            master: 1.0,
            master_meter: None,
            ramp: DEFAULT_RAMP,
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

    /// A ramp to `to` starting where this one is at `t`.
    fn towards(&self, to: f32, t: Timestamp, length: Duration) -> Self {
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
    ramp: Ramp,
    /// The input had a block last tick (a returning input fades in).
    present: bool,
    /// The input has a block this tick.
    seen: bool,
}

/// Sums its inputs with per-strip gain and mute and a master gain.
pub struct MixerNode {
    desc: NodeDescriptor,
    config: AudioGraphConfig,
    params: ParamReceiver<MixerParams>,
    capacity: usize,
    /// One per honoured strip, parallel to the first `capacity` of `params.strips`.
    strips: Vec<StripState>,
    scratch: Vec<StripState>,
    master: Ramp,
    /// The parameters have not been applied yet.
    fresh: bool,
    stats: Arc<MixerStats>,
}

/// The descriptor of mixer nodes.
pub fn mixer_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new(MIXER_NODE_TYPE),
        NodeKind::AudioMixer,
        vec![Pin::many("in", LinkKind::Audio)],
        vec![Pin::one("main", LinkKind::Audio)],
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
                scratch: Vec::with_capacity(strips),
                master: Ramp::steady(1.0),
                fresh: true,
                stats: Arc::clone(&stats),
            },
            tx,
            stats,
        )
    }

    /// Takes new parameters into the ramp state, without allocating (both vectors hold
    /// `capacity`).
    fn apply_params(&mut self, t: Timestamp) {
        let p = self.params.get();
        let ramp = p.ramp;
        self.scratch.clear();
        for strip in p.strips.iter().take(self.capacity) {
            let target = strip.target();
            let old = self.strips.iter().find(|s| s.producer == strip.producer);
            let state = match old {
                Some(old) if !self.fresh => StripState {
                    producer: strip.producer,
                    ramp: old.ramp.towards(target, t, ramp),
                    present: old.present,
                    seen: false,
                },
                Some(old) => StripState {
                    producer: strip.producer,
                    ramp: Ramp::steady(target),
                    present: old.present,
                    seen: false,
                },
                None => StripState {
                    producer: strip.producer,
                    ramp: Ramp::steady(target),
                    present: false,
                    seen: false,
                },
            };
            self.scratch.push(state);
        }
        std::mem::swap(&mut self.strips, &mut self.scratch);
        let master = sanitize(p.master);
        self.master = if self.fresh {
            Ramp::steady(master)
        } else {
            self.master.towards(master, t, ramp)
        };
        self.fresh = false;
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
        if self.params.update() || self.fresh {
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

        for s in &mut self.strips {
            s.seen = false;
        }
        for (link, block) in inputs.audios(0).enumerate() {
            let Some(block) = block else { continue };
            let Some(producer) = inputs.source(0, link) else {
                continue;
            };
            let Some(strip) = self.strips.iter_mut().find(|s| s.producer == producer) else {
                self.stats.unknown_inputs.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            if block.format() != format || block.frames() != frames {
                self.stats.invalid_inputs.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if !strip.present {
                // An input that (re)joins fades in from silence.
                strip.ramp = Ramp {
                    from: 0.0,
                    to: strip.ramp.to,
                    start: t0,
                    length: ramp,
                };
            }
            strip.seen = true;
            let (g0, g1) = (strip.ramp.at(t0), strip.ramp.at(t1));
            let meter = self
                .params
                .get()
                .strips
                .iter()
                .find(|s| s.producer == producer);
            let meter = meter.and_then(|s| s.meter.as_deref());
            let dg = (g1 - g0) * inv;
            for (c, ch) in chans.iter_mut().enumerate().take(n) {
                let Some(src) = block.channel(c) else {
                    continue;
                };
                let mut peak = 0.0f32;
                if g0 == g1 {
                    for (o, &x) in ch[..frames].iter_mut().zip(src) {
                        let y = x * g0;
                        peak = peak.max(y.abs());
                        *o += y;
                    }
                } else {
                    for (f, (o, &x)) in ch[..frames].iter_mut().zip(src).enumerate() {
                        let y = x * (g0 + dg * (f + 1) as f32);
                        peak = peak.max(y.abs());
                        *o += y;
                    }
                }
                if let Some(m) = meter {
                    m.record(c, peak);
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
            let mut peak = 0.0f32;
            for (f, y) in ch[..frames].iter_mut().enumerate() {
                *y *= m0 + dm * (f + 1) as f32;
                peak = peak.max(y.abs());
            }
            if let Some(m) = meter {
                m.record(c, peak);
            }
        }
        out.commit(format, t0, frames)?;
        Ok(())
    }
}
