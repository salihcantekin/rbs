//! The text source node on the render runtime (ADR-0129 decision 5, hot path, P15).
//!
//! - `steady/text` and `steady/still`: one render tick of a source and a probe with
//!   nothing new. The still source is the baseline that exists on main.
//! - `new_picture/1080p`: one render tick that uploads a new 1920 x 1080 picture
//!   (CPU side: the node, the budget and wgpu's staging copy; the GPU work is waited for
//!   outside the timed part).
//! - `twenty_clocks`: twenty 1080p pictures sent on the same tick; prints the ticks
//!   until all are uploaded and the slowest tick (the budget's spread).
//!
//! Needs a GPU adapter (software is fine). Run: `cargo bench -p rbs-nodes --bench text`.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)] // Benchmark harness.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{GpuRuntime, RenderLocal};
use rbs_gpu::{GpuContext, GpuRequest};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_nodes::{StillSourceNode, TextFrame, TextSourceNode};
use rbs_text::TextPicture;

struct Sink(NodeDescriptor);

impl Node<GpuRuntime> for Sink {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.0
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        std::hint::black_box(ctx.inputs().video(0, 0).map(|f| f.sequence()));
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
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
            cadence: Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60),
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
        let sink = Sink(NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.bench.sink"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        ));
        let s = self.graph.add_node(Arc::new(sink.0.clone())).unwrap();
        self.graph.set_state(s, Lifecycle::Standby).unwrap();
        self.graph.set_state(s, Lifecycle::Active).unwrap();
        self.ctl.adopt(s, Box::new(sink)).unwrap();
        self.graph.connect(id, "video", s, "in").unwrap();
        id
    }
    fn apply(&mut self) {
        assert!(self.ctl.apply(&self.graph).unwrap().failed.is_empty());
        self.exec.swap_pending();
        let _ = self.ctl.collect();
    }
    fn tick(&mut self) -> Duration {
        let t = Instant::now();
        self.exec.run_tick(Tick::of(&self.cadence, self.tick));
        let took = t.elapsed();
        self.tick += 1;
        self.ctx.wait_idle().unwrap();
        self.ctx.poll();
        let _ = self.ctl.collect();
        took
    }
}

fn frame(request: u64, w: u32, h: u32) -> TextFrame {
    let picture = TextPicture::new(w, h, vec![200; (w * h * 4) as usize]).unwrap();
    TextFrame::new(request, 1, Arc::new(picture)).unwrap()
}

fn bench(c: &mut Criterion) {
    let ctx = match GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) {
        Ok(ctx) => Arc::new(ctx),
        Err(e) => {
            eprintln!("SKIPPED: no GPU adapter ({e})");
            return;
        }
    };
    eprintln!("adapter: {}", ctx.adapter_name());

    let mut group = c.benchmark_group("steady");
    {
        let mut r = Rig::new(Arc::clone(&ctx));
        let (node, _tx) = TextSourceNode::new(Some(frame(1, 1920, 1080)));
        r.add(Box::new(node));
        r.apply();
        for _ in 0..4 {
            r.tick();
        }
        group.bench_function("text", |b| {
            b.iter(|| {
                r.exec.run_tick(Tick::of(&r.cadence, r.tick));
                r.tick += 1;
            })
        });
    }
    {
        let mut r = Rig::new(Arc::clone(&ctx));
        let still = StillSourceNode::new(1920, 1080, vec![200; 1920 * 1080 * 4]).unwrap();
        r.add(Box::new(still));
        r.apply();
        for _ in 0..4 {
            r.tick();
        }
        group.bench_function("still", |b| {
            b.iter(|| {
                r.exec.run_tick(Tick::of(&r.cadence, r.tick));
                r.tick += 1;
            })
        });
    }
    group.finish();

    let mut group = c.benchmark_group("new_picture");
    group.sample_size(20);
    {
        let mut r = Rig::new(Arc::clone(&ctx));
        let (node, mut tx) = TextSourceNode::new(Some(frame(1, 1920, 1080)));
        r.add(Box::new(node));
        r.apply();
        for _ in 0..4 {
            r.tick();
        }
        let pictures: Vec<TextFrame> = (0..2).map(|i| frame(10 + i, 1920, 1080)).collect();
        let mut n = 0u64;
        group.bench_function("1080p", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    n += 1;
                    let mut f = pictures[(n % 2) as usize].clone();
                    f.request = 100 + n;
                    drop(tx.recycle());
                    drop(tx.send(Some(f)));
                    total += r.tick();
                }
                total
            })
        });
    }
    group.finish();

    // Twenty full-HD clocks change on the same tick.
    let mut r = Rig::new(Arc::clone(&ctx));
    let mut nodes = Vec::new();
    for _ in 0..20 {
        let (node, tx) = TextSourceNode::new(Some(frame(1, 1920, 1080)));
        let stats = node.stats();
        r.add(Box::new(node));
        nodes.push((tx, stats));
    }
    r.apply();
    for _ in 0..4 {
        r.tick();
    }
    for (tx, _) in &mut nodes {
        drop(tx.send(Some(frame(2, 1920, 1080))));
    }
    let mut ticks = 0;
    let mut slowest = Duration::ZERO;
    while nodes
        .iter()
        .any(|(_, s)| s.uploaded_request.load(Ordering::Relaxed) != 2)
    {
        slowest = slowest.max(r.tick());
        ticks += 1;
        assert!(ticks < 40);
    }
    let waited: u64 = nodes
        .iter()
        .map(|(_, s)| s.budget_deferred.load(Ordering::Relaxed))
        .sum();
    eprintln!(
        "twenty_clocks: all uploaded in {ticks} ticks, slowest tick {slowest:?}, {waited} budget waits"
    );
}

criterion_group!(benches, bench);
criterion_main!(benches);
