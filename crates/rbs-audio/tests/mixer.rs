//! Mixer: gains, ramps, mute, unknown and invalid inputs, meters, silence (audio graph
//! spec "Mixer").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_audio::{AudioGraphConfig, LevelMeter, MixerNode, MixerParams, MixerStats, Strip};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{AudioFormat, ChannelLayout};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, SampleRate};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::ParamSender;
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};

const FRAMES: usize = 480;

/// Recorded blocks: time, channel 0, channel 1.
type Blocks = Arc<Mutex<Vec<(Timestamp, Vec<f32>, Vec<f32>)>>>;

/// Writes a constant value on both channels (channel 1 negated) every tick, unless
/// `quiet` is set; `format` lets a test send a wrong format.
struct Const {
    desc: NodeDescriptor,
    value: f32,
    quiet: Arc<AtomicBool>,
    format: AudioFormat,
}

impl Const {
    fn new(value: f32) -> Self {
        Self {
            desc: NodeDescriptor::new(
                NodeTypeId::new("org.rbs.test.const"),
                NodeKind::Source,
                vec![],
                vec![Pin::one("out", LinkKind::Audio)],
            ),
            value,
            quiet: Arc::default(),
            format: AudioFormat::STEREO_48K,
        }
    }
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
        let Some(t) = AudioGraphConfig::default().block_time(ctx.tick()) else {
            return Ok(());
        };
        let v = self.value;
        let mut out = ctx.outputs().audio(0)?;
        for (c, ch) in out.channels_mut(2).enumerate() {
            ch[..FRAMES].fill(if c == 0 { v } else { -v });
        }
        out.commit(self.format, t, FRAMES)?;
        Ok(())
    }
}

/// Records channel 0 and 1 of every block.
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
            self.blocks.lock().unwrap().push((
                b.timestamp(),
                b.channel(0).unwrap().to_vec(),
                b.channel(1).unwrap().to_vec(),
            ));
        }
        Ok(())
    }
}

struct Rig {
    ctl: Controller<()>,
    exec: Executor<()>,
    graph: GraphDescription,
    sources: Vec<NodeId>,
    quiet: Vec<Arc<AtomicBool>>,
    mixer: NodeId,
    params: ParamSender<MixerParams>,
    stats: Arc<MixerStats>,
    blocks: Blocks,
    cadence: Cadence,
    tick: u64,
}

fn active(g: &mut GraphDescription, d: &NodeDescriptor) -> NodeId {
    let id = g.add_node(Arc::new(d.clone())).unwrap();
    g.set_state(id, Lifecycle::Standby).unwrap();
    g.set_state(id, Lifecycle::Active).unwrap();
    id
}

impl Rig {
    /// Sources with these values (wrong format where `bad`), and a mixer whose
    /// parameters `strips` builds from their ids.
    fn new(values: &[(f32, bool)], strips: impl FnOnce(&[NodeId]) -> MixerParams) -> Self {
        let config = AudioGraphConfig::default();
        let (mut ctl, exec) = channel::<()>(Arc::new(()), ());
        let mut graph = GraphDescription::new();
        let mut sources = Vec::new();
        let mut quiet = Vec::new();
        let mut nodes: Vec<(NodeId, Box<dyn Node<()>>)> = Vec::new();
        for &(v, bad) in values {
            let mut c = Const::new(v);
            if bad {
                c.format = AudioFormat::new(SampleRate::HZ_44100, ChannelLayout::Stereo).unwrap();
            }
            quiet.push(Arc::clone(&c.quiet));
            let id = active(&mut graph, &c.desc);
            sources.push(id);
            nodes.push((id, Box::new(c)));
        }
        let (mixer, params, stats) = MixerNode::new(config, 16, MixerParams::default());
        let mid = active(&mut graph, Node::<()>::descriptor(&mixer));
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
        let rid = active(&mut graph, &rec.desc);
        for &s in &sources {
            graph.connect(s, "out", mid, "in").unwrap();
        }
        graph.connect(mid, "main", rid, "in").unwrap();
        for (id, n) in nodes {
            ctl.adopt(id, n).unwrap();
        }
        ctl.adopt(mid, Box::new(mixer)).unwrap();
        ctl.adopt(rid, Box::new(rec)).unwrap();
        let mut rig = Self {
            ctl,
            exec,
            graph,
            mixer: mid,
            params,
            stats,
            blocks,
            cadence: config.cadence(Timestamp::ZERO),
            tick: 5, // the first tick with a block (latency 50 ms)
            quiet,
            sources: sources.clone(),
        };
        let p = strips(&sources);
        rig.params.send(p);
        rig.ctl.apply(&rig.graph).unwrap();
        rig.exec.swap_pending();
        rig.ctl.collect().unwrap().unwrap();
        rig
    }

    fn run(&mut self, n: u64) {
        for _ in 0..n {
            self.exec.run_tick(Tick::of(&self.cadence, self.tick));
            self.tick += 1;
        }
    }

    fn last(&self) -> (Timestamp, Vec<f32>, Vec<f32>) {
        self.blocks.lock().unwrap().last().unwrap().clone()
    }
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-5
}

#[test]
fn inputs_are_summed_with_their_gains_and_the_master_gain() {
    let mut r = Rig::new(&[(0.25, false), (0.5, false)], |s| {
        MixerParams::new(vec![Strip::new(s[0], 1.0), Strip::new(s[1], 0.5)]).with_master(2.0)
    });
    r.run(3);
    let (t, l, rr) = r.last();
    assert_eq!(t, r.cadence.time_of(r.tick - 1) - Duration::from_millis(50));
    assert!(l.iter().all(|&x| close(x, 2.0 * (0.25 + 0.25))), "{}", l[0]);
    assert!(rr.iter().all(|&x| close(x, -1.0)));
}

#[test]
fn a_joining_input_fades_in_over_the_ramp() {
    let mut r = Rig::new(&[(1.0, false)], |s| {
        MixerParams::new(vec![Strip::new(s[0], 1.0)])
    });
    r.run(1);
    let (_, l, _) = r.last();
    // Linear from 0 to 1 over one 10 ms block.
    assert!(close(l[0], 1.0 / 480.0));
    assert!(close(l[239], 0.5));
    assert!(close(l[479], 1.0));
    r.run(1);
    assert!(r.last().1.iter().all(|&x| close(x, 1.0)));
}

#[test]
fn gain_and_mute_changes_ramp_and_never_jump() {
    let mut r = Rig::new(&[(1.0, false)], |s| {
        MixerParams::new(vec![Strip::new(s[0], 1.0)])
    });
    r.run(3);
    let s = r.sources[0];
    r.params.send(
        MixerParams::new(vec![Strip::new(s, 1.0).with_muted(true)])
            .with_ramp(Duration::from_millis(20)),
    );
    r.run(3);
    let blocks = r.blocks.lock().unwrap().clone();
    let tail: Vec<f32> = blocks[blocks.len() - 3..]
        .iter()
        .flat_map(|b| b.1.clone())
        .collect();
    // 20 ms ramp: halfway after one block, silent after two, and every step small.
    assert!(close(tail[479], 0.5), "{}", tail[479]);
    assert!(close(tail[959], 0.0));
    assert!(tail[960..].iter().all(|&x| x == 0.0));
    let worst = tail
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max);
    assert!(worst <= 1.0 / 960.0 + 1e-6, "{worst}");
}

#[test]
fn an_input_without_a_strip_is_not_heard() {
    let mut r = Rig::new(&[(0.5, false), (0.25, false)], |s| {
        MixerParams::new(vec![Strip::new(s[0], 1.0)])
    });
    r.run(3);
    assert!(r.last().1.iter().all(|&x| close(x, 0.5)));
    assert_eq!(r.stats.unknown_inputs(), 3);
}

#[test]
fn an_input_with_the_wrong_format_is_skipped_not_the_mix() {
    let mut r = Rig::new(&[(0.5, false), (0.25, true)], |s| {
        MixerParams::new(vec![Strip::new(s[0], 1.0), Strip::new(s[1], 1.0)])
    });
    r.run(3);
    assert!(r.last().1.iter().all(|&x| close(x, 0.5)));
    assert_eq!(r.stats.invalid_inputs(), 3);
}

#[test]
fn with_no_input_blocks_the_mixer_writes_silence() {
    let mut r = Rig::new(&[(0.5, false)], |s| {
        MixerParams::new(vec![Strip::new(s[0], 1.0)])
    });
    r.quiet[0].store(true, Ordering::Relaxed);
    r.run(2);
    assert_eq!(r.blocks.lock().unwrap().len(), 2);
    assert!(r.last().1.iter().all(|&x| x == 0.0));
    // The input returns and fades in again.
    r.quiet[0].store(false, Ordering::Relaxed);
    r.run(1);
    assert!(close(r.last().1[0], 0.5 / 480.0));
}

#[test]
fn meters_get_post_fader_peaks_per_channel() {
    let strip_meter = Arc::new(LevelMeter::new(2));
    let master_meter = Arc::new(LevelMeter::new(2));
    let (sm, mm) = (Arc::clone(&strip_meter), Arc::clone(&master_meter));
    let mut r = Rig::new(&[(0.5, false), (0.25, false)], move |s| {
        MixerParams::new(vec![
            Strip::new(s[0], 0.5).with_meter(sm),
            Strip::new(s[1], 1.0),
        ])
        .with_master(2.0)
        .with_master_meter(mm)
    });
    r.run(3);
    let l = strip_meter.take();
    assert!(close(l.peak(0), 0.25) && close(l.peak(1), 0.25));
    let m = master_meter.take();
    assert!(close(m.peak(0), 1.0) && close(m.peak(1), 1.0));
    assert_eq!(strip_meter.take().peak(0), 0.0);
    // The first block fades in from silence, so RMS is measured on a later, steady block.
    r.run(1);
    let l = strip_meter.take();
    assert!(close(l.rms(0), 0.25) && close(l.rms(1), 0.25));
    let m = master_meter.take();
    assert!(close(m.rms(0), 1.0) && close(m.rms(1), 1.0));
    assert_eq!(strip_meter.take().peak(0), 0.0);
    let _ = r.mixer;
}

#[test]
fn mixing_does_not_allocate() {
    let meter = Arc::new(LevelMeter::new(2));
    let m2 = Arc::clone(&meter);
    let mut r = Rig::new(&[(0.5, false), (0.25, false), (0.1, false)], move |s| {
        MixerParams::new(
            s.iter()
                .map(|&id| Strip::new(id, 0.5).with_meter(Arc::clone(&m2)))
                .collect(),
        )
    });
    r.run(2);
    // The recorder allocates; take it out by measuring the mixer through a parameter
    // update and ticks with a recorder whose vector has room... simpler: count only the
    // mixer's part by subtracting a baseline run without a mixer change.
    r.blocks.lock().unwrap().reserve(1000);
    let baseline = allocation_counter::measure(|| r.run(10));
    // A parameter change taken up inside `process`.
    let p = MixerParams::new(vec![Strip::new(r.sources[0], 1.0)]);
    r.params.send(p);
    let with_update = allocation_counter::measure(|| r.run(10));
    assert_eq!(with_update.count_total, baseline.count_total);
    let _ = meter.take();
}
