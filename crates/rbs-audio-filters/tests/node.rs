//! The audio filter node inside a real executor: source -> filter -> recorder.

#![allow(clippy::expect_used)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rbs_audio_filters::gain::Gain;
use rbs_audio_filters::gate::NoiseGate;
use rbs_audio_filters::{AudioFilterNode, AudioFilterStats, AudioFilterType, DspSetup};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{AudioFormat, ChannelLayout};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, Rational, SampleRate};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::ParamSender;
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_params::{Value, Values};

const FRAMES: usize = 480;

/// Recorded blocks: timestamp and channel 0.
type Blocks = Arc<Mutex<Vec<(Timestamp, Vec<f32>)>>>;

struct Const {
    desc: NodeDescriptor,
    value: f32,
    quiet: Arc<AtomicBool>,
    format: AudioFormat,
}

impl Node<()> for Const {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        if self.quiet.load(Ordering::Relaxed) {
            return Ok(());
        }
        let t = ctx.tick().time;
        let mut out = ctx.outputs().audio(0)?;
        for ch in out.channels_mut(2) {
            ch[..FRAMES].fill(self.value);
        }
        out.commit(self.format, t, FRAMES)?;
        Ok(())
    }
}

struct Rec {
    desc: NodeDescriptor,
    blocks: Blocks,
}

impl Node<()> for Rec {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        if let Some(b) = ctx.inputs().audio(0, 0) {
            let mut v = self.blocks.lock().expect("lock");
            if v.len() < 1000 {
                v.push((b.timestamp(), b.channel(0).expect("channel").to_vec()));
            }
        }
        Ok(())
    }
}

fn active(g: &mut GraphDescription, d: &NodeDescriptor) -> NodeId {
    let id = g.add_node(Arc::new(d.clone())).expect("add");
    g.set_state(id, Lifecycle::Standby).expect("standby");
    g.set_state(id, Lifecycle::Active).expect("active");
    id
}

struct Rig {
    exec: Executor<()>,
    params: ParamSender<Values>,
    stats: Arc<AudioFilterStats>,
    quiet: Arc<AtomicBool>,
    blocks: Blocks,
    cadence: Cadence,
    tick: u64,
    _ctl: Controller<()>,
}

impl Rig {
    fn new(ty: &dyn AudioFilterType, values: Values, source_format: AudioFormat) -> Self {
        let setup = DspSetup::new(AudioFormat::STEREO_48K, FRAMES as u32);
        let (mut ctl, exec) = channel::<()>(Arc::new(()), ());
        let mut graph = GraphDescription::new();
        let quiet = Arc::new(AtomicBool::new(false));
        let src = Const {
            desc: NodeDescriptor::new(
                NodeTypeId::new("org.rbs.test.const"),
                NodeKind::Source,
                vec![],
                vec![Pin::one("out", LinkKind::Audio)],
            ),
            value: 0.25,
            quiet: Arc::clone(&quiet),
            format: source_format,
        };
        let (filter, (params, _meter, stats)) =
            AudioFilterNode::new(ty, setup, values).expect("valid");
        let rec = Rec {
            desc: NodeDescriptor::new(
                NodeTypeId::new("org.rbs.test.rec"),
                NodeKind::Output,
                vec![Pin::one("in", LinkKind::Audio)],
                vec![],
            ),
            blocks: Arc::default(),
        };
        let blocks = Arc::clone(&rec.blocks);
        let s = active(&mut graph, &src.desc);
        let f = active(&mut graph, Node::<()>::descriptor(&filter));
        let r = active(&mut graph, &rec.desc);
        graph.connect(s, "out", f, "in").expect("link");
        graph.connect(f, "out", r, "in").expect("link");
        ctl.adopt(s, Box::new(src)).expect("adopt");
        ctl.adopt(f, Box::new(filter)).expect("adopt");
        ctl.adopt(r, Box::new(rec)).expect("adopt");
        ctl.apply(&graph).expect("apply");
        let mut exec = exec;
        exec.swap_pending();
        ctl.collect().expect("collect").expect("collected");
        let rate = Rational::new(100, 1).expect("rate");
        Self {
            exec,
            params,
            stats,
            quiet,
            blocks,
            cadence: Cadence::new(Timestamp::ZERO, rate),
            tick: 5,
            _ctl: ctl,
        }
    }

    fn run(&mut self, n: u64) {
        for _ in 0..n {
            self.exec.run_tick(Tick::of(&self.cadence, self.tick));
            self.tick += 1;
        }
    }
}

fn gain(db: f32) -> Values {
    Values::from_pairs(&Gain as &dyn AudioFilterType, [("gain", Value::Number(db))]).expect("valid")
}

#[test]
fn the_node_applies_the_dsp_and_keeps_the_timestamp() {
    let mut r = Rig::new(&Gain, gain(6.0), AudioFormat::STEREO_48K);
    r.run(3);
    let blocks = r.blocks.lock().expect("lock").clone();
    // A joining node has no output on its first tick (ADR-0020).
    assert!(!blocks.is_empty());
    let (t, ch) = blocks.last().expect("a block");
    let expect = 0.25 * 10.0f32.powf(6.0 / 20.0);
    assert!((ch[0] - expect).abs() < 1e-5 && (ch[FRAMES - 1] - expect).abs() < 1e-5);
    assert!(*t > Timestamp::ZERO);
}

#[test]
fn a_value_edit_takes_effect_without_a_reset() {
    let mut r = Rig::new(&Gain, gain(0.0), AudioFormat::STEREO_48K);
    r.run(3);
    r.params.send(gain(-6.0));
    r.run(3);
    let blocks = r.blocks.lock().expect("lock").clone();
    let last = &blocks.last().expect("a block").1;
    assert!((last[0] - 0.25 * 10.0f32.powf(-6.0 / 20.0)).abs() < 1e-5);
}

#[test]
fn no_input_block_publishes_nothing() {
    let mut r = Rig::new(&Gain, gain(0.0), AudioFormat::STEREO_48K);
    r.run(3);
    let n = r.blocks.lock().expect("lock").len();
    r.quiet.store(true, Ordering::Relaxed);
    r.run(3);
    assert_eq!(r.blocks.lock().expect("lock").len(), n);
}

#[test]
fn a_block_of_the_wrong_format_is_dropped_and_counted() {
    let wrong = AudioFormat::new(SampleRate::HZ_44100, ChannelLayout::Stereo).expect("format");
    let mut r = Rig::new(&Gain, gain(0.0), wrong);
    r.run(4);
    assert!(r.stats.invalid_inputs() > 0);
    assert!(r.blocks.lock().expect("lock").is_empty());
}

#[test]
fn a_gate_node_opens_on_a_loud_input() {
    let values = Values::defaults(&NoiseGate as &dyn AudioFilterType);
    let mut r = Rig::new(&NoiseGate, values, AudioFormat::STEREO_48K);
    r.run(1);
    r.run(1);
    let blocks = r.blocks.lock().expect("lock").clone();
    // 0.25 is above the threshold: the gate opens within the block and reaches unity.
    let last = &blocks.last().expect("a block").1;
    assert!((last[FRAMES - 1] - 0.25).abs() < 1e-4);
}

#[test]
fn running_the_node_does_not_allocate() {
    let mut r = Rig::new(&Gain, gain(0.0), AudioFormat::STEREO_48K);
    r.run(2);
    // The recorder allocates (until it holds 1000 blocks); fill it first so only the
    // source, the filter and the executor run.
    r.run(1000);
    let baseline = allocation_counter::measure(|| r.run(10));
    r.params.send(gain(3.0));
    let with_update = allocation_counter::measure(|| r.run(10));
    assert_eq!(with_update.count_total, baseline.count_total);
}

/// A test filter with latency: a plain delay line of `DELAY` frames (ADR-0130).
struct Delay;

const DELAY: usize = 100;

struct DelayDsp {
    lines: Vec<Vec<f32>>,
    pos: usize,
}

impl rbs_audio_filters::AudioDsp for DelayDsp {
    fn set(&mut self, _: &Values) {}
    fn reset(&mut self) {
        for l in &mut self.lines {
            l.fill(0.0);
        }
        self.pos = 0;
    }
    fn process(&mut self, channels: &mut [&mut [f32]], frames: usize) {
        for f in 0..frames {
            for (ch, line) in channels.iter_mut().zip(&mut self.lines) {
                let out = line[self.pos];
                line[self.pos] = if ch[f].is_normal() { ch[f] } else { 0.0 };
                ch[f] = out;
            }
            self.pos = (self.pos + 1) % DELAY;
        }
    }
    fn latency_frames(&self) -> u32 {
        DELAY as u32
    }
}

impl AudioFilterType for Delay {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new("org.rbs.audio-filter.test-delay")
    }
    fn name(&self) -> &'static str {
        "Delay"
    }
    fn fields(&self) -> &'static [rbs_params::Field] {
        &[]
    }
    fn make(
        &self,
        setup: DspSetup,
        _: &Values,
    ) -> Result<Box<dyn rbs_audio_filters::AudioDsp>, rbs_params::ParamError> {
        let n = setup.format.channels.channels();
        Ok(Box::new(DelayDsp {
            lines: vec![vec![0.0; DELAY]; n],
            pos: 0,
        }))
    }
    fn latency_frames(&self, _: DspSetup) -> u32 {
        DELAY as u32
    }
}

#[test]
fn a_latent_filter_reports_its_latency_and_delays_the_audio() {
    assert!(rbs_audio_filters::conformance::check_type(&Delay).is_empty());
    let mut r = Rig::new(
        &Delay,
        Values::defaults(&Delay as &dyn AudioFilterType),
        AudioFormat::STEREO_48K,
    );
    assert_eq!(r.stats.latency_frames(), DELAY as u32);
    r.run(3);
    let blocks = r.blocks.lock().expect("lock").clone();
    let first = &blocks.first().expect("a block").1;
    assert!(first[..DELAY].iter().all(|&x| x == 0.0));
    assert!(first[DELAY..].iter().all(|&x| x == 0.25));
}

#[test]
fn a_latent_filter_starts_clean_after_a_gap() {
    let mut r = Rig::new(
        &Delay,
        Values::defaults(&Delay as &dyn AudioFilterType),
        AudioFormat::STEREO_48K,
    );
    r.run(3);
    r.quiet.store(true, Ordering::Relaxed);
    r.run(2);
    let n = r.blocks.lock().expect("lock").len();
    r.quiet.store(false, Ordering::Relaxed);
    r.run(1);
    let blocks = r.blocks.lock().expect("lock").clone();
    assert_eq!(blocks.len(), n + 1);
    let resumed = &blocks.last().expect("a block").1;
    // The stale 0.25 held in the delay line before the gap must not play.
    assert!(resumed[..DELAY].iter().all(|&x| x == 0.0));
    assert!(resumed[DELAY..].iter().all(|&x| x == 0.25));
}

#[test]
fn a_pass_through_has_no_latency() {
    let setup = DspSetup::new(AudioFormat::STEREO_48K, FRAMES as u32);
    let (_, (_, _, stats)) = AudioFilterNode::pass_through(
        &Delay,
        setup,
        Values::defaults(&Delay as &dyn AudioFilterType),
    );
    assert_eq!(stats.latency_frames(), 0);
}
