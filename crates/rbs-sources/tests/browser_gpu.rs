//! The browser picture node on the render runtime (ADR-0109 decision 8): every page
//! picture reaches the output, none before its stamp. Runs on any GPU adapter,
//! software included; skips when there is none (set `RBS_REQUIRE_GPU` to fail instead).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::{Arc, Mutex};

use rbs_browser_ipc::Configure;
use rbs_core::lifecycle::Lifecycle;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_gpu::node::{GpuRuntime, RenderLocal};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};
use rbs_graph::description::GraphDescription;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel};
use rbs_scene::model::SourceId;
use rbs_sources::{
    BrowserBudget, BrowserClaim, BrowserFrameSink, BrowserSession, BrowserSettings,
    BrowserSourceNode, BrowserTree, ShowClass,
};

const W: u32 = 4;
const H: u32 = 2;

fn gpu() -> Option<Arc<GpuContext>> {
    match GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) {
        Ok(ctx) => Some(Arc::new(ctx)),
        Err(GpuError::NoAdapter) => {
            assert!(
                std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
                "RBS_REQUIRE_GPU is set but no adapter was found"
            );
            eprintln!("SKIPPED: no GPU adapter");
            None
        }
        Err(e) => panic!("{e}"),
    }
}

/// Hands the sink to the test, which plays the supervisor.
struct Tree(Mutex<Option<Arc<BrowserFrameSink>>>);

struct Session;

impl BrowserSession for Session {
    fn poll(&mut self) -> Option<i32> {
        None
    }
    fn stop(&mut self) {}
}

impl BrowserTree for Tree {
    fn start(
        &self,
        _config: &Configure,
        sink: Arc<BrowserFrameSink>,
        _sound: Option<Arc<rbs_sources::BrowserSound>>,
    ) -> Result<Box<dyn BrowserSession>, String> {
        *self.0.lock().unwrap() = Some(sink);
        Ok(Box::new(Session))
    }
}

/// Records the timestamp of every new picture on its input, with the tick it came in.
struct Probe {
    desc: NodeDescriptor,
    seen: Arc<Mutex<Vec<(u64, Timestamp)>>>,
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
        let index = ctx.tick().index;
        if let Some(frame) = ctx.inputs().video(0, 0) {
            let key = (frame.timestamp(), frame.sequence());
            if self.last != Some(key) {
                self.last = Some(key);
                self.seen.lock().unwrap().push((index, frame.timestamp()));
            }
        }
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

#[test]
fn every_page_picture_goes_on_air_at_its_stamp() {
    let Some(ctx) = gpu() else { return };
    let budget = BrowserBudget::shared();
    let source = SourceId::from_raw(1);
    budget.grant(&[BrowserClaim {
        source,
        class: ShowClass::Program,
    }]);
    let tree = Arc::new(Tree(Mutex::new(None)));
    let node = BrowserSourceNode::new(
        BrowserSettings {
            url: "https://example.com/".into(),
            width: W,
            height: H,
            fps: 30,
            transparent: false,
        },
        budget,
        source,
        Some(tree.clone()),
    );
    let (mut ctl, mut exec) = channel::<GpuRuntime>(Arc::clone(&ctx), RenderLocal::default());
    let mut graph = GraphDescription::new();
    let page = graph.add_node(Arc::new(node.descriptor().clone())).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let probe = Probe {
        desc: NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.test.probe"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        ),
        seen: Arc::clone(&seen),
        last: None,
    };
    let out = graph
        .add_node(Arc::new(probe.descriptor().clone()))
        .unwrap();
    for (id, node) in [
        (page, Box::new(node) as Box<dyn Node<GpuRuntime>>),
        (out, Box::new(probe)),
    ] {
        graph.set_state(id, Lifecycle::Standby).unwrap();
        graph.set_state(id, Lifecycle::Active).unwrap();
        ctl.adopt(id, node).unwrap();
    }
    graph.connect(page, "video", out, "in").unwrap();
    let report = ctl.apply(&graph).unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    exec.swap_pending();
    let _ = ctl.collect();
    let sink = tree.0.lock().unwrap().clone().expect("the helper started");

    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
    let at = |i| Tick::of(&cadence, i).time;
    let rgba = [200u8; (W * H * 4) as usize];
    for i in 0..12 {
        // The page paints a picture stamped at ticks 2, 5 and 9, each published two
        // ticks early (a stamp the edge gives is never in the future; this checks
        // the render side alone).
        if let Some(stamp) = [2, 5, 9].into_iter().find(|s| s - 2 == i) {
            sink.publish_at(&rgba, W, H, Some(at(stamp)));
        }
        exec.run_tick(Tick::of(&cadence, i));
        ctx.wait_idle().unwrap();
        ctx.poll();
        let _ = ctl.collect();
    }
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![(2, at(2)), (5, at(5)), (9, at(9))],
        "each picture on air at its tick, none frozen or early"
    );
    assert_eq!(exec.health(page).unwrap().errors, 0);
}
