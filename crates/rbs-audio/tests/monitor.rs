//! The mixer's monitor output: monitor modes, solo, the fader and mute on the monitor,
//! no master on the monitor, `Only` degrading to `Off` while the monitor is not read,
//! and ramp state surviving the link swap (audio graph spec "Mixer", ADR-0097
//! decisions 5 and 6).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_audio::{AudioGraphConfig, MixerNode, MixerParams, MonitorMode, Strip};
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

/// Sources of constant values into a mixer; `main` and `monitor` recorders.
struct Rig {
    ctl: Controller<()>,
    exec: Executor<()>,
    graph: GraphDescription,
    sources: Vec<NodeId>,
    mixer: NodeId,
    monitor: NodeId,
    link: Option<rbs_graph::description::Link>,
    params: ParamSender<MixerParams>,
    main: Seen,
    mon: Seen,
    cadence: Cadence,
    tick: u64,
}

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
        let (main, mon) = (rec(), rec());
        let (main_seen, mon_seen) = (Arc::clone(&main.seen), Arc::clone(&mon.seen));
        let a = active(&mut graph, &main.desc);
        let b = active(&mut graph, &mon.desc);
        for &s in &sources {
            graph.connect(s, "out", mid, "in").unwrap();
        }
        graph.connect(mid, "main", a, "in").unwrap();
        ctl.adopt(mid, Box::new(mixer)).unwrap();
        ctl.adopt(a, Box::new(main)).unwrap();
        ctl.adopt(b, Box::new(mon)).unwrap();
        let mut rig = Self {
            ctl,
            exec,
            graph,
            sources: sources.clone(),
            mixer: mid,
            monitor: b,
            link: None,
            params: tx,
            main: main_seen,
            mon: mon_seen,
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

    fn link_monitor(&mut self, on: bool) {
        if on {
            let link = self
                .graph
                .connect(self.mixer, "monitor", self.monitor, "in")
                .unwrap();
            self.link = Some(link);
        } else {
            assert!(self.graph.disconnect(self.link.take().unwrap()));
        }
        self.apply();
    }

    fn run(&mut self, n: u64) {
        for _ in 0..n {
            self.exec.run_tick(Tick::of(&self.cadence, self.tick));
            self.tick += 1;
        }
    }

    fn last_main(&self) -> Vec<f32> {
        self.main.lock().unwrap().last().cloned().flatten().unwrap()
    }

    fn last_mon(&self) -> Option<Vec<f32>> {
        self.mon.lock().unwrap().last().cloned().flatten()
    }

    /// Every main sample since block `from`, in order.
    fn main_since(&self, from: usize) -> Vec<f32> {
        self.main.lock().unwrap()[from..]
            .iter()
            .flat_map(|b| b.clone().unwrap())
            .collect()
    }
}

fn all(v: &[f32], x: f32) -> bool {
    v.iter().all(|&y| (y - x).abs() < 1e-5)
}

fn modes(s: &[NodeId], m: &[MonitorMode], master: f32) -> MixerParams {
    MixerParams::new(
        s.iter()
            .zip(m)
            .map(|(&id, &mode)| Strip::new(id, 0.5).with_monitor(mode))
            .collect(),
    )
    .with_master(master)
}

#[test]
fn an_unread_monitor_does_nothing_and_only_stays_on_the_mix() {
    use MonitorMode::*;
    let mut r = Rig::new(&[0.1, 0.2, 0.4], |s| modes(s, &[Off, Only, Also], 2.0));
    r.run(3);
    // Every strip on the mix: 2 x 0.5 x (0.1 + 0.2 + 0.4).
    assert!(all(&r.last_main(), 0.7));
    assert_eq!(r.last_mon(), None);
}

#[test]
fn the_monitor_hears_only_and_also_after_the_fader_without_the_master() {
    use MonitorMode::*;
    let mut r = Rig::new(&[0.1, 0.2, 0.4], |s| modes(s, &[Off, Only, Also], 2.0));
    r.run(3);
    r.link_monitor(true);
    r.run(3);
    // `Only` left the mix: 2 x 0.5 x (0.1 + 0.4); the monitor has 0.5 x (0.2 + 0.4).
    assert!(all(&r.last_main(), 0.5), "{}", r.last_main()[0]);
    assert!(all(&r.last_mon().unwrap(), 0.3));
}

#[test]
fn only_leaves_the_mix_by_a_ramp_and_returns_when_the_monitor_goes() {
    use MonitorMode::*;
    let mut r = Rig::new(&[0.2], |s| {
        modes(s, &[Only], 1.0).with_ramp(Duration::from_millis(20))
    });
    r.run(3);
    let from = r.main.lock().unwrap().len();
    r.link_monitor(true);
    r.run(3);
    r.link_monitor(false);
    let unlinked = r.mon.lock().unwrap().len();
    r.run(3);
    let main = r.main_since(from);
    // 0.1 on the mix, ramping to 0 over 20 ms and back.
    assert!((main[479] - 0.05).abs() < 1e-4, "{}", main[479]);
    assert!(all(&main[960..1440], 0.0));
    assert!(all(&main[main.len() - 480..], 0.1));
    let worst = main
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max);
    assert!(worst <= 0.1 / 960.0 + 1e-6, "{worst}");
    // Nothing reaches the monitor once it is unlinked.
    assert!(
        r.mon.lock().unwrap()[unlinked..]
            .iter()
            .all(Option::is_none)
    );
}

#[test]
fn solo_plays_only_soloed_strips_on_the_monitor_and_leaves_the_mix() {
    use MonitorMode::*;
    let mut r = Rig::new(&[0.1, 0.2, 0.4], |s| {
        MixerParams::new(vec![
            Strip::new(s[0], 1.0).with_monitor(Also),
            Strip::new(s[1], 1.0).with_monitor(Also).with_solo(true),
            Strip::new(s[2], 1.0).with_solo(true),
        ])
    });
    r.link_monitor(true);
    r.run(3);
    // Soloed strips, even one that is Off; the mix has every strip.
    assert!(all(&r.last_mon().unwrap(), 0.6));
    assert!(all(&r.last_main(), 0.7));
}

#[test]
fn a_muted_strip_is_silent_on_the_monitor_too() {
    let mut r = Rig::new(&[0.4], |s| {
        MixerParams::new(vec![
            Strip::new(s[0], 1.0)
                .with_monitor(MonitorMode::Also)
                .with_muted(true),
        ])
    });
    r.link_monitor(true);
    r.run(3);
    assert!(all(&r.last_mon().unwrap(), 0.0));
}

#[test]
fn linking_the_monitor_keeps_the_mixer_and_its_ramps() {
    let mut r = Rig::new(&[0.4], |s| {
        MixerParams::new(vec![Strip::new(s[0], 1.0).with_monitor(MonitorMode::Also)])
    });
    r.run(3);
    let s = r.sources[0];
    // A slow fade in progress across the swap.
    r.params.send(
        MixerParams::new(vec![Strip::new(s, 0.0).with_monitor(MonitorMode::Also)])
            .with_ramp(Duration::from_millis(100)),
    );
    r.run(2);
    let from = r.main.lock().unwrap().len();
    r.link_monitor(true);
    r.run(3);
    let main = r.main_since(from - 1);
    // A re-created mixer would restart the strip (fade in from 0, or jump to the
    // target); the kept one continues the 100 ms ramp, one small step per frame.
    let step = 0.4 / 4800.0;
    for w in main.windows(2) {
        assert!(((w[0] - w[1]) - step).abs() < 1e-6, "{} -> {}", w[0], w[1]);
    }
    // The monitor fades in (its send ramps from 0) and never exceeds the fader.
    let mon: Vec<f32> = r
        .mon
        .lock()
        .unwrap()
        .iter()
        .flatten()
        .flatten()
        .copied()
        .collect();
    assert!(mon[0] < 0.001, "{}", mon[0]);
    let tail = &main[main.len() - mon.len()..];
    assert!(mon.iter().zip(tail).all(|(m, x)| *m <= *x + 1e-6));
}

#[test]
fn monitoring_does_not_allocate() {
    use MonitorMode::*;
    let mut r = Rig::new(&[0.1, 0.2, 0.4], |s| modes(s, &[Off, Only, Also], 1.0));
    r.link_monitor(true);
    r.run(2);
    r.main.lock().unwrap().reserve(100);
    r.mon.lock().unwrap().reserve(100);
    let base = allocation_counter::measure(|| r.run(10));
    // A mode change taken up in `process`, ramping both sends.
    let s = r.sources.clone();
    r.params.send(modes(&s, &[Also, Off, Only], 1.0));
    r.main.lock().unwrap().reserve(100);
    r.mon.lock().unwrap().reserve(100);
    let changed = allocation_counter::measure(|| r.run(10));
    assert_eq!(changed.count_total, base.count_total);
}
