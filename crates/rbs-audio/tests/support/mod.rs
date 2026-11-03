//! A scripted audio input device for the audio graph tests: a tone stamped with its
//! true master time, a device clock that drifts, timestamp jitter, delivery delay and
//! gaps, all driven by a `TestClock` (ADR-0032: no sleeps, no real time).

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_audio::{AudioGraphConfig, InputNode, InputParams};
use rbs_audio_io::{AudioInputStream, AudioIoError, AudioRead, InputStats};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{AudioFormat, ChannelLayout};
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_core::timing::{Cadence, SampleRate};
use rbs_graph::description::GraphDescription;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::ParamSender;
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};

/// Frequency of the test tone: low, so a phase measures time unambiguously over
/// +-10 ms.
pub const TONE_HZ: f64 = 50.0;
pub const AMPLITUDE: f64 = 0.5;

/// What the scripted device does.
#[derive(Clone)]
pub struct Script {
    pub rate: SampleRate,
    pub channels: ChannelLayout,
    /// Device clock runs fast by this fraction (1e-4 = 100 ppm).
    pub drift: f64,
    /// Peak timestamp jitter.
    pub jitter: Duration,
    /// Packet length in device frames.
    pub packet: usize,
    /// How long after its last frame a packet becomes readable.
    pub delay: Duration,
    /// Device frame ranges `(first, len)` never delivered (the next run is flagged).
    pub gaps: Vec<(u64, u64)>,
    /// Packets from device frame `.0` up to (not including) `.1` are delivered `.2`
    /// later.
    pub late: Option<(u64, u64, Duration)>,
    /// Packets starting at device frames `.0...1` report timestamps `.2` seconds off
    /// (a stray timestamp); their audio and delivery time are unchanged.
    pub stray: Option<(u64, u64, f64)>,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            rate: SampleRate::HZ_48000,
            channels: ChannelLayout::Mono,
            drift: 0.0,
            jitter: Duration::ZERO,
            packet: 480,
            delay: Duration::from_millis(1),
            gaps: Vec::new(),
            late: None,
            stray: None,
        }
    }
}

pub struct Device {
    script: Script,
    clock: TestClock,
    started: Option<f64>,
    /// Next device frame to deliver, and how far into its packet a partial read got.
    next: u64,
    pending_discontinuity: bool,
}

impl Device {
    pub fn new(script: Script, clock: TestClock) -> Self {
        Self {
            script,
            clock,
            started: None,
            next: 0,
            pending_discontinuity: true,
        }
    }

    /// True master time (seconds) of device frame `k`.
    fn time_of(&self, k: u64) -> f64 {
        self.started.unwrap_or(0.0)
            + k as f64 / (f64::from(self.script.rate.hz()) * (1.0 + self.script.drift))
    }

    fn jitter(&self, k: u64) -> f64 {
        let x = (k.wrapping_mul(2_654_435_761) % 1000) as f64 / 1000.0 - 0.5;
        2.0 * x * self.script.jitter.as_secs_f64()
    }
}

/// The tone at true master time `t` seconds.
pub fn tone(t: f64) -> f64 {
    AMPLITUDE * (std::f64::consts::TAU * TONE_HZ * t).sin()
}

impl AudioInputStream for Device {
    fn format(&self) -> AudioFormat {
        AudioFormat::new(self.script.rate, self.script.channels).unwrap()
    }
    fn start(&mut self) -> Result<(), AudioIoError> {
        if self.started.is_none() {
            self.started = Some(self.clock.now().as_nanos() as f64 * 1e-9);
        }
        Ok(())
    }
    fn stop(&mut self) {
        self.started = None;
        self.next = 0;
        self.pending_discontinuity = true;
    }
    fn read(&mut self, out: &mut [&mut [f32]]) -> Result<AudioRead, AudioIoError> {
        if self.started.is_none() {
            return Ok(AudioRead::EMPTY);
        }
        // Skip gaps.
        for &(first, len) in &self.script.gaps {
            if self.next >= first && self.next < first + len {
                self.next = first + len;
                self.pending_discontinuity = true;
            }
        }
        let packet = self.script.packet as u64;
        let packet_end = (self.next / packet + 1) * packet;
        let mut delay = self.script.delay.as_secs_f64();
        if let Some((from, until, late)) = self.script.late {
            if self.next >= from && self.next < until {
                delay += late.as_secs_f64();
            }
        }
        let now = self.clock.now().as_nanos() as f64 * 1e-9;
        if now < self.time_of(packet_end) + delay {
            return Ok(AudioRead::EMPTY);
        }
        let mut end = packet_end;
        for &(first, _) in &self.script.gaps {
            if first > self.next && first < end {
                end = first;
            }
        }
        let n = ((end - self.next) as usize).min(out[0].len());
        for k in 0..n {
            let v = tone(self.time_of(self.next + k as u64)) as f32;
            for ch in out.iter_mut() {
                ch[k] = v;
            }
        }
        let first = self.next;
        let mut ts = self.time_of(first) + self.jitter(first);
        if let Some((from, until, off)) = self.script.stray {
            if first >= from && first < until {
                ts += off;
            }
        }
        self.next += n as u64;
        let d = std::mem::replace(&mut self.pending_discontinuity, false);
        Ok(AudioRead::new(
            n,
            Timestamp::from_nanos((ts * 1e9).round() as u64),
            d,
        ))
    }
    fn available(&self) -> usize {
        0
    }
    fn stats(&self) -> InputStats {
        InputStats::default()
    }
}

/// One measured block: its time, the tone's time error (output content time minus
/// wanted time, seconds) and its RMS.
#[derive(Debug, Clone, Copy)]
pub struct Measure {
    pub at: Timestamp,
    pub error: f64,
    pub rms: f64,
}

/// Records the time error of the tone in each block of input 0 (first channel).
pub struct Probe {
    desc: NodeDescriptor,
    pub seen: Arc<Mutex<Vec<Measure>>>,
    /// Signed sync offset (ns) the source was asked for.
    offset_ns: i64,
}

impl Probe {
    pub fn new(offset_ns: i64) -> Self {
        Self {
            desc: NodeDescriptor::new(
                NodeTypeId::new("org.rbs.test.probe"),
                NodeKind::Output,
                vec![Pin::one("in", LinkKind::Audio)],
                vec![],
            ),
            seen: Arc::default(),
            offset_ns,
        }
    }
}

impl Node<()> for Probe {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        let Some(block) = ctx.inputs().audio(0, 0) else {
            return Ok(());
        };
        let x = block.channel(0).unwrap();
        let rate = f64::from(block.format().sample_rate.hz());
        let t0 = block.timestamp().as_nanos() as f64 * 1e-9 - self.offset_ns as f64 * 1e-9;
        let w = std::f64::consts::TAU * TONE_HZ;
        let (mut a, mut b, mut e) = (0.0, 0.0, 0.0);
        for (k, &v) in x.iter().enumerate() {
            let t = t0 + k as f64 / rate;
            a += f64::from(v) * (w * t).sin();
            b += f64::from(v) * (w * t).cos();
            e += f64::from(v) * f64::from(v);
        }
        let rms = (e / x.len() as f64).sqrt();
        let error = if rms > 0.01 { b.atan2(a) / w } else { f64::NAN };
        self.seen.lock().unwrap().push(Measure {
            at: block.timestamp(),
            error,
            rms,
        });
        Ok(())
    }
}

/// An input node on `script` feeding a probe, on one executor, ticked on the graph's
/// cadence with a `TestClock`.
pub struct Rig {
    pub clock: TestClock,
    pub ctl: Controller<()>,
    pub exec: Executor<()>,
    pub cadence: Cadence,
    pub tick: u64,
    pub seen: Arc<Mutex<Vec<Measure>>>,
    pub params: ParamSender<InputParams>,
    pub stats: Arc<rbs_audio::InputStats>,
    pub config: AudioGraphConfig,
}

impl Rig {
    pub fn new(script: Script, config: AudioGraphConfig, offset: InputParams) -> Self {
        let clock = TestClock::new();
        let device = Device::new(script, clock.clone());
        let (node, mut params, stats) =
            InputNode::new(Box::new(device), config, Duration::from_millis(200));
        params.send(offset);
        let probe = Probe::new(offset.offset_ns);
        let seen = Arc::clone(&probe.seen);
        let (mut ctl, exec) = channel::<()>(Arc::new(()), ());
        ctl.set_audio_block_frames(config.block_frames);
        let mut g = GraphDescription::new();
        let ids: Vec<_> = [Node::<()>::descriptor(&node).clone(), probe.desc.clone()]
            .into_iter()
            .map(|d| {
                let id = g.add_node(Arc::new(d)).unwrap();
                g.set_state(id, Lifecycle::Standby).unwrap();
                g.set_state(id, Lifecycle::Active).unwrap();
                id
            })
            .collect();
        g.connect(ids[0], "out", ids[1], "in").unwrap();
        ctl.adopt(ids[0], Box::new(node)).unwrap();
        ctl.adopt(ids[1], Box::new(probe)).unwrap();
        ctl.apply(&g).unwrap();
        let mut exec = exec;
        exec.swap_pending();
        ctl.collect().unwrap().unwrap();
        Self {
            clock,
            ctl,
            exec,
            cadence: config.cadence(Timestamp::ZERO),
            tick: 0,
            seen,
            params,
            stats,
            config,
        }
    }

    /// Runs `n` ticks, moving the clock to each tick's time first.
    pub fn run(&mut self, n: u64) {
        for _ in 0..n {
            let t = self.cadence.time_of(self.tick);
            let now = self.clock.now();
            if t > now {
                self.clock.advance(t.saturating_since(now));
            }
            self.exec.run_tick(Tick::of(&self.cadence, self.tick));
            self.tick += 1;
        }
    }

    /// Largest absolute time error of measured blocks after `from` (seconds).
    pub fn max_error_after(&self, from: Duration) -> f64 {
        let from = Timestamp::from_nanos(from.as_nanos() as u64);
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.at >= from && !m.error.is_nan())
            .map(|m| m.error.abs())
            .fold(0.0, f64::max)
    }

    pub fn silent_blocks_after(&self, from: Duration) -> usize {
        let from = Timestamp::from_nanos(from.as_nanos() as u64);
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.at >= from && m.error.is_nan())
            .count()
    }
}
