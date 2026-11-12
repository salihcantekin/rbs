//! Native export to IOSurfaces for VideoToolbox (spec `gpu-compositor.md`, "Native
//! export"). The surface bytes, read through the IOSurface API as an encoder would, must
//! equal the two-plane conversion of the same canvas byte for byte.
#![cfg(target_vendor = "apple")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::solid;
use objc2_io_surface::{IOSurfaceLockOptions, IOSurfaceRef};
use rbs_core::capability::CapabilityValue;
use rbs_core::media::{
    ColorSpace, FramePayload, NativeSurface, PixelFormat, SurfaceSync, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_gpu::context::CAP_P010;
use rbs_gpu::export::{FOURCC_NV12, FOURCC_NV12_FULL, FOURCC_P010, SurfaceExporter};
use rbs_gpu::{
    CanvasDesc, Compositor, GpuContext, GpuError, Layer, TextureDesc, TextureId, TexturePool,
    YuvConverter, YuvFormat, readback, yuv,
};

const W: u32 = 64;
const H: u32 = 32;

/// A canvas with red on the left half and a mid grey on the right.
fn canvas(ctx: &GpuContext, pool: &mut TexturePool) -> TextureId {
    let mut comp = Compositor::new(
        ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        2,
    )
    .unwrap();
    let canvas = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let red = solid(ctx, pool, 2, 2, [1.0, 0.0, 0.0, 1.0]);
    let grey = solid(ctx, pool, 2, 2, [0.2, 0.2, 0.2, 1.0]);
    let half = W as f32 / 2.0;
    let layers = [
        Layer::new(red, 0.0, 0.0, half, H as f32),
        Layer::new(grey, half, 0.0, half, H as f32),
    ];
    comp.render(ctx, pool, &layers, canvas).unwrap();
    canvas
}

/// Tightly packed plane bytes from the two-plane conversion (the reference output).
fn reference(
    ctx: &GpuContext,
    pool: &mut TexturePool,
    format: YuvFormat,
    canvas: TextureId,
) -> [Vec<u8>; 2] {
    let luma = pool.acquire(ctx, &yuv::luma_desc(format, W, H)).unwrap();
    let chroma = pool.acquire(ctx, &yuv::chroma_desc(format, W, H)).unwrap();
    let mut conv = YuvConverter::new(ctx, format, W, H).unwrap();
    conv.convert(ctx, pool, canvas, luma, chroma).unwrap();
    [
        readback::read_bytes(ctx, pool, luma).unwrap(),
        readback::read_bytes(ctx, pool, chroma).unwrap(),
    ]
}

fn io_surface(frame: &VideoFrame) -> &IOSurfaceRef {
    let FramePayload::Native { surface, sync, .. } = *frame.payload() else {
        panic!("exported frame is not native");
    };
    assert_eq!(sync, SurfaceSync::Ready);
    let NativeSurface::IoSurface { surface, .. } = surface else {
        panic!("exported frame is not an IOSurface");
    };
    // SAFETY: the frame's lease keeps the surface alive for the frame's lifetime, which
    // bounds the returned reference.
    unsafe { &*(surface as *const IOSurfaceRef) }
}

/// Tightly packed bytes of both planes, read through the IOSurface API.
fn surface_bytes(io: &IOSurfaceRef) -> [Vec<u8>; 2] {
    let mut seed = 0;
    // SAFETY: read-only lock; the planes are read inside it and within each plane's
    // `bytes_per_row * height` extent.
    unsafe {
        assert_eq!(io.lock(IOSurfaceLockOptions::ReadOnly, &mut seed), 0);
        let planes = [0, 1].map(|p| {
            let base = io.base_address_of_plane(p).as_ptr().cast::<u8>();
            let stride = io.bytes_per_row_of_plane(p);
            let row = io.width_of_plane(p) * io.bytes_per_element_of_plane(p);
            let mut out = Vec::with_capacity(row * io.height_of_plane(p));
            for y in 0..io.height_of_plane(p) {
                out.extend_from_slice(std::slice::from_raw_parts(base.add(y * stride), row));
            }
            out
        });
        assert_eq!(io.unlock(IOSurfaceLockOptions::ReadOnly, &mut seed), 0);
        planes
    }
}

/// Waits for the GPU and takes the next ready frame.
fn ready(ctx: &GpuContext, exporter: &mut SurfaceExporter) -> VideoFrame {
    ctx.wait_idle().unwrap();
    exporter
        .take_ready(ctx)
        .unwrap()
        .expect("frame ready after wait_idle")
}

fn round_trip(format: YuvFormat) {
    let Some(ctx) = common::gpu() else { return };
    let p010 = ctx.capabilities().get(&CAP_P010) == Some(&CapabilityValue::Flag(true));
    if format == YuvFormat::P010 && !p010 {
        eprintln!("SKIPPED: device lacks gpu.p010");
        return;
    }
    let mut pool = TexturePool::new();
    let canvas = canvas(ctx, &mut pool);
    let want = reference(ctx, &mut pool, format, canvas);

    let mut exporter = SurfaceExporter::new(ctx, format, W, H, 3).unwrap();
    exporter
        .export(ctx, &pool, canvas, Timestamp::from_nanos(42), 7)
        .unwrap();
    let frame = ready(ctx, &mut exporter);
    assert_eq!(frame.timestamp(), Timestamp::from_nanos(42));
    assert_eq!(frame.sequence(), 7);
    assert_eq!(frame.format().pixel_format, format.pixel_format());
    assert_eq!(frame.format().size.width, W);

    let io = io_surface(&frame);
    let fourcc = match format {
        YuvFormat::Nv12 => FOURCC_NV12,
        YuvFormat::P010 => FOURCC_P010,
    };
    assert_eq!(io.pixel_format(), fourcc);
    assert_eq!(io.plane_count(), 2);
    let got = surface_bytes(io);
    assert_eq!(got[0], want[0], "{format:?} luma");
    assert_eq!(got[1], want[1], "{format:?} chroma");
    let stats = exporter.stats();
    assert_eq!((stats.converted, stats.delivered, stats.busy), (1, 1, 0));
    drop(frame);
}

#[test]
fn nv12_surface_matches_planes() {
    round_trip(YuvFormat::Nv12);
}

#[test]
fn screen_surface_is_full_range_and_matches_the_srgb_planes() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let canvas = canvas(ctx, &mut pool);
    let luma = pool
        .acquire(ctx, &yuv::luma_desc(YuvFormat::Nv12, W, H))
        .unwrap();
    let chroma = pool
        .acquire(ctx, &yuv::chroma_desc(YuvFormat::Nv12, W, H))
        .unwrap();
    let mut conv =
        YuvConverter::with_color(ctx, YuvFormat::Nv12, W, H, ColorSpace::SRGB_VIDEO).unwrap();
    conv.convert(ctx, &pool, canvas, luma, chroma).unwrap();
    let want = [
        readback::read_bytes(ctx, &pool, luma).unwrap(),
        readback::read_bytes(ctx, &pool, chroma).unwrap(),
    ];
    let mut exporter =
        SurfaceExporter::with_color(ctx, YuvFormat::Nv12, W, H, 2, ColorSpace::SRGB_VIDEO).unwrap();
    assert_eq!(exporter.format().color, ColorSpace::SRGB_VIDEO);
    exporter
        .export(ctx, &pool, canvas, Timestamp::from_nanos(1), 1)
        .unwrap();
    let frame = ready(ctx, &mut exporter);
    assert_eq!(frame.format().color, ColorSpace::SRGB_VIDEO);
    let io = io_surface(&frame);
    assert_eq!(io.pixel_format(), FOURCC_NV12_FULL);
    let got = surface_bytes(io);
    assert_eq!(got[0], want[0]);
    assert_eq!(got[1], want[1]);
}

#[test]
fn p010_surface_matches_planes() {
    round_trip(YuvFormat::P010);
}

#[test]
fn held_surfaces_are_not_reused() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let canvas = canvas(ctx, &mut pool);
    let mut exporter = SurfaceExporter::new(ctx, YuvFormat::Nv12, W, H, 2).unwrap();

    let take = |exporter: &mut SurfaceExporter, n: u64| {
        exporter
            .export(ctx, &pool, canvas, Timestamp::from_nanos(n), n)
            .unwrap();
        ready(ctx, exporter)
    };
    let first = take(&mut exporter, 1);
    let second = take(&mut exporter, 2);
    assert_ne!(
        std::ptr::from_ref(io_surface(&first)),
        std::ptr::from_ref(io_surface(&second))
    );

    // Both surfaces are held by the "encoder": the next frame is refused, not written.
    assert_eq!(
        exporter.export(ctx, &pool, canvas, Timestamp::from_nanos(3), 3),
        Err(GpuError::ExportBusy { surfaces: 2 })
    );
    assert_eq!(exporter.stats().busy, 1);

    // Releasing one frame frees exactly its surface.
    let released = std::ptr::from_ref(io_surface(&first));
    drop(first);
    let third = take(&mut exporter, 3);
    assert_eq!(std::ptr::from_ref(io_surface(&third)), released);
    drop((second, third));
}

#[test]
fn frames_outlive_exporter_and_pool() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let canvas = canvas(ctx, &mut pool);
    let want = reference(ctx, &mut pool, YuvFormat::Nv12, canvas);
    let before = pool.allocated();
    let mut exporter = SurfaceExporter::new(ctx, YuvFormat::Nv12, W, H, 2).unwrap();
    assert_eq!(pool.allocated(), before, "the exporter holds no pool slots");
    exporter
        .export(ctx, &pool, canvas, Timestamp::ZERO, 0)
        .unwrap();
    let frame = ready(ctx, &mut exporter);
    assert_eq!(exporter.held(), 1);

    // The encoder still holds the frame after the render side is torn down: the lease
    // alone keeps the surface valid and unchanged.
    drop(exporter);
    drop(pool);
    ctx.wait_idle().unwrap();
    assert_eq!(surface_bytes(io_surface(&frame)), want);
    drop(frame);
}

#[test]
fn frames_come_out_in_order_and_drop_on_other_threads() {
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let canvas = canvas(ctx, &mut pool);
    let mut exporter = SurfaceExporter::new(ctx, YuvFormat::Nv12, W, H, 4).unwrap();
    for n in 0..3 {
        exporter
            .export(ctx, &pool, canvas, Timestamp::from_nanos(n), n)
            .unwrap();
    }
    assert_eq!(exporter.pending(), 3);
    ctx.wait_idle().unwrap();
    let frames: Vec<VideoFrame> =
        std::iter::from_fn(|| exporter.take_ready(ctx).unwrap()).collect();
    assert_eq!(
        frames.iter().map(VideoFrame::sequence).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(exporter.held(), 3);

    // An encoder thread drops the frames; the surfaces become free for the render thread.
    std::thread::spawn(move || drop(frames)).join().unwrap();
    assert_eq!(exporter.held(), 0);
    for n in 3..7 {
        exporter
            .export(ctx, &pool, canvas, Timestamp::from_nanos(n), n)
            .unwrap();
    }
    assert_eq!(exporter.stats().busy, 0);
}

#[test]
fn surfaces_wrap_as_cvpixelbuffers() {
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferCreateWithIOSurface, CVPixelBufferGetHeight,
        CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth,
        kCVReturnSuccess,
    };
    let Some(ctx) = common::gpu() else { return };
    let mut pool = TexturePool::new();
    let canvas = canvas(ctx, &mut pool);
    let mut exporter = SurfaceExporter::new(ctx, YuvFormat::Nv12, W, H, 2).unwrap();
    exporter
        .export(ctx, &pool, canvas, Timestamp::ZERO, 0)
        .unwrap();
    let frame = ready(ctx, &mut exporter);
    let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: the surface is valid while `frame` lives; `out` is a valid out pointer and
    // receives a +1 reference, which `CFRetained::from_raw` takes over.
    let buffer = unsafe {
        let status = CVPixelBufferCreateWithIOSurface(
            None,
            io_surface(&frame),
            None,
            std::ptr::NonNull::from(&mut out),
        );
        assert_eq!(status, kCVReturnSuccess);
        objc2_core_foundation::CFRetained::from_raw(std::ptr::NonNull::new(out).unwrap())
    };
    assert_eq!(CVPixelBufferGetPixelFormatType(&buffer), FOURCC_NV12);
    assert_eq!(CVPixelBufferGetPlaneCount(&buffer), 2);
    assert_eq!(CVPixelBufferGetWidth(&buffer), W as usize);
    assert_eq!(CVPixelBufferGetHeight(&buffer), H as usize);
}

#[test]
fn invalid_configurations_are_rejected() {
    let Some(ctx) = common::gpu() else { return };
    assert!(matches!(
        SurfaceExporter::new(ctx, YuvFormat::Nv12, 15, 8, 3),
        Err(GpuError::InvalidSize { .. })
    ));
    assert!(matches!(
        SurfaceExporter::new(ctx, YuvFormat::Nv12, W, H, 1),
        Err(GpuError::Unsupported(_))
    ));
}

#[test]
fn nothing_is_ready_before_export() {
    let Some(ctx) = common::gpu() else { return };
    let mut exporter = SurfaceExporter::new(ctx, YuvFormat::Nv12, W, H, 2).unwrap();
    assert!(exporter.take_ready(ctx).unwrap().is_none());
    assert_eq!(exporter.pending(), 0);
    assert_eq!(exporter.format().pixel_format, PixelFormat::Nv12);
}
