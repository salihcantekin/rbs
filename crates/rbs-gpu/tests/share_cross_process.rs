//! Cross-process preview (spec `gpu-compositor.md`, "Preview export (cross-process)",
//! ADR-0028 on macOS, ADR-0036 on Windows).
//!
//! The main test publishes a canvas whose colour depends on the frame sequence and runs
//! this test binary again as a child process (the "UI"). The child subscribes, aliases the
//! surfaces on its own device, reads every frame back and checks its colour: the API has
//! no path that copies pixels, so the colours reach the child only through the shared
//! memory. On macOS both sides also compare global IOSurface IDs.
#![cfg(any(target_vendor = "apple", windows))]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::solid;
use rbs_core::time::Timestamp;
use rbs_gpu::share::{
    PREVIEW_TEXTURE_FORMAT, PreviewDesc, PreviewEndpoint, PreviewPublisher, PreviewSubscriber,
};
use rbs_gpu::{
    CanvasDesc, Compositor, GpuContext, GpuError, Layer, TextureDesc, TextureId, TexturePool,
};

/// Set in the child: the endpoint to subscribe to.
const CHILD_ENDPOINT: &str = "RBS_PREVIEW_CHILD_ENDPOINT";
/// Set in the child: the engine's IOSurface IDs, comma separated, in slot order (macOS).
const CHILD_IDS: &str = "RBS_PREVIEW_CHILD_IDS";
/// Set in the child: `check` (read frames back, drop them, exit) or `hold` (keep every
/// surface and exit without releasing any).
const CHILD_MODE: &str = "RBS_PREVIEW_CHILD_MODE";

const CANVAS_W: u32 = 128;
const CANVAS_H: u32 = 64;
const PREVIEW_W: u32 = 64;
const PREVIEW_H: u32 = 32;
/// Frames the child checks.
const FRAMES: u64 = 12;

/// Linear canvas colour of frame `sequence`.
fn colour(sequence: u64) -> [f32; 4] {
    match sequence % 4 {
        0 => [1.0, 0.0, 0.0, 1.0],
        1 => [0.0, 1.0, 0.0, 1.0],
        2 => [0.0, 0.0, 1.0, 1.0],
        _ => [0.2, 0.2, 0.2, 1.0],
    }
}

/// sRGB-encoded bytes the preview must hold for frame `sequence`.
fn expected(sequence: u64) -> [u8; 4] {
    let enc = |l: f32| {
        let s = if l <= 0.003_130_8 {
            12.92 * l
        } else {
            1.055 * l.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0).round() as u8
    };
    let c = colour(sequence);
    [enc(c[0]), enc(c[1]), enc(c[2]), 255]
}

/// Renders the canvas of frame `sequence` and returns it.
struct Engine {
    pool: TexturePool,
    comp: Compositor,
    canvas: TextureId,
    sources: [TextureId; 4],
}

impl Engine {
    fn new(ctx: &GpuContext) -> Self {
        let mut pool = TexturePool::new();
        let comp = Compositor::new(
            ctx,
            CanvasDesc {
                width: CANVAS_W,
                height: CANVAS_H,
            },
            1,
        )
        .unwrap();
        let canvas = pool
            .acquire(ctx, &TextureDesc::canvas(CANVAS_W, CANVAS_H))
            .unwrap();
        let sources = [0, 1, 2, 3].map(|s| solid(ctx, &mut pool, 2, 2, colour(s)));
        Self {
            pool,
            comp,
            canvas,
            sources,
        }
    }

    fn render(&mut self, ctx: &GpuContext, sequence: u64) {
        let src = self.sources[(sequence % 4) as usize];
        let layers = [Layer::new(src, 0.0, 0.0, CANVAS_W as f32, CANVAS_H as f32)];
        self.comp
            .render(ctx, &self.pool, &layers, self.canvas)
            .unwrap();
    }

    fn publish(&mut self, ctx: &GpuContext, publisher: &mut PreviewPublisher, sequence: u64) {
        self.render(ctx, sequence);
        match publisher.publish(
            ctx,
            &self.pool,
            self.canvas,
            Timestamp::from_nanos(sequence * 1000),
            sequence,
        ) {
            Ok(()) | Err(GpuError::ExportBusy { .. }) => {}
            Err(e) => panic!("publish failed: {e}"),
        }
    }
}

/// A publisher, or `None` (skip) on devices without unified memory (macOS) or not on
/// D3D12 (Windows).
fn publisher(ctx: &GpuContext) -> Option<PreviewPublisher> {
    match PreviewPublisher::new(
        ctx,
        PreviewDesc {
            width: PREVIEW_W,
            height: PREVIEW_H,
            surfaces: 3,
        },
    ) {
        Ok(p) => Some(p),
        Err(GpuError::Unsupported(why)) => {
            eprintln!("SKIPPED: {why}");
            None
        }
        Err(e) => panic!("publisher failed: {e}"),
    }
}

/// Reads the top-left pixel of a preview texture back (test only).
fn pixel(ctx: &GpuContext, texture: &wgpu::Texture) -> [u8; 4] {
    assert_eq!(texture.format(), PREVIEW_TEXTURE_FORMAT);
    let buffer = ctx.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("preview readback"),
        size: 256,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = ctx
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: Some(1),
            },
        },
        wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
    ctx.queue().submit(Some(encoder.finish()));
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.unwrap());
    ctx.wait_idle().unwrap();
    let data = slice.get_mapped_range().unwrap();
    [data[0], data[1], data[2], data[3]]
}

fn close(a: [u8; 4], b: [u8; 4]) -> bool {
    a.iter().zip(b).all(|(&x, y)| x.abs_diff(y) <= 1)
}

/// Runs this test binary again as the "UI" child in `mode` and drives the engine's render
/// loop until the child exits. Returns the child's output.
fn run_child(
    ctx: &GpuContext,
    publisher: &mut PreviewPublisher,
    engine: &mut Engine,
    mode: &str,
    sequence: &mut u64,
) -> String {
    #[cfg(target_vendor = "apple")]
    let ids: Vec<String> = publisher.surface_ids().map(|i| i.to_string()).collect();
    #[cfg(not(target_vendor = "apple"))]
    let ids: Vec<String> = Vec::new();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "preview_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENDPOINT, publisher.endpoint().to_string())
        .env(CHILD_IDS, ids.join(","))
        .env(CHILD_MODE, mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "child did not finish"
        );
        *sequence += 1;
        engine.publish(ctx, publisher, *sequence);
        publisher.tick(ctx).unwrap();
        thread::yield_now();
    };
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert!(status.success(), "child failed:\n{out}");
    out
}

/// Ticks until the engine notices the child is gone (bounded), then checks that every
/// surface is free and nothing is converted any more.
fn assert_unsubscribed(
    ctx: &GpuContext,
    publisher: &mut PreviewPublisher,
    engine: &mut Engine,
    sequence: &mut u64,
) {
    let start = Instant::now();
    while publisher.subscribed() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the engine did not notice the UI exit"
        );
        *sequence += 1;
        engine.publish(ctx, publisher, *sequence);
        publisher.tick(ctx).unwrap();
        thread::yield_now();
    }
    assert_eq!(publisher.held(), 0);
    let before = publisher.stats();
    engine.publish(ctx, publisher, *sequence + 1);
    assert_eq!(publisher.stats().converted, before.converted);
    assert_eq!(publisher.stats().skipped, before.skipped + 1);
}

#[test]
fn preview_reaches_another_process_without_copies() {
    if std::env::var_os(CHILD_ENDPOINT).is_some() {
        return; // This process is the child; it runs `preview_child` only.
    }
    let Some(ctx) = common::gpu() else { return };
    let Some(mut publisher) = publisher(ctx) else {
        return;
    };
    let mut engine = Engine::new(ctx);
    let mut sequence = 0;
    let out = run_child(ctx, &mut publisher, &mut engine, "check", &mut sequence);
    assert!(
        out.contains("preview child ok"),
        "child did not run:\n{out}"
    );
    let stats = publisher.stats();
    assert_eq!(stats.subscribes, 1);
    assert!(stats.sent >= FRAMES, "{stats:?}");
    // The child dropped everything and exited: the subscription ends.
    assert_unsubscribed(ctx, &mut publisher, &mut engine, &mut sequence);
}

#[test]
fn ui_that_exits_holding_every_surface_frees_them() {
    if std::env::var_os(CHILD_ENDPOINT).is_some() {
        return;
    }
    let Some(ctx) = common::gpu() else { return };
    let Some(mut publisher) = publisher(ctx) else {
        return;
    };
    let mut engine = Engine::new(ctx);
    let mut sequence = 0;
    let out = run_child(ctx, &mut publisher, &mut engine, "hold", &mut sequence);
    assert!(
        out.contains("preview child holding"),
        "child did not run:\n{out}"
    );
    // No frame can be sent (every surface is held), so only the kernel (no-senders notice
    // on macOS, broken pipe on Windows) can tell the engine that the UI is gone.
    assert_unsubscribed(ctx, &mut publisher, &mut engine, &mut sequence);
    assert_eq!(publisher.stats().subscribes, 1);
    // And a new UI can subscribe again and gets every frame right (on Windows this
    // listens again after a connection that ended with writes of its own still queued).
    let out = run_child(ctx, &mut publisher, &mut engine, "check", &mut sequence);
    assert!(
        out.contains("preview child ok"),
        "second child did not run:\n{out}"
    );
    assert_eq!(publisher.stats().subscribes, 2);
    assert_unsubscribed(ctx, &mut publisher, &mut engine, &mut sequence);
}

/// The "UI" process. Run by the tests above; ignored in normal runs.
#[test]
#[ignore = "child process of the cross-process tests"]
fn preview_child() {
    let Some(endpoint) = std::env::var_os(CHILD_ENDPOINT) else {
        return;
    };
    let endpoint: PreviewEndpoint = endpoint.to_str().unwrap().parse().unwrap();
    let ctx = common::gpu().expect("GPU in the child");
    let mut sub = PreviewSubscriber::connect(&endpoint, Duration::from_secs(10)).unwrap();
    assert_eq!(sub.size(), (PREVIEW_W, PREVIEW_H));

    // Same IOSurfaces as the engine's ring: same memory, nothing copied.
    #[cfg(target_vendor = "apple")]
    {
        let ids = std::env::var(CHILD_IDS).unwrap();
        let ids: Vec<u32> = ids.split(',').map(|s| s.parse().unwrap()).collect();
        assert_eq!(sub.surfaces(), ids.len());
        for (slot, id) in ids.iter().enumerate() {
            assert_eq!(sub.surface_id(slot), Some(*id), "slot {slot}");
        }
    }
    assert_eq!(sub.surfaces(), 3);

    if std::env::var(CHILD_MODE).as_deref() == Ok("hold") {
        let mut held = Vec::new();
        while held.len() < sub.surfaces() {
            held.push(sub.recv(Duration::from_secs(10)).unwrap().expect("frame"));
        }
        println!("preview child holding");
        // Exit without dropping anything: the kernel cleans up the rights and handles.
        std::process::exit(0);
    }
    let textures = sub.textures(ctx).unwrap();
    // The first frame stays held to the end: its surface must not be written again.
    let first = sub.recv(Duration::from_secs(10)).unwrap().expect("frame");
    let first_pixel = pixel(ctx, &textures[first.slot()]);
    assert!(
        close(first_pixel, expected(first.sequence())),
        "frame {}: {first_pixel:?} != {:?}",
        first.sequence(),
        expected(first.sequence())
    );
    let mut last = first.sequence();
    for _ in 1..FRAMES {
        let frame = sub.recv(Duration::from_secs(10)).unwrap().expect("frame");
        assert!(frame.sequence() > last, "frames out of order");
        assert_ne!(frame.slot(), first.slot(), "held surface was reused");
        assert_eq!(
            frame.timestamp(),
            Timestamp::from_nanos(frame.sequence() * 1000)
        );
        last = frame.sequence();
        let got = pixel(ctx, &textures[frame.slot()]);
        assert!(
            close(got, expected(frame.sequence())),
            "frame {}: {got:?} != {:?}",
            frame.sequence(),
            expected(frame.sequence())
        );
        // Dropped after the readback has completed: the surface goes back.
    }
    assert_eq!(pixel(ctx, &textures[first.slot()]), first_pixel);
    drop(first);
    println!("preview child ok");
}

/// Runs `connect` on another thread while this thread ticks the publisher. On success it
/// also ticks until the engine has seen the subscription: on Windows `connect` returns
/// once its `ACCEPT` is written, and the engine reads it at a later tick.
fn connect_in_process(
    ctx: &GpuContext,
    publisher: &mut PreviewPublisher,
    endpoint: PreviewEndpoint,
    timeout: Duration,
) -> Result<PreviewSubscriber, GpuError> {
    let handle = thread::spawn(move || PreviewSubscriber::connect(&endpoint, timeout));
    while !handle.is_finished() {
        publisher.tick(ctx).unwrap();
        thread::yield_now();
    }
    let sub = handle.join().unwrap()?;
    let start = Instant::now();
    while !publisher.subscribed() {
        // Hang guard only; the loop ends on the condition.
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "subscription not seen"
        );
        publisher.tick(ctx).unwrap();
        thread::yield_now();
    }
    Ok(sub)
}

#[test]
fn wrong_token_gets_no_surfaces_and_idle_publish_does_no_work() {
    let Some(ctx) = common::gpu() else { return };
    let Some(mut publisher) = publisher(ctx) else {
        return;
    };
    let mut engine = Engine::new(ctx);

    // Nobody subscribed: no GPU work.
    engine.publish(ctx, &mut publisher, 1);
    assert_eq!(publisher.stats().converted, 0);
    assert_eq!(publisher.stats().skipped, 1);

    let mut wrong = publisher.endpoint().clone();
    wrong.token ^= 1;
    let err = connect_in_process(ctx, &mut publisher, wrong, Duration::from_millis(300))
        .err()
        .expect("a wrong token must not subscribe");
    assert!(matches!(err, GpuError::Disconnected(_)), "{err}");
    assert!(!publisher.subscribed());
    assert!(publisher.stats().rejected >= 1);

    // An unknown service fails at once.
    let missing = PreviewEndpoint {
        service: "org.rbs.preview.missing".into(),
        token: 0,
    };
    assert!(matches!(
        PreviewSubscriber::connect(&missing, Duration::from_millis(100)),
        Err(GpuError::Disconnected(_))
    ));

    // The right token subscribes; a full ring is refused, not overwritten.
    let endpoint = publisher.endpoint().clone();
    let mut sub =
        connect_in_process(ctx, &mut publisher, endpoint, Duration::from_secs(5)).unwrap();
    assert!(publisher.subscribed());
    let mut held = Vec::new();
    for seq in 2..10 {
        engine.publish(ctx, &mut publisher, seq);
        ctx.wait_idle().unwrap();
        publisher.tick(ctx).unwrap();
        while let Some(f) = sub.recv(Duration::ZERO).unwrap() {
            held.push(f);
        }
    }
    assert_eq!(
        held.len(),
        3,
        "a ring of 3 delivers 3 frames while all are held"
    );
    assert_eq!(publisher.held(), 3);
    assert!(publisher.stats().busy > 0);

    // A second UI is refused while the first one is alive.
    let endpoint = publisher.endpoint().clone();
    let second = connect_in_process(ctx, &mut publisher, endpoint, Duration::from_millis(300));
    assert!(second.is_err(), "a second subscriber must be refused");
    assert_eq!(publisher.held(), 3);

    // Releasing frees the surfaces again.
    held.clear();
    // Mach delivers at once; a pipe read's completion may land a moment after the
    // write returned (it is posted to the engine thread), so tick until it is seen.
    let start = Instant::now();
    while publisher.held() > 0 {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "releases not seen"
        );
        publisher.tick(ctx).unwrap();
        thread::yield_now();
    }
    engine.publish(ctx, &mut publisher, 20);
    ctx.wait_idle().unwrap();
    publisher.tick(ctx).unwrap();
    let f = sub
        .recv(Duration::from_secs(1))
        .unwrap()
        .expect("frame after release");
    assert_eq!(f.sequence(), 20);

    // The UI drops its subscriber and frames: the engine ends the subscription, and a
    // new UI can subscribe.
    drop(f);
    drop(sub);
    let start = Instant::now();
    while publisher.subscribed() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "no-senders not seen"
        );
        publisher.tick(ctx).unwrap();
        thread::yield_now();
    }
    let endpoint = publisher.endpoint().clone();
    let mut sub =
        connect_in_process(ctx, &mut publisher, endpoint, Duration::from_secs(5)).unwrap();
    assert_eq!(publisher.stats().subscribes, 2);

    // The engine goes away: the subscriber learns it.
    drop(publisher);
    assert!(matches!(
        sub.recv(Duration::from_secs(1)),
        Err(GpuError::Disconnected(_))
    ));
}
