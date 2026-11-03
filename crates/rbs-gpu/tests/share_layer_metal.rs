//! The preview as a Core Animation layer (spec `gpu-compositor.md`, "Preview export
//! (cross-process)", ADR-0034). Engine and UI run in this process; time is fake.
//!
//! The layers here are never in a window, so they run on the test's own thread; a layer
//! attached to a view must be used on the view's (main) thread.
#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::thread;
use std::time::Duration;

use common::solid;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_quartz_core::CALayer;
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_gpu::share::{
    DEFAULT_RETIRE_AFTER, LayerRect, PreviewDesc, PreviewEndpoint, PreviewFrame, PreviewLayer,
    PreviewPublisher, PreviewSubscriber,
};
use rbs_gpu::{
    CanvasDesc, Compositor, GpuContext, GpuError, Layer, TextureDesc, TextureId, TexturePool,
};

const CANVAS: u32 = 32;
/// 60 frames per second.
const FRAME: Duration = Duration::from_nanos(16_666_667);

/// The engine side: a canvas and a publisher with a ring of `surfaces`.
struct Engine {
    pool: TexturePool,
    comp: Compositor,
    canvas: TextureId,
    source: TextureId,
    publisher: PreviewPublisher,
}

impl Engine {
    /// `None` (skip) on devices without unified memory.
    fn new(ctx: &GpuContext, surfaces: usize) -> Option<Self> {
        let publisher = match PreviewPublisher::new(
            ctx,
            PreviewDesc {
                width: CANVAS,
                height: CANVAS,
                surfaces,
            },
        ) {
            Ok(p) => p,
            Err(GpuError::Unsupported(why)) => {
                eprintln!("SKIPPED: {why}");
                return None;
            }
            Err(e) => panic!("publisher failed: {e}"),
        };
        let mut pool = TexturePool::new();
        let comp = Compositor::new(
            ctx,
            CanvasDesc {
                width: CANVAS,
                height: CANVAS,
            },
            1,
        )
        .unwrap();
        let canvas = pool
            .acquire(ctx, &TextureDesc::canvas(CANVAS, CANVAS))
            .unwrap();
        let source = solid(ctx, &mut pool, 2, 2, [0.0, 0.5, 1.0, 1.0]);
        Some(Self {
            pool,
            comp,
            canvas,
            source,
            publisher,
        })
    }

    /// Renders and publishes frame `seq`; `false` when every surface was busy.
    fn publish(&mut self, ctx: &GpuContext, seq: u64) -> bool {
        let layers = [Layer::new(
            self.source,
            0.0,
            0.0,
            CANVAS as f32,
            CANVAS as f32,
        )];
        self.comp
            .render(ctx, &self.pool, &layers, self.canvas)
            .unwrap();
        let busy = match self.publisher.publish(
            ctx,
            &self.pool,
            self.canvas,
            Timestamp::from_nanos(seq),
            seq,
        ) {
            Ok(()) => false,
            Err(GpuError::ExportBusy { .. }) => true,
            Err(e) => panic!("publish failed: {e}"),
        };
        ctx.wait_idle().unwrap();
        self.publisher.tick(ctx).unwrap();
        !busy
    }

    /// Lets the engine see the UI's releases.
    fn held(&mut self, ctx: &GpuContext) -> usize {
        self.publisher.tick(ctx).unwrap();
        self.publisher.held()
    }

    /// Subscribes on another thread while this one ticks the publisher.
    fn subscribe(&mut self, ctx: &GpuContext) -> PreviewSubscriber {
        let endpoint: PreviewEndpoint = self.publisher.endpoint().clone();
        let handle =
            thread::spawn(move || PreviewSubscriber::connect(&endpoint, Duration::from_secs(5)));
        while !handle.is_finished() {
            self.publisher.tick(ctx).unwrap();
            thread::yield_now();
        }
        handle.join().unwrap().unwrap()
    }
}

/// The next frame (receive timeout only as a hang guard).
fn next(sub: &mut PreviewSubscriber) -> PreviewFrame {
    sub.recv(Duration::from_secs(1))
        .unwrap()
        .expect("a frame per successful publish")
}

/// IOSurface ID of a layer's contents.
fn contents_id(layer: &PreviewLayer) -> Option<u32> {
    // SAFETY: reading `contents` has no requirement beyond a live layer; it is unsafe
    // only because its type is not known to the compiler.
    let contents = unsafe { layer.layer().contents() }?;
    let object: &objc2::runtime::AnyObject = &contents;
    // SAFETY: the layer's contents are only ever set to IOSurfaces, which are toll-free
    // bridged with `IOSurfaceRef`.
    let surface = unsafe {
        &*(object as *const objc2::runtime::AnyObject).cast::<objc2_io_surface::IOSurfaceRef>()
    };
    Some(surface.id())
}

/// At 60 fps with the default retire time, a ring of `surfaces_needed` surfaces never
/// leaves the engine without a free one, the layer shows the engine's surface itself,
/// and replaced frames go back once their time has passed, not before.
#[test]
fn layer_shows_engine_surfaces_and_retires_by_time() {
    let Some(ctx) = common::gpu() else { return };
    // Probe the ring size the layer asks for with a throwaway engine of one size.
    let Some(mut probe) = Engine::new(ctx, 2) else {
        return;
    };
    let needed = PreviewLayer::new(&probe.subscribe(ctx)).surfaces_needed(FRAME);
    assert_eq!(
        needed, 6,
        "34 ms at 60 fps: 3 retiring + shown + written + in flight"
    );
    drop(probe);

    let mut engine = Engine::new(ctx, needed).unwrap();
    let mut sub = engine.subscribe(ctx);
    let ids: Vec<u32> = engine.publisher.surface_ids().collect();
    let clock = TestClock::new();
    let mut layer = PreviewLayer::new(&sub);
    assert_eq!(
        contents_id(&layer),
        None,
        "nothing shown before the first frame"
    );
    assert_eq!(layer.next_retire(), None);

    for seq in 1..=30 {
        clock.advance(FRAME);
        assert!(
            engine.publish(ctx, seq),
            "engine found no free surface at {seq}"
        );
        let frame = next(&mut sub);
        assert_eq!(frame.sequence(), seq);
        let id = ids[frame.slot()];
        layer.show(frame, clock.now());
        assert_eq!(
            contents_id(&layer),
            Some(id),
            "contents is the engine's surface"
        );
        assert!(layer.retiring() <= 3, "at most 34 ms of frames retire");
        assert_eq!(engine.held(ctx), 1 + layer.retiring());
        assert!(!layer.engine_may_starve());
    }

    // No new frame: replaced frames go back once their time has passed, not before.
    let waiting = layer.retire(clock.now());
    assert!(waiting > 0);
    let due = layer.next_retire().expect("a frame waits");
    clock.set(due - Duration::from_nanos(1));
    assert_eq!(
        layer.retire(clock.now()),
        waiting,
        "not before the retire time"
    );
    clock.set(clock.now() + DEFAULT_RETIRE_AFTER);
    assert_eq!(layer.retire(clock.now()), 0);
    assert_eq!(engine.held(ctx), 1, "only the shown frame is held");

    // Detaching retires the shown frame by the same rule.
    layer.detach(clock.now());
    assert_eq!(layer.retiring(), 1);
    assert_eq!(engine.held(ctx), 1);
    clock.advance(DEFAULT_RETIRE_AFTER);
    assert_eq!(layer.retire(clock.now()), 0);
    assert_eq!(engine.held(ctx), 0);
}

/// Jittered frame times with retirement at `next_retire` (what the UI's timer does):
/// the UI never holds more than the ring minus the engine's two.
#[test]
fn jittered_frames_retired_at_due_time_leave_the_engine_a_surface() {
    let Some(ctx) = common::gpu() else { return };
    let Some(mut probe) = Engine::new(ctx, 2) else {
        return;
    };
    let needed = PreviewLayer::new(&probe.subscribe(ctx)).surfaces_needed(FRAME);
    drop(probe);
    let mut engine = Engine::new(ctx, needed).unwrap();
    let mut sub = engine.subscribe(ctx);
    let clock = TestClock::new();
    let mut layer = PreviewLayer::new(&sub);
    // Intervals around 16.7 ms: early, late and a long gap.
    let gaps_ms = [16, 20, 12, 17, 25, 9, 16, 30, 16, 14, 19, 16];
    for (i, gap) in gaps_ms.iter().cycle().take(48).enumerate() {
        let target = clock.now() + Duration::from_millis(*gap);
        // The timer fires at each due time before the next frame arrives.
        while let Some(due) = layer.next_retire().filter(|d| *d <= target) {
            clock.set(due);
            layer.retire(clock.now());
        }
        clock.set(target);
        let seq = i as u64 + 1;
        assert!(
            engine.publish(ctx, seq),
            "engine found no free surface at {seq}"
        );
        layer.show(next(&mut sub), clock.now());
        assert!(
            layer.retiring() < needed - 2,
            "UI holds {} of {needed} at {seq}",
            1 + layer.retiring()
        );
        engine.held(ctx);
    }
}

/// A ring too small for the retire time: the UI is told to poll, and frames still go
/// back without a new one arriving.
#[test]
fn small_ring_reports_starvation_until_retired() {
    let Some(ctx) = common::gpu() else { return };
    let Some(mut engine) = Engine::new(ctx, 3) else {
        return;
    };
    let mut sub = engine.subscribe(ctx);
    let clock = TestClock::new();
    let mut layer = PreviewLayer::new(&sub);
    for seq in 1..=3 {
        assert!(engine.publish(ctx, seq));
        layer.show(next(&mut sub), clock.now());
    }
    assert_eq!(layer.retiring(), 2);
    assert!(layer.engine_may_starve());
    assert!(!engine.publish(ctx, 4), "every surface is held");
    clock.advance(DEFAULT_RETIRE_AFTER);
    assert_eq!(layer.retire(clock.now()), 0);
    assert!(!layer.engine_may_starve());
    assert_eq!(engine.held(ctx), 1, "the engine sees the releases");
    assert!(engine.publish(ctx, 5), "a surface is free again");
    drop(layer);
    drop(next(&mut sub));
    assert_eq!(
        engine.held(ctx),
        0,
        "dropping the layer gives every frame back"
    );
}

/// Placement converts the top-left rectangle for unflipped and flipped superlayers and
/// follows a change of the superlayer's height.
#[test]
fn rect_follows_the_superlayer_geometry() {
    let Some(ctx) = common::gpu() else { return };
    let Some(mut engine) = Engine::new(ctx, 3) else {
        return;
    };
    let sub = engine.subscribe(ctx);
    let mut layer = PreviewLayer::new(&sub);
    let rect = LayerRect {
        x: 10.0,
        y: 20.0,
        width: 30.0,
        height: 40.0,
    };
    let parent = CALayer::new();
    parent.setBounds(CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(100.0, 80.0),
    ));
    // Placed before attaching: applied again once attached.
    layer.set_rect(rect);
    layer.attach_to_layer(&parent);
    layer.set_rect(rect);
    let frame = layer.layer().frame();
    assert_eq!((frame.origin.x, frame.origin.y), (10.0, 80.0 - 20.0 - 40.0));
    assert_eq!((frame.size.width, frame.size.height), (30.0, 40.0));

    // Unchanged rectangle and geometry: nothing is applied.
    let moved = CGRect::new(CGPoint::new(1.0, 2.0), CGSize::new(3.0, 4.0));
    layer.layer().setFrame(moved);
    layer.set_rect(rect);
    assert_eq!(
        layer.layer().frame().origin.x,
        1.0,
        "set_rect re-applied an unchanged rect"
    );
    layer.layer().setFrame(frame);

    parent.setBounds(CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(100.0, 200.0),
    ));
    layer.set_rect(rect);
    assert_eq!(layer.layer().frame().origin.y, 200.0 - 20.0 - 40.0);

    parent.setGeometryFlipped(true);
    layer.set_rect(rect);
    assert_eq!(layer.layer().frame().origin.y, 20.0);
}
