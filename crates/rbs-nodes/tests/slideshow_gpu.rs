//! Slide show source node on the render runtime (ADR-0171 decisions 7 and 8, spec
//! `io-nodes.md`, "Slide show source node"). Runs on any GPU adapter, software included; skips when there
//! is none (set `RBS_REQUIRE_GPU` to fail instead).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};

use rbs_core::lifecycle::Lifecycle;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::SlideKind;
use rbs_gpu::node::{GpuRuntime, RenderLocal};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::ParamSender;
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_nodes::{SlideFrame, SlideshowNode, SlideshowShared};

/// Time, generation, width and height of each new frame a probe saw.
type Seen = Arc<Mutex<Vec<(Timestamp, u64, u32, u32)>>>;

/// What `Rig::slideshow` gives back.
type SlideRig = (
    NodeId,
    ParamSender<Option<SlideFrame>>,
    Arc<SlideshowShared>,
    Seen,
);

/// Keeps every new frame on its input.
struct Probe {
    desc: NodeDescriptor,
    frames: Seen,
    last: Option<(Timestamp, u64)>,
}

impl Node<GpuRuntime> for Probe {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        if let Some(frame) = ctx.inputs().video(0, 0) {
            let key = (frame.timestamp(), frame.sequence());
            if self.last != Some(key) {
                self.last = Some(key);
                let size = frame.format().size;
                self.frames
                    .lock()
                    .unwrap()
                    .push((key.0, key.1, size.width, size.height));
            }
        }
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

fn gpu() -> Option<Arc<GpuContext>> {
    // One device per test binary: software Vulkan drivers crash with several.
    static CTX: OnceLock<Option<Arc<GpuContext>>> = OnceLock::new();
    let ctx = CTX.get_or_init(|| {
        match GpuContext::new(&GpuRequest {
            allow_software: true,
            ..GpuRequest::default()
        }) {
            Ok(ctx) => Some(Arc::new(ctx)),
            Err(GpuError::NoAdapter) => None,
            Err(e) => panic!("{e}"),
        }
    });
    if ctx.is_none() {
        assert!(
            std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
            "RBS_REQUIRE_GPU is set but no adapter was found"
        );
        eprintln!("SKIPPED: no GPU adapter");
    }
    ctx.clone()
}

const SIZE: (u32, u32) = (16, 8);

fn pic(serial: u64, now: bool, cut: bool, value: u8) -> SlideFrame {
    let rgba = vec![value; (SIZE.0 * SIZE.1 * 4) as usize];
    SlideFrame::new(serial, now, cut, SIZE, Arc::new(rgba)).unwrap()
}

struct Rig {
    ctx: Arc<GpuContext>,
    ctl: Controller<GpuRuntime>,
    exec: Executor<GpuRuntime>,
    graph: GraphDescription,
    cadence: Cadence,
    tick: u64,
}

impl Rig {
    fn new(ctx: Arc<GpuContext>) -> Self {
        let (ctl, exec) = channel::<GpuRuntime>(Arc::clone(&ctx), RenderLocal::default());
        Self {
            ctx,
            ctl,
            exec,
            graph: GraphDescription::new(),
            cadence: Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30),
            tick: 0,
        }
    }

    fn add(&mut self, node: Box<dyn Node<GpuRuntime>>) -> NodeId {
        let id = self
            .graph
            .add_node(Arc::new(node.descriptor().clone()))
            .unwrap();
        self.graph.set_state(id, Lifecycle::Standby).unwrap();
        self.graph.set_state(id, Lifecycle::Active).unwrap();
        self.ctl.adopt(id, node).unwrap();
        id
    }

    fn slideshow(
        &mut self,
        kind: Option<SlideKind>,
        interval_ms: u64,
        transition_ms: u32,
    ) -> SlideRig {
        let shared = Arc::new(SlideshowShared::default());
        shared.interval_ms.store(interval_ms, Ordering::Relaxed);
        shared.transition_ms.store(transition_ms, Ordering::Relaxed);
        let (node, tx) = SlideshowNode::new(SIZE, kind, Arc::clone(&shared), None);
        let stats = Arc::clone(&shared);
        let id = self.add(Box::new(node));
        let frames = Arc::new(Mutex::new(Vec::new()));
        let probe = self.add(Box::new(Probe {
            desc: NodeDescriptor::new(
                NodeTypeId::new_static("org.rbs.test.probe"),
                NodeKind::Output,
                vec![Pin::one("in", LinkKind::Video)],
                vec![],
            ),
            frames: Arc::clone(&frames),
            last: None,
        }));
        self.graph.connect(id, "video", probe, "in").unwrap();
        (id, tx, stats, frames)
    }

    fn apply(&mut self) {
        let report = self.ctl.apply(&self.graph).unwrap();
        assert!(report.failed.is_empty(), "{report:?}");
        self.exec.swap_pending();
        let _ = self.ctl.collect();
    }

    fn ticks(&mut self, n: u64) {
        for _ in 0..n {
            self.exec.run_tick(Tick::of(&self.cadence, self.tick));
            self.tick += 1;
            self.ctx.wait_idle().unwrap();
            self.ctx.poll();
            let _ = self.ctl.collect();
        }
    }
}

fn uploaded(s: &SlideshowShared) -> u64 {
    s.stats.uploaded.load(Ordering::Relaxed)
}

fn frames(s: &SlideshowShared) -> u64 {
    s.stats.frames.load(Ordering::Relaxed)
}

#[test]
fn nothing_shows_before_the_first_picture() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx);
    let (_, _tx, s, seen) = rig.slideshow(Some(SlideKind::Fade), 1000, 300);
    rig.apply();
    rig.ticks(5);
    assert_eq!(frames(&s), 0);
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(s.shown.load(Ordering::Relaxed), 0);
}

#[test]
fn the_first_picture_shows_at_once_without_a_change() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx);
    let (_, mut tx, s, seen) = rig.slideshow(Some(SlideKind::Fade), 1000, 300);
    rig.apply();
    tx.send(Some(pic(1, false, true, 10)));
    rig.ticks(2);
    assert_eq!(frames(&s), 1);
    assert_eq!(s.shown.load(Ordering::Relaxed), 1);
    assert_eq!(s.stats.changes.load(Ordering::Relaxed), 0);
    assert_eq!(seen.lock().unwrap()[0].2, SIZE.0);
}

#[test]
fn the_next_picture_is_uploaded_ahead_and_shown_on_time_with_a_cut() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx);
    // 30 fps: 300 ms is 9 ticks.
    let (_, mut tx, s, _) = rig.slideshow(None, 300, 0);
    rig.apply();
    tx.send(Some(pic(1, false, true, 10)));
    rig.ticks(1);
    tx.send(Some(pic(2, false, false, 20)));
    rig.ticks(1);
    assert_eq!(uploaded(&s), 2, "uploaded ahead of its change");
    assert_eq!(s.shown.load(Ordering::Relaxed), 1);
    rig.ticks(6);
    assert_eq!(
        s.shown.load(Ordering::Relaxed),
        1,
        "not before the interval"
    );
    rig.ticks(3);
    assert_eq!(s.shown.load(Ordering::Relaxed), 2);
    assert_eq!(frames(&s), 2);
}

#[test]
fn a_fade_draws_every_tick_for_its_length_then_shows_the_picture() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx);
    // 200 ms change: the start tick, then draws until 200 ms have passed.
    let (_, mut tx, s, seen) = rig.slideshow(Some(SlideKind::Fade), 10_000, 200);
    rig.apply();
    tx.send(Some(pic(1, false, true, 10)));
    rig.ticks(1);
    tx.send(Some(pic(2, true, false, 20)));
    rig.ticks(1); // uploaded and the change starts
    assert_eq!(s.shown.load(Ordering::Relaxed), 2);
    rig.ticks(10);
    assert_eq!(s.stats.changes.load(Ordering::Relaxed), 1);
    let n = seen.lock().unwrap().len();
    // The first picture, six draws (0..200 ms at 33 ms), the final picture.
    assert!((7..=9).contains(&n), "{n} frames");
    assert_eq!(s.stats.draw_skipped.load(Ordering::Relaxed), 0);
}

#[test]
fn a_picture_sent_during_a_change_waits_for_its_end() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx);
    let (_, mut tx, s, _) = rig.slideshow(Some(SlideKind::Slide), 10_000, 300);
    rig.apply();
    tx.send(Some(pic(1, false, true, 10)));
    rig.ticks(1);
    tx.send(Some(pic(2, true, false, 20)));
    rig.ticks(2);
    tx.send(Some(pic(3, true, false, 30)));
    rig.ticks(3);
    assert_eq!(uploaded(&s), 2, "no upload while the change runs");
    rig.ticks(12);
    assert_eq!(uploaded(&s), 3);
    assert_eq!(s.shown.load(Ordering::Relaxed), 3);
}

#[test]
fn paused_and_manual_never_change_by_themselves() {
    let Some(ctx) = gpu() else { return };
    for (interval, paused) in [(100, true), (0, false)] {
        let mut rig = Rig::new(Arc::clone(&ctx));
        let (_, mut tx, s, _) = rig.slideshow(None, interval, 0);
        s.paused.store(paused, Ordering::Relaxed);
        rig.apply();
        tx.send(Some(pic(1, false, true, 10)));
        rig.ticks(1);
        tx.send(Some(pic(2, false, false, 20)));
        rig.ticks(20);
        assert_eq!(s.shown.load(Ordering::Relaxed), 1);
        // A command still shows at once.
        tx.send(Some(pic(3, true, false, 30)));
        rig.ticks(2);
        assert_eq!(s.shown.load(Ordering::Relaxed), 3);
    }
}

#[test]
fn the_change_length_is_capped_at_half_the_interval() {
    let s = SlideshowShared::default();
    s.transition_ms.store(5000, Ordering::Relaxed);
    s.interval_ms.store(2000, Ordering::Relaxed);
    assert_eq!(s.change_length(), std::time::Duration::from_millis(1000));
    s.interval_ms.store(0, Ordering::Relaxed);
    assert_eq!(s.change_length(), std::time::Duration::from_millis(5000));
}

#[test]
fn standby_gives_everything_back_and_warm_shows_the_picture_again() {
    let Some(ctx) = gpu() else { return };
    let mut rig = Rig::new(ctx);
    let (id, mut tx, s, _) = rig.slideshow(Some(SlideKind::Fade), 10_000, 200);
    rig.apply();
    tx.send(Some(pic(1, false, true, 10)));
    rig.ticks(2);
    rig.graph.set_state(id, Lifecycle::Standby).unwrap();
    rig.apply();
    rig.ticks(2);
    rig.graph.set_state(id, Lifecycle::Active).unwrap();
    rig.apply();
    rig.ticks(2);
    assert_eq!(uploaded(&s), 2);
    assert_eq!(frames(&s), 2);
    assert_eq!(s.stats.changes.load(Ordering::Relaxed), 0);
}

/// Allocations of 100 steady ticks of a graph of `node` and a probe.
fn steady_allocations(ctx: Arc<GpuContext>, node: Box<dyn Node<GpuRuntime>>) -> u64 {
    let mut rig = Rig::new(ctx);
    let id = rig.add(node);
    let probe = rig.add(Box::new(Probe {
        desc: NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.test.probe"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        ),
        frames: Arc::new(Mutex::new(Vec::new())),
        last: None,
    }));
    rig.graph.connect(id, "video", probe, "in").unwrap();
    rig.apply();
    rig.ticks(3);
    let start = rig.tick;
    allocation_counter::measure(|| {
        for i in start..start + 100 {
            rig.exec.run_tick(Tick::of(&rig.cadence, i));
        }
    })
    .count_total
}

/// Between changes a slide show adds no allocation to a tick (baseline: a still source).
#[test]
fn steady_slide_show_ticks_do_not_allocate() {
    let Some(ctx) = gpu() else { return };
    let still =
        rbs_nodes::StillSourceNode::new(SIZE.0, SIZE.1, vec![255; (SIZE.0 * SIZE.1 * 4) as usize])
            .unwrap();
    let baseline = steady_allocations(Arc::clone(&ctx), Box::new(still));
    let shared = Arc::new(SlideshowShared::default());
    shared.interval_ms.store(60_000, Ordering::Relaxed);
    let (node, _tx) = SlideshowNode::new(
        SIZE,
        Some(SlideKind::Fade),
        Arc::clone(&shared),
        Some(pic(1, false, true, 9)),
    );
    let with = steady_allocations(ctx, Box::new(node));
    assert_eq!(with, baseline, "the slide show node allocated");
    assert_eq!(uploaded(&shared), 1);
}
