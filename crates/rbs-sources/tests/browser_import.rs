//! Failed imports of shared-texture pages on the render runtime (ADR-0157 decisions 9
//! and 11): `BrowserSourceNode::process` counts each failed import in the page's
//! sink, a success ends the run, and the native branch allocates nothing. Off
//! Windows a Direct3D handle never imports, which is the failure these tests need, so
//! they do not build on Windows (a made-up handle value must never be opened there).
//! Runs on any GPU adapter, software included; skips when there is none (set
//! `RBS_REQUIRE_GPU` to fail instead).

#![cfg(not(windows))]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rbs_browser_ipc::Configure;
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{
    ColorSpace, D3dApi, Lease, LeaseOwner, NativeSurface, PixelFormat, Size, SurfaceSync,
    VideoFormat, VideoFrame,
};
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

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

/// Every test of this file holds this while it runs: `ALLOCS` counts the whole
/// process, so a test that measures allocations must not overlap another one.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    // A failed test poisons the lock; the next one still needs to run alone.
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// SAFETY: forwards every call unchanged to the system allocator; only counts calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's, forwarded to `System`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc` with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `ptr` came from `System.alloc` with this `layout`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

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

/// Counts the leases the studio holds, as the page's frame section does.
#[derive(Default)]
struct Slots(AtomicU32);

// SAFETY: `retain` and `release` only adjust a counter; no texture is behind the
// handle, and the test never reads one.
unsafe impl LeaseOwner for Slots {
    fn retain(&self, _token: u64) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn release(&self, _token: u64) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// An output that takes the page's picture, so the page is on air.
struct Out(NodeDescriptor);

impl Node<GpuRuntime> for Out {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.0
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, _: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

/// A picture on a Direct3D shared handle with one lease on `slots`.
fn native(slots: &Arc<Slots>) -> VideoFrame {
    slots.0.fetch_add(1, Ordering::Relaxed);
    // SAFETY: the lease releases the one reference counted just above.
    let lease = unsafe { Lease::adopt(Arc::clone(slots) as Arc<dyn LeaseOwner>, 0) };
    let format = VideoFormat::new(Size::new(W, H), PixelFormat::Bgra8, ColorSpace::SRGB);
    let surface = NativeSurface::d3d_shared_handle(0x40, true, D3dApi::D3d11, 1);
    // SAFETY: the handle is never opened off Windows (the import fails first), and no
    // GPU work reads it; the lease keeps the counter alive.
    unsafe { VideoFrame::native(format, Timestamp::ZERO, surface, SurfaceSync::Ready, lease) }
}

/// A page node running on the render runtime, and the sink its tree got.
fn running_page(
    ctx: &Arc<GpuContext>,
) -> (
    rbs_graph::runtime::Executor<GpuRuntime>,
    rbs_graph::runtime::Controller<GpuRuntime>,
    Arc<BrowserFrameSink>,
) {
    running_page_sized(ctx, W, H)
}

/// As [`running_page`], for a page of `width` x `height`.
fn running_page_sized(
    ctx: &Arc<GpuContext>,
    width: u32,
    height: u32,
) -> (
    rbs_graph::runtime::Executor<GpuRuntime>,
    rbs_graph::runtime::Controller<GpuRuntime>,
    Arc<BrowserFrameSink>,
) {
    let budget = BrowserBudget::shared();
    let source = SourceId::from_raw(1);
    budget.grant(&[BrowserClaim {
        source,
        class: ShowClass::Program,
    }]);
    let tree = Arc::new(Tree(Mutex::new(None)));
    let node = BrowserSourceNode::new(
        BrowserSettings {
            width,
            height,
            ..BrowserSettings::page("https://example.com/")
        },
        budget,
        source,
        Some(tree.clone()),
    );
    let (mut ctl, mut exec) = channel::<GpuRuntime>(Arc::clone(ctx), RenderLocal::default());
    let mut graph = GraphDescription::new();
    let page = graph.add_node(Arc::new(node.descriptor().clone())).unwrap();
    let probe = Out(NodeDescriptor::new(
        NodeTypeId::new_static("org.rbs.test.out"),
        NodeKind::Output,
        vec![Pin::one("in", LinkKind::Video)],
        vec![],
    ));
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
    (exec, ctl, sink)
}

#[test]
fn each_failed_import_counts_and_the_lease_is_given_back() {
    let _alone = serial();
    let Some(ctx) = gpu() else { return };
    let (mut exec, mut ctl, sink) = running_page(&ctx);
    let slots = Arc::new(Slots::default());
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
    for i in 0..10 {
        sink.publish_native(native(&slots), None);
        exec.run_tick(Tick::of(&cadence, i));
        let _ = ctl.collect();
        assert_eq!(sink.failed_imports(), i as u32 + 1, "tick {i}");
    }
    assert_eq!(slots.0.load(Ordering::Relaxed), 0, "every lease given back");
    // A tick with no new picture counts nothing.
    exec.run_tick(Tick::of(&cadence, 10));
    assert_eq!(sink.failed_imports(), 10);
    // A shared-memory picture is not an import and leaves the count alone.
    sink.publish(&[9; (W * H * 4) as usize], W, H);
    exec.run_tick(Tick::of(&cadence, 11));
    ctx.wait_idle().unwrap();
    assert_eq!(sink.failed_imports(), 10);
}

/// The native branch of `process`, counting included, allocates nothing: a tick that
/// takes and fails a native picture allocates as much as a tick with no picture
/// (CLAUDE.md rule 5, ADR-0157 decision 11).
#[test]
fn a_native_picture_adds_no_allocation_to_the_tick() {
    let _alone = serial();
    let Some(ctx) = gpu() else { return };
    let (mut exec, mut ctl, sink) = running_page(&ctx);
    let slots = Arc::new(Slots::default());
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
    // Warm up: the first ticks may size the runtime's buffers.
    for i in 0..4 {
        sink.publish_native(native(&slots), None);
        exec.run_tick(Tick::of(&cadence, i));
        let _ = ctl.collect();
    }
    let mut idle = Vec::new();
    let mut with_picture = Vec::new();
    for i in 4..24 {
        let picture = i % 2 == 0;
        if picture {
            sink.publish_native(native(&slots), None);
        }
        let before = ALLOCS.load(Ordering::Relaxed);
        exec.run_tick(Tick::of(&cadence, i));
        let took = ALLOCS.load(Ordering::Relaxed) - before;
        if picture {
            with_picture.push(took);
        } else {
            idle.push(took);
        }
        let _ = ctl.collect();
    }
    let most_idle = idle.iter().copied().max().unwrap();
    let most_picture = with_picture.iter().copied().max().unwrap();
    assert!(
        most_picture <= most_idle,
        "a native picture allocated: {with_picture:?} vs idle {idle:?}"
    );
    assert_eq!(sink.failed_imports(), 14);
}

/// A picture on a studio-owned IOSurface (macOS, ADR-0167) with one lease on `slots`.
#[cfg(target_os = "macos")]
fn io_surface(
    slots: &Arc<Slots>,
    surfaces: &rbs_gpu::browser_surfaces::PageSurfaces,
    slot: usize,
    w: u32,
    h: u32,
) -> VideoFrame {
    slots.0.fetch_add(1, Ordering::Relaxed);
    // SAFETY: the lease releases the one reference counted just above.
    let lease = unsafe { Lease::adopt(Arc::clone(slots) as Arc<dyn LeaseOwner>, slot as u64) };
    let format = VideoFormat::new(Size::new(w, h), PixelFormat::Bgra8, ColorSpace::SRGB);
    let surface = NativeSurface::io_surface(surfaces.surface(slot).unwrap());
    // SAFETY: the surface lives in `surfaces`, which outlives every tick of the test.
    unsafe { VideoFrame::native(format, Timestamp::ZERO, surface, SurfaceSync::Ready, lease) }
}

/// macOS GPU pages (ADR-0167 decision 10): page pictures on the studio's IOSurfaces
/// import on the render runtime with no failure, and every lease goes back once the
/// GPU no longer reads the surface.
#[cfg(target_os = "macos")]
#[test]
fn io_surface_pictures_import_and_give_every_lease_back() {
    let _alone = serial();
    let Some(ctx) = gpu() else { return };
    let (mut exec, mut ctl, sink) = running_page(&ctx);
    let slots = Arc::new(Slots::default());
    let surfaces = rbs_gpu::browser_surfaces::PageSurfaces::create(W, H, 4).unwrap();
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
    for i in 0..12u64 {
        sink.publish_native(io_surface(&slots, &surfaces, (i % 4) as usize, W, H), None);
        exec.run_tick(Tick::of(&cadence, i));
        let _ = ctl.collect();
    }
    assert_eq!(sink.failed_imports(), 0);
    // Only the picture on air still holds its lease; the others went back.
    ctx.wait_idle().unwrap();
    exec.run_tick(Tick::of(&cadence, 12));
    ctx.wait_idle().unwrap();
    exec.run_tick(Tick::of(&cadence, 13));
    assert!(
        slots.0.load(Ordering::Relaxed) <= 1,
        "leases held: {}",
        slots.0.load(Ordering::Relaxed)
    );
}

/// Before/after numbers for the hot path (ADR-0167 decision 10, P15): render ticks
/// that take a 1920x1080 page picture as bytes (the macOS path before, an upload per
/// picture) against one on a cached IOSurface (after). Ignored: it prints times.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "slow: prints tick times for the hot-path record"]
fn slow_tick_times_bytes_against_io_surface() {
    let Some(ctx) = gpu() else { return };
    let (w, h) = (1920u32, 1080u32);
    let run = |native: bool| -> (Vec<u128>, u64) {
        let (mut exec, mut ctl, sink) = running_page_sized(&ctx, w, h);
        let slots = Arc::new(Slots::default());
        let surfaces = rbs_gpu::browser_surfaces::PageSurfaces::create(w, h, 4).unwrap();
        let bytes = vec![128u8; (w * h * 4) as usize];
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_30);
        let mut times = Vec::new();
        let mut allocs = 0;
        for i in 0..340u64 {
            if native {
                sink.publish_native(io_surface(&slots, &surfaces, (i % 4) as usize, w, h), None);
            } else {
                sink.publish(&bytes, w, h);
            }
            let before = ALLOCS.load(Ordering::Relaxed);
            let t = std::time::Instant::now();
            exec.run_tick(Tick::of(&cadence, i));
            let took = ALLOCS.load(Ordering::Relaxed) - before;
            ctx.wait_idle().unwrap();
            if i >= 40 {
                times.push(t.elapsed().as_micros());
                allocs = allocs.max(took);
            }
            let _ = ctl.collect();
        }
        times.sort_unstable();
        (times, allocs)
    };
    for (name, native) in [("bytes (before)", false), ("IOSurface (after)", true)] {
        let (t, allocs) = run(native);
        let at = |q: f64| t[((t.len() - 1) as f64 * q) as usize];
        eprintln!(
            "{name}: tick incl. GPU wait p50 {} us, p99 {} us, max {} us; most allocations in a tick {allocs}",
            at(0.5),
            at(0.99),
            t[t.len() - 1]
        );
    }
}
