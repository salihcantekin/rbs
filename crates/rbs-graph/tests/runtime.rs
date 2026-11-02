//! Contract tests for the runtime: Node calls, swaps, warm-up, failures (ADR-0020,
//! media graph spec "Scheduling" and "Live editing").

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests and benches fail by panicking.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, PixelFormat, Size, TextureId,
    VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    ApplyError, Controller, Executor, Node, NodeError, OutputError, PER_TICK_EVENTS, PrepareCtx,
    ProcessCtx, Tick, WarmCtx, channel,
};

/// What happened to the probes, in order.
#[derive(Default)]
struct Log {
    events: Mutex<Vec<String>>,
}

impl Log {
    fn push(&self, s: String) {
        self.events.lock().unwrap().push(s);
    }
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    /// Emits a frame whose texture slot is the tick index.
    Source,
    /// Passes the first input frame through.
    Filter,
    /// Records the texture slots it sees on every link of input 0.
    Sink,
    /// Emits `n` packets per tick.
    Encoder(usize),
    /// Records the number of packets it sees.
    PacketSink,
}

struct Probe {
    name: &'static str,
    desc: NodeDescriptor,
    role: Role,
    log: Arc<Log>,
    fail_process: Arc<AtomicBool>,
    fail_prepare: bool,
    fail_warm: bool,
    panic_process: bool,
    /// Ticks processed by this instance; survives swaps.
    ticks: Arc<AtomicU32>,
}

fn fmt() -> VideoFormat {
    VideoFormat::new(Size::new(16, 16), PixelFormat::Bgra8, ColorSpace::SRGB)
}

fn descriptor(role: Role) -> NodeDescriptor {
    let (kind, inputs, outputs) = match role {
        Role::Source => (
            NodeKind::Source,
            vec![],
            vec![Pin::one("out", LinkKind::Video)],
        ),
        Role::Filter => (
            NodeKind::VideoFilter,
            vec![Pin::one("in", LinkKind::Video)],
            vec![Pin::one("out", LinkKind::Video)],
        ),
        Role::Sink => (
            NodeKind::Output,
            vec![Pin::many("in", LinkKind::Video)],
            vec![],
        ),
        Role::Encoder(_) => (
            NodeKind::Encoder,
            vec![Pin::one("in", LinkKind::Video)],
            vec![Pin::one("out", LinkKind::Packets)],
        ),
        Role::PacketSink => (
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Packets)],
            vec![],
        ),
    };
    NodeDescriptor::new(
        NodeTypeId::new(format!("org.rbs.test.{}", role_name(role))),
        kind,
        inputs,
        outputs,
    )
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Source => "source",
        Role::Filter => "filter",
        Role::Sink => "sink",
        Role::Encoder(_) => "encoder",
        Role::PacketSink => "packet-sink",
    }
}

impl Probe {
    fn new(name: &'static str, role: Role, log: &Arc<Log>) -> Self {
        Self {
            name,
            desc: descriptor(role),
            role,
            log: Arc::clone(log),
            fail_process: Arc::new(AtomicBool::new(false)),
            fail_prepare: false,
            fail_warm: false,
            panic_process: false,
            ticks: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl Node<()> for Probe {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        self.log.push(format!("prepare {}", self.name));
        if self.fail_prepare {
            return Err(NodeError::Failed("device busy".into()));
        }
        Ok(())
    }

    fn warm(&mut self, _ctx: &mut WarmCtx<'_, ()>) -> Result<(), NodeError> {
        self.log.push(format!("warm {}", self.name));
        if self.fail_warm {
            return Err(NodeError::Backend("no memory"));
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        if self.panic_process {
            panic!("probe panic");
        }
        if self.fail_process.load(Ordering::Relaxed) {
            return Err(NodeError::Backend("flaky"));
        }
        self.ticks.fetch_add(1, Ordering::Relaxed);
        let tick = ctx.tick();
        let inputs = ctx.inputs();
        match self.role {
            Role::Source => {
                let frame = VideoFrame::texture(
                    fmt(),
                    tick.time,
                    TextureId::new(tick.index as u32, 0),
                    None,
                );
                ctx.outputs().set_video(0, Some(frame))?;
            }
            Role::Filter => {
                let frame = inputs.video(0, 0).and_then(VideoFrame::share);
                ctx.outputs().set_video(0, frame)?;
            }
            Role::Sink => {
                let seen: Vec<String> = inputs
                    .videos(0)
                    .map(|f| match f {
                        Some(f) => match f.payload_texture() {
                            Some(t) => t.slot().to_string(),
                            None => "?".into(),
                        },
                        None => "-".into(),
                    })
                    .collect();
                self.log
                    .push(format!("{} saw [{}]", self.name, seen.join(",")));
            }
            Role::Encoder(n) => {
                if inputs.video(0, 0).is_some() {
                    for i in 0..n {
                        let packet = EncodedPacket::new(
                            CodecId::H264,
                            BitstreamFormat::AnnexB,
                            tick.time,
                            tick.time,
                            i == 0,
                            Bytes::from_static(b"x"),
                        )
                        .map_err(|_| NodeError::InvalidInput("dts"))?;
                        if let Err(r) = ctx.outputs().push_packet(0, packet) {
                            self.log
                                .push(format!("{} rejected {:?}", self.name, r.error));
                        }
                    }
                }
            }
            Role::PacketSink => {
                self.log
                    .push(format!("{} got {}", self.name, inputs.packets(0).count()));
            }
        }
        Ok(())
    }

    fn cool(&mut self, _ctx: &mut WarmCtx<'_, ()>) {
        self.log.push(format!("cool {}", self.name));
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, ()>) {
        self.log.push(format!("release {}", self.name));
    }
}

/// Texture slot of a frame, for assertions.
trait PayloadTexture {
    fn payload_texture(&self) -> Option<TextureId>;
}

impl PayloadTexture for VideoFrame {
    fn payload_texture(&self) -> Option<TextureId> {
        match self.payload() {
            rbs_core::media::FramePayload::Texture(t) => Some(*t),
            _ => None,
        }
    }
}

struct Rig {
    graph: GraphDescription,
    ctl: Controller<()>,
    exec: Executor<()>,
    log: Arc<Log>,
    cadence: Cadence,
    tick: u64,
}

impl Rig {
    fn new() -> Self {
        let (ctl, exec) = channel::<()>(Arc::new(()), ());
        Self {
            graph: GraphDescription::new(),
            ctl,
            exec,
            log: Arc::new(Log::default()),
            cadence: Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60),
            tick: 0,
        }
    }

    fn add(&mut self, probe: Probe) -> NodeId {
        let id = self
            .graph
            .add_node(Arc::new(probe.desc.clone()))
            .expect("valid descriptor");
        self.graph.set_state(id, Lifecycle::Standby).unwrap();
        self.ctl.adopt(id, Box::new(probe)).unwrap();
        id
    }

    fn node(&mut self, name: &'static str, role: Role) -> NodeId {
        let probe = Probe::new(name, role, &self.log);
        self.add(probe)
    }

    fn activate(&mut self, ids: &[NodeId]) {
        for &id in ids {
            self.graph.set_state(id, Lifecycle::Active).unwrap();
        }
    }

    fn link(&mut self, from: NodeId, to: NodeId) {
        self.graph.connect(from, "out", to, "in").unwrap();
    }

    fn tick(&mut self) {
        let tick = Tick::of(&self.cadence, self.tick);
        self.tick += 1;
        self.exec.run_tick(tick);
    }

    /// Applies the graph and runs one tick so the executor swaps, then collects.
    fn apply(&mut self) -> rbs_graph::runtime::ApplyReport {
        let report = self.ctl.apply(&self.graph).expect("apply");
        self.tick();
        self.ctl.collect().unwrap().expect("swapped");
        report
    }
}

#[test]
fn frames_flow_in_dependency_order_after_warm_up() {
    let mut rig = Rig::new();
    let out = rig.node("out", Role::Sink);
    let filter = rig.node("filter", Role::Filter);
    let src = rig.node("src", Role::Source);
    rig.link(src, filter);
    rig.link(filter, out);
    rig.activate(&[src, filter, out]);

    rig.ctl.apply(&rig.graph).unwrap();
    // Prepare runs on the control thread, in plan order, before anything is sent.
    assert_eq!(
        rig.log.take(),
        ["prepare src", "prepare filter", "prepare out"]
    );
    rig.tick(); // empty schedule runs, then the swap warms the new nodes
    assert_eq!(rig.log.take(), ["warm src", "warm filter", "warm out"]);
    rig.ctl.collect().unwrap().unwrap();

    rig.tick(); // tick 1
    rig.tick(); // tick 2
    assert_eq!(rig.log.take(), ["out saw [1]", "out saw [2]"]);
}

#[test]
fn starved_and_dead_end_nodes_never_run_and_are_not_prepared() {
    let mut rig = Rig::new();
    let src = rig.node("src", Role::Source);
    let lonely = rig.node("lonely", Role::Source); // no sink
    let filter = rig.node("filter", Role::Filter); // starved
    let out = rig.node("out", Role::Sink);
    rig.link(src, out);
    rig.graph.connect(filter, "out", out, "in").unwrap();
    rig.activate(&[src, lonely, filter, out]);
    rig.apply();
    let log = rig.log.take();
    assert!(
        !log.iter()
            .any(|l| l.contains("lonely") || l.contains("filter"))
    );
    rig.tick();
    // The skipped filter's link is not wired: one input link only.
    assert_eq!(rig.log.take(), ["out saw [1]"]);
}

#[test]
fn nodes_and_their_last_frame_survive_a_swap() {
    let mut rig = Rig::new();
    let probe = Probe::new("src", Role::Source, &rig.log);
    let ticks = Arc::clone(&probe.ticks);
    let stalled = Arc::clone(&probe.fail_process);
    let src = rig.add(probe);
    let out = rig.node("out", Role::Sink);
    rig.link(src, out);
    rig.activate(&[src, out]);
    rig.apply();
    rig.tick();
    rig.log.take();

    // Add a second output; src and out stay, so they are not prepared or warmed again.
    let second = rig.node("second", Role::Sink);
    rig.link(src, second);
    rig.activate(&[second]);
    rig.ctl.apply(&rig.graph).unwrap();
    assert_eq!(rig.log.take(), ["prepare second"]);
    rig.tick(); // tick 2 on the old schedule, then swap
    assert_eq!(rig.log.take(), ["out saw [2]", "warm second"]);
    assert_eq!(rig.ctl.collect().unwrap().unwrap(), []);
    rig.tick();
    assert_eq!(rig.log.take(), ["out saw [3]", "second saw [3]"]);
    // The same instance kept running: it counted every tick.
    assert_eq!(ticks.load(Ordering::Relaxed), 3);

    // A producer that has no new frame at the swap: its last frame moves with it.
    stalled.store(true, Ordering::Relaxed);
    rig.tick(); // 4: src fails, frame 3 stays
    rig.graph.set_state(second, Lifecycle::Standby).unwrap();
    rig.apply(); // tick 5, then swap
    rig.log.take();
    rig.tick();
    assert_eq!(rig.log.take(), ["out saw [3]"]);
}

#[test]
fn video_slot_keeps_the_last_good_frame_while_a_producer_fails() {
    let mut rig = Rig::new();
    let probe = Probe::new("src", Role::Source, &rig.log);
    let flaky = Arc::clone(&probe.fail_process);
    let src = rig.add(probe);
    let out = rig.node("out", Role::Sink);
    rig.link(src, out);
    rig.activate(&[src, out]);
    rig.apply();
    rig.exec.set_failure_limit(3);
    rig.tick(); // 1
    flaky.store(true, Ordering::Relaxed);
    rig.tick(); // 2 fails
    rig.tick(); // 3 fails
    rig.log.take();
    let health = rig.exec.health(src).unwrap();
    assert_eq!((health.errors, health.consecutive_errors), (2, 2));
    assert!(!health.bypassed);
    rig.tick(); // 4 fails: bypassed
    assert!(rig.exec.health(src).unwrap().bypassed);
    flaky.store(false, Ordering::Relaxed);
    rig.tick(); // bypassed: not called again, frame 1 stays
    assert_eq!(rig.log.take(), ["out saw [1]", "out saw [1]"]);
}

#[test]
fn a_panic_is_caught_and_bypasses_only_that_node() {
    let mut rig = Rig::new();
    let mut probe = Probe::new("bad", Role::Source, &rig.log);
    probe.panic_process = true;
    let bad = rig.add(probe);
    let good = rig.node("good", Role::Source);
    let out = rig.node("out", Role::Sink);
    rig.link(bad, out);
    rig.link(good, out);
    rig.activate(&[bad, good, out]);
    rig.apply();
    rig.log.take();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    rig.tick();
    std::panic::set_hook(hook);
    let health = rig.exec.health(bad).unwrap();
    assert!(health.panicked && health.bypassed);
    assert_eq!(rig.log.take(), ["out saw [-,1]"]);
}

#[test]
fn failed_prepare_leaves_the_node_idle_and_replans_without_it() {
    let mut rig = Rig::new();
    let mut probe = Probe::new("cam", Role::Source, &rig.log);
    probe.fail_prepare = true;
    let cam = rig.add(probe);
    let filter = rig.node("filter", Role::Filter);
    let out = rig.node("out", Role::Sink);
    let src = rig.node("src", Role::Source);
    rig.link(cam, filter);
    rig.link(filter, out);
    rig.link(src, out);
    rig.activate(&[cam, filter, out, src]);
    let report = rig.apply();
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, cam);
    // The filter lost its only producer, so it is not scheduled; it was prepared in
    // the first round and released again.
    assert!(!report.plan.order.contains(&filter));
    assert!(rig.ctl.is_idle(cam) && rig.ctl.is_idle(filter));
    let log = rig.log.take();
    assert!(log.contains(&"release cam".to_string()));
    assert!(log.contains(&"release filter".to_string()));
    // The description still wants cam Active; only the effective state changed.
    assert_eq!(rig.graph.node(cam).unwrap().state, Lifecycle::Active);
}

#[test]
fn failed_warm_bypasses_and_cools_the_node() {
    let mut rig = Rig::new();
    let mut probe = Probe::new("src", Role::Source, &rig.log);
    probe.fail_warm = true;
    let src = rig.add(probe);
    let out = rig.node("out", Role::Sink);
    rig.link(src, out);
    rig.activate(&[src, out]);
    rig.apply();
    let log = rig.log.take();
    assert!(log.ends_with(&["warm src".into(), "cool src".into(), "warm out".into()]));
    let health = rig.exec.health(src).unwrap();
    assert!(health.warm_failed && health.bypassed);
    rig.tick();
    assert_eq!(rig.log.take(), ["out saw [-]"]);
}

#[test]
fn demoted_nodes_are_cooled_released_and_come_back_with_warm_up() {
    let mut rig = Rig::new();
    let src = rig.node("src", Role::Source);
    let out = rig.node("out", Role::Sink);
    rig.link(src, out);
    rig.activate(&[src, out]);
    rig.apply();
    rig.log.take();

    // The output goes away (for example the preview is closed): src has no sink.
    rig.graph.set_state(out, Lifecycle::Standby).unwrap();
    rig.ctl.apply(&rig.graph).unwrap();
    rig.tick();
    let left = rig.ctl.collect().unwrap().unwrap();
    let ids: Vec<NodeId> = left.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, [src, out]);
    assert_eq!(
        rig.log.take(),
        [
            "out saw [1]",
            "cool src",
            "cool out",
            "release src",
            "release out"
        ]
    );
    assert!(rig.ctl.is_idle(src));

    // Back on: both are prepared and warmed again before they run.
    rig.graph.set_state(out, Lifecycle::Active).unwrap();
    rig.apply();
    assert_eq!(
        rig.log.take(),
        ["prepare src", "prepare out", "warm src", "warm out"]
    );
    rig.tick();
    assert_eq!(rig.log.take(), ["out saw [3]"]);
}

#[test]
fn a_second_apply_waits_for_the_swap() {
    let mut rig = Rig::new();
    let src = rig.node("src", Role::Source);
    let out = rig.node("out", Role::Sink);
    rig.link(src, out);
    rig.activate(&[src, out]);
    rig.ctl.apply(&rig.graph).unwrap();
    assert_eq!(rig.ctl.apply(&rig.graph), Err(ApplyError::Busy));
    assert_eq!(rig.ctl.collect().unwrap(), None);
    rig.tick();
    assert!(rig.ctl.collect().unwrap().is_some());
    assert!(rig.ctl.apply(&rig.graph).is_ok());
}

#[test]
fn running_nodes_cannot_be_taken_or_adopted_twice() {
    let mut rig = Rig::new();
    let src = rig.node("src", Role::Source);
    let out = rig.node("out", Role::Sink);
    rig.link(src, out);
    rig.activate(&[src, out]);
    rig.apply();
    assert!(rig.ctl.take(src).is_err());
    let again = Probe::new("again", Role::Source, &rig.log);
    assert!(rig.ctl.adopt(src, Box::new(again)).is_err());
}

#[test]
fn missing_instance_and_wrong_descriptor_are_reported() {
    let mut rig = Rig::new();
    let out = rig.node("out", Role::Sink);
    let ghost = rig
        .graph
        .add_node(Arc::new(descriptor(Role::Source)))
        .unwrap();
    rig.graph.set_state(ghost, Lifecycle::Standby).unwrap();
    let wrong = rig
        .graph
        .add_node(Arc::new(descriptor(Role::Source)))
        .unwrap();
    rig.graph.set_state(wrong, Lifecycle::Standby).unwrap();
    rig.ctl
        .adopt(wrong, Box::new(Probe::new("wrong", Role::Filter, &rig.log)))
        .unwrap();
    let src = rig.node("src", Role::Source);
    rig.link(ghost, out);
    rig.link(wrong, out);
    rig.link(src, out);
    rig.activate(&[ghost, wrong, out, src]);
    let report = rig.apply();
    assert_eq!(
        report.failed,
        [
            (ghost, NodeError::NoInstance),
            (wrong, NodeError::DescriptorMismatch)
        ]
    );
    rig.tick();
    assert!(rig.log.take().ends_with(&["out saw [1]".to_string()]));
}

#[test]
fn packets_are_per_tick_events_with_a_bounded_buffer() {
    let mut rig = Rig::new();
    let src = rig.node("src", Role::Source);
    let enc = rig.node("enc", Role::Encoder(PER_TICK_EVENTS + 1));
    let sink = rig.node("sink", Role::PacketSink);
    rig.link(src, enc);
    rig.link(enc, sink);
    rig.activate(&[src, enc, sink]);
    rig.apply();
    rig.log.take();
    rig.tick();
    rig.tick();
    let full = format!("enc rejected {:?}", OutputError::Full(0));
    let got = format!("sink got {PER_TICK_EVENTS}");
    // Packets do not accumulate across ticks.
    assert_eq!(rig.log.take(), [full.clone(), got.clone(), full, got]);
}

#[test]
fn many_input_links_keep_connection_order() {
    let mut rig = Rig::new();
    let a = rig.node("a", Role::Source);
    let b = rig.node("b", Role::Filter);
    let c = rig.node("c", Role::Source);
    let out = rig.node("out", Role::Sink);
    rig.link(c, out);
    rig.link(a, b);
    rig.link(b, out);
    rig.link(a, out);
    rig.activate(&[a, b, c, out]);
    rig.apply();
    rig.log.take();
    rig.tick();
    assert_eq!(rig.log.take(), ["out saw [1,1,1]"]);
    // Clearing one producer's frame shows as a gap at its position.
    rig.graph.set_state(c, Lifecycle::Standby).unwrap();
    rig.apply();
    rig.log.take();
    rig.tick();
    assert_eq!(rig.log.take(), ["out saw [3,3]"]);
}

#[test]
fn audio_links_are_refused_without_preparing_anything() {
    let mut rig = Rig::new();
    let mic = rig
        .graph
        .add_node(Arc::new(NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.mic"),
            NodeKind::Source,
            vec![],
            vec![Pin::one("out", LinkKind::Audio)],
        )))
        .unwrap();
    let mon = rig
        .graph
        .add_node(Arc::new(NodeDescriptor::new(
            NodeTypeId::new("org.rbs.test.monitor"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Audio)],
            vec![],
        )))
        .unwrap();
    rig.graph.connect(mic, "out", mon, "in").unwrap();
    for id in [mic, mon] {
        rig.graph.set_state(id, Lifecycle::Standby).unwrap();
        rig.graph.set_state(id, Lifecycle::Active).unwrap();
    }
    assert!(matches!(
        rig.ctl.apply(&rig.graph),
        Err(ApplyError::UnsupportedLink(_))
    ));
    assert!(!rig.ctl.in_flight());
    assert!(rig.log.take().is_empty());
}
