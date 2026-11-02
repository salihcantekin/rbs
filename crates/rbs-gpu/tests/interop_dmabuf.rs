//! Conformance test for DMA-BUF import on Vulkan (spec `gpu-compositor.md`, "Native import").
//!
//! The producer is `/dev/udmabuf`: a sealed memfd filled on the CPU and wrapped as a linear
//! DMA-BUF. The import test runs where the device has `gpu.import.dmabuf` and udmabuf is
//! available; `RBS_REQUIRE_DMABUF` turns a missing piece into a failure. CI's software
//! Vulkan (lavapipe) has no DMA-BUF import, so CI runs only the rejection test.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use common::{assert_close, at};
use rbs_core::capability::CapabilityValue;
use rbs_core::media::{
    ColorSpace, DmaBufPlane, DmaBufSurface, Lease, LeaseOwner, NativeSurface, PixelFormat, Size,
    SurfaceSync, VideoFormat, VideoFrame,
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

const W: u32 = 64;
const H: u32 = 64;
const STRIDE: u32 = W * 4;
const AR24: u32 = u32::from_le_bytes(*b"AR24");
const LINEAR: u64 = 0;

/// `struct udmabuf_create` from `linux/udmabuf.h`.
#[repr(C)]
struct UdmabufCreate {
    memfd: u32,
    flags: u32,
    offset: u64,
    size: u64,
}
/// `_IOW('u', 0x42, struct udmabuf_create)`.
const UDMABUF_CREATE: libc::c_ulong = 0x4018_7542;

/// A linear ARGB8888 DMA-BUF filled with opaque red, or `None` without udmabuf.
fn red_dmabuf() -> Option<OwnedFd> {
    let size = (STRIDE * H) as usize;
    let pixels: Vec<u8> = (0..W * H).flat_map(|_| [0u8, 0, 255, 255]).collect();
    // SAFETY: plain syscalls on descriptors created here; every result is checked and
    // each descriptor is owned by an `OwnedFd` right after creation.
    unsafe {
        let dev = libc::open(c"/dev/udmabuf".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if dev < 0 {
            return None;
        }
        let dev = OwnedFd::from_raw_fd(dev);
        let memfd = libc::memfd_create(
            c"rbs-dmabuf-test".as_ptr(),
            libc::MFD_ALLOW_SEALING | libc::MFD_CLOEXEC,
        );
        assert!(memfd >= 0, "memfd_create");
        let memfd = OwnedFd::from_raw_fd(memfd);
        assert_eq!(libc::ftruncate(memfd.as_raw_fd(), size as libc::off_t), 0);
        let written = libc::pwrite(memfd.as_raw_fd(), pixels.as_ptr().cast(), size, 0);
        assert_eq!(written, size as isize);
        assert_eq!(
            libc::fcntl(memfd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK),
            0
        );
        let create = UdmabufCreate {
            memfd: memfd.as_raw_fd() as u32,
            flags: 0x01, // UDMABUF_FLAGS_CLOEXEC
            offset: 0,
            size: size as u64,
        };
        let buf = libc::ioctl(dev.as_raw_fd(), UDMABUF_CREATE, &create);
        if buf < 0 {
            return None;
        }
        Some(OwnedFd::from_raw_fd(buf))
    }
}

fn frame(fd: i32, lease: Lease) -> VideoFrame {
    let format = VideoFormat::new(Size::new(W, H), PixelFormat::Bgra8, ColorSpace::SRGB);
    let surface = DmaBufSurface::new(
        AR24,
        LINEAR,
        &[DmaBufPlane {
            fd,
            offset: 0,
            stride: STRIDE,
        }],
    )
    .unwrap();
    // SAFETY: the test keeps `fd` open for longer than the frame and its lease.
    unsafe {
        VideoFrame::native(
            format,
            Timestamp::ZERO,
            NativeSurface::DmaBuf(surface),
            SurfaceSync::Ready,
            lease,
        )
    }
}

fn has_dmabuf_import(ctx: &rbs_gpu::GpuContext) -> bool {
    ctx.capabilities().get(&rbs_gpu::context::CAP_IMPORT_DMABUF)
        == Some(&CapabilityValue::Flag(true))
}

fn required() -> bool {
    std::env::var_os("RBS_REQUIRE_DMABUF").is_some_and(|v| !v.is_empty())
}

#[test]
fn dmabuf_is_aliased_rendered_and_handed_back() {
    let Some(ctx) = common::gpu() else { return };
    if !has_dmabuf_import(ctx) {
        assert!(
            !required(),
            "RBS_REQUIRE_DMABUF is set but the device has no DMA-BUF import"
        );
        eprintln!("SKIPPED: device has no DMA-BUF import");
        return;
    }
    let Some(buf) = red_dmabuf() else {
        assert!(
            !required(),
            "RBS_REQUIRE_DMABUF is set but /dev/udmabuf is not usable"
        );
        eprintln!("SKIPPED: /dev/udmabuf is not usable");
        return;
    };
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
            .import(ctx, &mut pool, frame(buf.as_raw_fd(), owner.lease()))
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
        importer.retire(ctx, &mut pool, Timestamp::ZERO);
        ctx.wait_idle().unwrap();
        importer.retire(ctx, &mut pool, Timestamp::ZERO);
        assert_eq!(owner.held(), 0, "lease returned after the GPU finished");
    }
    assert_eq!(
        ctx.metrics().surfaces_opened() - opened_before,
        1,
        "the buffer is opened once and reused"
    );
}

#[test]
fn dmabuf_without_device_support_or_with_fences_is_rejected() {
    let Some(ctx) = common::gpu() else { return };
    let owner = Arc::new(Producer::default());
    let mut pool = TexturePool::new();
    let mut importer = Importer::new();

    // Any open descriptor will do: the frame is rejected before it is imported.
    let file = std::fs::File::open("/proc/self/exe").unwrap();
    let mut f = frame(file.as_raw_fd(), owner.lease());
    if has_dmabuf_import(ctx) {
        // With device support, a fenced frame is what gets rejected.
        let format = *f.format();
        drop(f);
        let surface = DmaBufSurface::new(
            AR24,
            LINEAR,
            &[DmaBufPlane {
                fd: file.as_raw_fd(),
                offset: 0,
                stride: STRIDE,
            }],
        )
        .unwrap();
        // SAFETY: `file` outlives the frame.
        f = unsafe {
            VideoFrame::native(
                format,
                Timestamp::ZERO,
                NativeSurface::DmaBuf(surface),
                SurfaceSync::sync_file(-1),
                owner.lease(),
            )
        };
    }
    assert!(matches!(
        importer.import(ctx, &mut pool, f),
        Err(rbs_gpu::GpuError::Unsupported(_))
    ));
    assert_eq!(owner.held(), 0, "a rejected frame hands its lease back");
    assert_eq!(importer.open_surfaces(), 0);
}
