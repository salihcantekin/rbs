//! Conformance test for IOSurface import on Metal (spec `gpu-compositor.md`,
//! "Native import"). A test producer creates an IOSurface, fills it on the CPU and hands it
//! over as a native frame.
#![cfg(target_vendor = "apple")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use common::{assert_close, at};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString};
use objc2_io_surface::{
    IOSurfaceLockOptions, IOSurfaceRef, kIOSurfaceBytesPerElement, kIOSurfaceHeight,
    kIOSurfacePixelFormat, kIOSurfaceWidth,
};
use rbs_core::media::{
    ColorSpace, Lease, LeaseOwner, NativeSurface, PixelFormat, Size, SurfaceSync, VideoFormat,
    VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_gpu::{CanvasDesc, Compositor, Importer, Layer, TextureDesc, TexturePool, readback};

#[derive(Default)]
struct Producer {
    count: AtomicI64,
}

// SAFETY: the counter is the only state; retain/release never fail.
unsafe impl LeaseOwner for Producer {
    fn retain(&self, _token: u64) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }
    fn release(&self, _token: u64) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Producer {
    fn lease(self: &Arc<Self>) -> Lease {
        self.retain(0);
        // SAFETY: one reference was just retained for this lease.
        unsafe { Lease::adopt(Arc::clone(self) as Arc<dyn LeaseOwner>, 0) }
    }
    fn held(&self) -> i64 {
        self.count.load(Ordering::SeqCst)
    }
}

/// Tests that count `surfaces_opened` on the shared context run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const W: u32 = 8;
const H: u32 = 8;

/// An 8x8 'BGRA' IOSurface filled with opaque red.
fn red_surface() -> CFRetained<IOSurfaceRef> {
    // SAFETY: the keys are CoreFoundation constants that live for the whole process.
    let keys: [&CFString; 4] = unsafe {
        [
            kIOSurfaceWidth,
            kIOSurfaceHeight,
            kIOSurfaceBytesPerElement,
            kIOSurfacePixelFormat,
        ]
    };
    let values = [
        CFNumber::new_i32(W as i32),
        CFNumber::new_i32(H as i32),
        CFNumber::new_i32(4),
        CFNumber::new_i32(i32::from_be_bytes(*b"BGRA")),
    ];
    let values: Vec<&CFNumber> = values.iter().map(|v| &**v).collect();
    let props = CFDictionary::from_slices(&keys, &values);
    // SAFETY: `props` is a valid dictionary of IOSurface properties.
    let surface = unsafe { IOSurfaceRef::new(props.as_ref()) }.expect("IOSurfaceCreate");

    // SAFETY: the surface is locked for CPU writes while the rows are written, each row
    // lies inside the allocation (`bytes_per_row * height`), and nothing else uses it yet.
    unsafe {
        assert_eq!(
            surface.lock(IOSurfaceLockOptions::empty(), std::ptr::null_mut()),
            0
        );
        let base = surface.base_address().as_ptr().cast::<u8>();
        let stride = surface.bytes_per_row();
        for y in 0..H as usize {
            let row = std::slice::from_raw_parts_mut(base.add(y * stride), W as usize * 4);
            for px in row.chunks_exact_mut(4) {
                px.copy_from_slice(&[0, 0, 255, 255]);
            }
        }
        assert_eq!(
            surface.unlock(IOSurfaceLockOptions::empty(), std::ptr::null_mut()),
            0
        );
    }
    surface
}

fn frame(surface: &IOSurfaceRef, sync: SurfaceSync, lease: Lease) -> VideoFrame {
    sized_frame(surface, Size::new(W, H), sync, lease)
}

fn sized_frame(surface: &IOSurfaceRef, size: Size, sync: SurfaceSync, lease: Lease) -> VideoFrame {
    let format = VideoFormat::new(size, PixelFormat::Bgra8, ColorSpace::SRGB);
    let ptr = std::ptr::from_ref(surface) as usize;
    // SAFETY: the surface outlives the frame (the test holds it), and the lease is held
    // until the importer hands it back.
    unsafe {
        VideoFrame::native(
            format,
            Timestamp::ZERO,
            NativeSurface::io_surface(ptr),
            sync,
            lease,
        )
    }
}

#[test]
fn io_surface_is_aliased_rendered_and_handed_back() {
    let Some(ctx) = common::gpu() else { return };
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let surface = red_surface();
    let owner = Arc::new(Producer::default());

    let mut pool = TexturePool::new();
    let mut importer = Importer::new();
    let mut comp = Compositor::new(
        ctx,
        CanvasDesc {
            width: W,
            height: H,
        },
        1,
    )
    .unwrap();
    let canvas = pool.acquire(ctx, &TextureDesc::canvas(W, H)).unwrap();
    let opened_before = ctx.metrics().surfaces_opened();

    for _ in 0..2 {
        let imported = importer
            .import(
                ctx,
                &mut pool,
                frame(&surface, SurfaceSync::Ready, owner.lease()),
            )
            .unwrap();
        let rbs_core::media::FramePayload::Texture(id) = *imported.payload() else {
            panic!("import must return a texture frame");
        };
        comp.render(
            ctx,
            &pool,
            &[Layer::new(id, 0.0, 0.0, W as f32, H as f32)],
            canvas,
        )
        .unwrap();
        let px = readback::read_rgba16f(ctx, &pool, canvas).unwrap();
        assert_close(at(&px, W, 4, 4), [1.0, 0.0, 0.0, 1.0], 0.01);

        assert_eq!(owner.held(), 1, "lease held while a frame references it");
        drop(imported);
        importer.retire(ctx, &mut pool); // schedules the hand-back
        ctx.wait_idle().unwrap();
        importer.retire(ctx, &mut pool); // sees it complete
        assert_eq!(owner.held(), 0, "lease returned after the GPU finished");
    }
    assert_eq!(
        ctx.metrics().surfaces_opened() - opened_before,
        1,
        "the surface is opened once and reused"
    );
    assert_eq!(importer.open_surfaces(), 1);
    assert_eq!(ctx.metrics().frames_cpu_copied(), 0);

    // The source stops: its surfaces leave the cache.
    importer.close_unused(&mut pool);
    assert_eq!(importer.open_surfaces(), 0);
}

#[test]
fn fenced_sync_is_rejected_on_metal() {
    let Some(ctx) = common::gpu() else { return };
    let surface = red_surface();
    let owner = Arc::new(Producer::default());
    let mut pool = TexturePool::new();
    let mut importer = Importer::new();

    let fenced = frame(&surface, SurfaceSync::timeline_fence(1, 1), owner.lease());
    assert!(matches!(
        importer.import(ctx, &mut pool, fenced),
        Err(rbs_gpu::GpuError::Unsupported(_))
    ));
    assert_eq!(owner.held(), 0, "a rejected frame hands its lease back");
    assert_eq!(importer.open_surfaces(), 0);
}

#[test]
fn frame_of_another_size_reopens_the_cached_surface() {
    let Some(ctx) = common::gpu() else { return };
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let surface = red_surface();
    let owner = Arc::new(Producer::default());
    let mut pool = TexturePool::new();
    let mut importer = Importer::new();
    let opened_before = ctx.metrics().surfaces_opened();

    for size in [Size::new(W, H), Size::new(W, H), Size::new(W / 2, H / 2)] {
        let f = sized_frame(&surface, size, SurfaceSync::Ready, owner.lease());
        drop(importer.import(ctx, &mut pool, f).unwrap());
        importer.retire(ctx, &mut pool);
        ctx.wait_idle().unwrap();
        importer.retire(ctx, &mut pool);
    }
    assert_eq!(
        ctx.metrics().surfaces_opened() - opened_before,
        2,
        "a size change on a cached key is treated as a new object"
    );
    assert_eq!(importer.open_surfaces(), 1);
    assert_eq!(owner.held(), 0);
}
