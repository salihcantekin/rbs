//! The mixer's bus outputs: post-fader send levels, the bus fader and meter, `main`
//! sends, `Only` leaving every bus, unread buses doing nothing and ramp state
//! surviving the link swap (audio graph spec "Mixer", ADR-0097 decisions 6, 9 and 10,
//! ADR-0170 decisions 1 and 2).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::{Arc, Mutex};

use rbs_audio::{
    AudioGraphConfig, BUS_PINS, LevelMeter, MixerNode, MixerParams, MonitorMode, Strip,
};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::AudioFormat;
use rbs_core::time::Timestamp;
use rbs_core::timing::Cadence;
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::ParamSender;
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};

const FRAMES: usize = 480;

/// Channel 0 of every block recorded, per tick (`None`: no block).
type Seen = Arc<Mutex<Vec<Option<Vec<f32>>>>>;

/// A constant on both channels every tick.
struct Const {
    desc: NodeDescriptor,
    value: f32,
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
        let Some(t) = AudioGraphConfig::default().block_time(ctx.tick()) else {
            return Ok(());
        };
        let mut out = ctx.outputs().audio(0)?;
        for ch in out.channels_mut(2) {
            ch[..FRAMES].fill(self.value);
        }
        out.commit(AudioFormat::STEREO_48K, t, FRAMES)?;
        Ok(())
    }
}

struct Rec {
    desc: NodeDescriptor,
    seen: Seen,
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
        let block = ctx
            .inputs()
            .audio(0, 0)
            .map(|b| b.channel(0).unwrap().to_vec());
        self.seen.lock().unwrap().push(block);
        Ok(())
    }
}

fn rec() -> Rec {
    Rec {
        desc: NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.rec"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Audio)],
            vec![],
        ),
        seen: Arc::default(),
    }
}

fn active(g: &mut GraphDescription, d: &NodeDescriptor) -> NodeId {
    let id = g.add_node(Arc::new(d.clone())).unwrap();
    g.set_state(id, Lifecycle::Standby).unwrap();
    g.set_state(id, Lifecycle::Active).unwrap();
    id
}

/// Constant sources into a mixer with `main`, `monitor` and bus slot 0 and 4
/// recorders; buses and the monitor are linked on demand.
struct Rig {
    ctl: Controller<()>,
    exec: Executor<()>,
    graph: GraphDescription,
    sources: Vec<NodeId>,
    mixer: NodeId,
    monitor: NodeId,
    buses: [NodeId; 2],
    links: Vec<(&'static str, rbs_graph::description::Link)>,
    params: ParamSender<MixerParams>,
    main: Seen,
    mon: Seen,
    bus: [Seen; 2],
    cadence: Cadence,
    tick: u64,
}

/// The bus slots the rig records.
const SLOTS: [usize; 2] = [0, 4];

impl Rig {
    fn new(values: &[f32], params: impl FnOnce(&[NodeId]) -> MixerParams) -> Self {
        let config = AudioGraphConfig::default();
        let (mut ctl, exec) = channel::<()>(Arc::new(()), ());
        let mut graph = GraphDescription::new();
        let mut sources = Vec::new();
        for &value in values {
            let c = Const {
                desc: NodeDescriptor::new(
                    NodeTypeId::new("org.rbs.test.const"),
                    NodeKind::Source,
                    vec![],
                    vec![Pin::one("out", LinkKind::Audio)],
                ),
                value,
            };
            let id = active(&mut graph, &c.desc);
            ctl.adopt(id, Box::new(c)).unwrap();
            sources.push(id);
        }
        let (mixer, tx, _) = MixerNode::new(config, 16, MixerParams::default());
        let mid = active(&mut graph, Node::<()>::descriptor(&mixer));
        let (main, mon, b0, b4) = (rec(), rec(), rec(), rec());
        let seen = [&main, &mon, &b0, &b4].map(|r| Arc::clone(&r.seen));
        let ids = [&main, &mon, &b0, &b4].map(|r| active(&mut graph, &r.desc));
        for &s in &sources {
            graph.connect(s, "out", mid, "in").unwrap();
        }
        graph.connect(mid, "main", ids[0], "in").unwrap();
        ctl.adopt(mid, Box::new(mixer)).unwrap();
        for (id, r) in ids.into_iter().zip([main, mon, b0, b4]) {
            ctl.adopt(id, Box::new(r)).unwrap();
        }
        let [main_seen, mon_seen, b0_seen, b4_seen] = seen;
        let mut rig = Self {
            ctl,
            exec,
            graph,
            sources: sources.clone(),
            mixer: mid,
            monitor: ids[1],
            buses: [ids[2], ids[3]],
            links: Vec::new(),
            params: tx,
            main: main_seen,
            mon: mon_seen,
            bus: [b0_seen, b4_seen],
            cadence: config.cadence(Timestamp::ZERO),
            tick: 5,
        };
        rig.params.send(params(&sources));
        rig.apply();
        rig
    }

    fn apply(&mut self) {
        self.ctl.apply(&self.graph).unwrap();
        self.exec.swap_pending();
        self.ctl.collect().unwrap().unwrap();
    }

    /// Links (or unlinks) pin `pin` of the mixer to its recorder.
    fn link(&mut self, pin: &'static str, on: bool) {
        if on {
            let to = match pin {
                "monitor" => self.monitor,
                p if p == BUS_PINS[SLOTS[0]] => self.buses[0],
                _ => self.buses[1],
            };
            let link = self.graph.connect(self.mixer, pin, to, "in").unwrap();
            self.links.push((pin, link));
        } else {
            let at = self.links.iter().position(|(p, _)| *p == pin).unwrap();
            let (_, link) = self.links.remove(at);
            assert!(self.graph.disconnect(link));
        }
        self.apply();
    }

    fn run(&mut self, n: u64) {
        for _ in 0..n {
            self.exec.run_tick(Tick::of(&self.cadence, self.tick));
            self.tick += 1;
        }
    }

    fn last(seen: &Seen) -> Option<Vec<f32>> {
        seen.lock().unwrap().last().cloned().flatten()
    }
}

fn all(v: &[f32], x: f32) -> bool {
    v.iter().all(|&y| (y - x).abs() < 1e-5)
}

/// Strips at fader 0.5: source 0 sends 1.0 to slot 0, source 1 sends 0.5 to slot 0 and
/// 1.0 to slot 4 and nothing to `main`.
fn sends(s: &[NodeId]) -> MixerParams {
    MixerParams::new(vec![
        Strip::new(s[0], 0.5).with_send(0, 1.0),
        Strip::new(s[1], 0.5)
            .with_send(0, 0.5)
            .with_send(4, 1.0)
            .with_main_send(0.0),
        Strip::new(s[2], 0.5),
    ])
    .with_master(2.0)
    .with_bus(4, 0.5, None)
}

#[test]
fn an_unread_bus_gives_nothing_and_main_takes_its_sends() {
    let mut r = Rig::new(&[0.1, 0.2, 0.4], sends);
    r.run(3);
    // Main: 2 x 0.5 x (0.1 + 0 + 0.4); no bus block.
    assert!(all(&Rig::last(&r.main).unwrap(), 0.5));
    assert_eq!(Rig::last(&r.bus[0]), None);
    assert_eq!(Rig::last(&r.bus[1]), None);
}

#[test]
fn a_read_bus_sums_post_fader_sends_after_its_own_fader() {
    let mut r = Rig::new(&[0.1, 0.2, 0.4], sends);
    r.run(2);
    r.link(BUS_PINS[0], true);
    r.link(BUS_PINS[4], true);
    r.run(3);
    // Slot 0: 0.5 x (0.1 x 1 + 0.2 x 0.5); slot 4: bus fader 0.5 x 0.5 x 0.2 x 1.
    let b0 = Rig::last(&r.bus[0]).unwrap();
    assert!(all(&b0, 0.1), "{}", b0[0]);
    let b4 = Rig::last(&r.bus[1]).unwrap();
    assert!(all(&b4, 0.05), "{}", b4[0]);
    // Main is unchanged by reading buses, and the master is not on a bus.
    assert!(all(&Rig::last(&r.main).unwrap(), 0.5));
}

#[test]
fn a_send_change_ramps_and_reading_a_bus_keeps_the_fader_ramp() {
    let mut r = Rig::new(&[0.4], |s| {
        MixerParams::new(vec![Strip::new(s[0], 0.5).with_send(0, 1.0)])
            .with_ramp(std::time::Duration::from_millis(20))
    });
    r.run(2);
    r.link(BUS_PINS[0], true);
    r.run(2);
    assert!(all(&Rig::last(&r.bus[0]).unwrap(), 0.2));
    let from = r.bus[0].lock().unwrap().len();
    r.params.send(
        MixerParams::new(vec![Strip::new(r.sources[0], 0.5).with_send(0, 0.0)])
            .with_ramp(std::time::Duration::from_millis(20)),
    );
    r.run(3);
    let blocks = r.bus[0].lock().unwrap()[from..].to_vec();
    let samples: Vec<f32> = blocks.into_iter().flat_map(|b| b.unwrap()).collect();
    // Two blocks of ramp, falling every frame, then silence: no step.
    assert!(samples.windows(2).take(2 * FRAMES - 1).all(|w| w[1] < w[0]));
    assert!(all(&samples[2 * FRAMES..], 0.0));
}

#[test]
fn only_leaves_every_bus_while_the_monitor_is_read() {
    let mut r = Rig::new(&[0.4], |s| {
        MixerParams::new(vec![
            Strip::new(s[0], 0.5)
                .with_send(0, 1.0)
                .with_monitor(MonitorMode::Only),
        ])
    });
    r.link(BUS_PINS[0], true);
    r.run(3);
    // Without a monitor, `Only` is `Off`: the strip is on the bus.
    assert!(all(&Rig::last(&r.bus[0]).unwrap(), 0.2));
    r.link("monitor", true);
    r.run(3);
    assert!(all(&Rig::last(&r.bus[0]).unwrap(), 0.0));
    assert!(all(&Rig::last(&r.mon).unwrap(), 0.2));
}

#[test]
fn a_bus_meter_reads_the_bus_after_its_fader() {
    let meter = Arc::new(LevelMeter::new(2));
    let m = Arc::clone(&meter);
    let mut r = Rig::new(&[0.4], move |s| {
        MixerParams::new(vec![Strip::new(s[0], 1.0).with_send(4, 1.0)]).with_bus(4, 0.5, Some(m))
    });
    r.link(BUS_PINS[4], true);
    r.run(3);
    let peak = meter.take().peak(0);
    assert!((peak - 0.2).abs() < 1e-5, "{peak}");
}

#[test]
fn reading_buses_does_not_allocate() {
    let mut r = Rig::new(&[0.1, 0.2, 0.4], sends);
    r.link(BUS_PINS[0], true);
    r.link(BUS_PINS[4], true);
    r.run(2);
    let reserve = |r: &Rig| {
        for seen in [&r.main, &r.bus[0], &r.bus[1]] {
            seen.lock().unwrap().reserve(100);
        }
    };
    reserve(&r);
    let base = allocation_counter::measure(|| r.run(10));
    // A send change taken up in `process`, ramping every bus send.
    let s = r.sources.clone();
    r.params.send(MixerParams::new(vec![
        Strip::new(s[0], 0.5).with_send(4, 1.0),
        Strip::new(s[1], 0.5).with_send(0, 1.0),
    ]));
    reserve(&r);
    let changed = allocation_counter::measure(|| r.run(10));
    assert_eq!(changed.count_total, base.count_total);
}

/// Every sample of `seen` from block `from` on, in order.
fn since(seen: &Seen, from: usize) -> Vec<f32> {
    seen.lock().unwrap()[from..]
        .iter()
        .flat_map(|b| b.clone().unwrap())
        .collect()
}

fn steps_smoothly(v: &[f32], max_step: f32) -> bool {
    v.windows(2).all(|w| (w[1] - w[0]).abs() <= max_step)
}

#[test]
fn a_fader_move_and_a_bus_gain_change_ramp_on_the_bus() {
    let ramp = std::time::Duration::from_millis(20);
    let mut r = Rig::new(&[0.4], |s| {
        MixerParams::new(vec![Strip::new(s[0], 1.0).with_send(0, 1.0)]).with_ramp(ramp)
    });
    r.link(BUS_PINS[0], true);
    r.run(2);
    let from = r.bus[0].lock().unwrap().len();
    // Fader 1 -> 0.5 and bus gain 1 -> 0.5 at once: 0.4 -> 0.1 over two blocks.
    let s = r.sources.clone();
    r.params.send(
        MixerParams::new(vec![Strip::new(s[0], 0.5).with_send(0, 1.0)])
            .with_ramp(ramp)
            .with_bus(0, 0.5, None),
    );
    r.run(3);
    let v = since(&r.bus[0], from);
    assert!(steps_smoothly(&v, 0.001), "a step in the bus");
    assert!(all(&v[2 * FRAMES..], 0.1), "{}", v[2 * FRAMES]);
}

#[test]
fn a_muted_strip_adds_nothing_to_a_bus() {
    let mut r = Rig::new(&[0.4, 0.2], |s| {
        MixerParams::new(vec![
            Strip::new(s[0], 1.0).with_send(0, 1.0).with_muted(true),
            Strip::new(s[1], 1.0).with_send(0, 1.0),
        ])
    });
    r.link(BUS_PINS[0], true);
    r.run(3);
    assert!(all(&Rig::last(&r.bus[0]).unwrap(), 0.2));
}

#[test]
fn unlinking_and_relinking_a_bus_mid_ramp_continues_the_ramp() {
    let ramp = std::time::Duration::from_millis(40);
    let mut r = Rig::new(&[0.4], |s| {
        MixerParams::new(vec![Strip::new(s[0], 1.0).with_send(0, 1.0)]).with_ramp(ramp)
    });
    r.link(BUS_PINS[0], true);
    r.run(2);
    let s = r.sources.clone();
    r.params
        .send(MixerParams::new(vec![Strip::new(s[0], 1.0).with_send(0, 0.0)]).with_ramp(ramp));
    r.run(1);
    r.link(BUS_PINS[0], false);
    r.run(1);
    r.link(BUS_PINS[0], true);
    let from = r.bus[0].lock().unwrap().len();
    r.run(3);
    let v = since(&r.bus[0], from);
    // Two of the four ramp blocks were run or skipped: the rest falls to 0 from about
    // half way, not from the start again.
    assert!(v[0] < 0.21 && v[0] > 0.19, "{}", v[0]);
    assert!(v.windows(2).take(2 * FRAMES - 1).all(|w| w[1] <= w[0]));
    assert!(all(&v[2 * FRAMES..], 0.0));
}
