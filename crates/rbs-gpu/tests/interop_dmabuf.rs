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
    frame_with_sync(fd, lease, SurfaceSync::Ready)
}

fn frame_with_sync(fd: i32, lease: Lease, sync: SurfaceSync) -> VideoFrame {
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
            sync,
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
        // With device support, a sync kind Vulkan does not take is what gets rejected.
        drop(f);
        f = frame_with_sync(
            file.as_raw_fd(),
            owner.lease(),
            SurfaceSync::timeline_fence(1, 1),
        );
    }
    assert!(matches!(
        importer.import(ctx, &mut pool, f),
        Err(rbs_gpu::GpuError::Unsupported(_))
    ));
    assert_eq!(owner.held(), 0, "a rejected frame hands its lease back");
    assert_eq!(importer.open_surfaces(), 0);
}

fn has_sync_file_import(ctx: &rbs_gpu::GpuContext) -> bool {
    ctx.capabilities()
        .get(&rbs_gpu::context::CAP_IMPORT_SYNC_FILE)
        == Some(&CapabilityValue::Flag(true))
}

#[test]
fn sync_file_without_device_support_is_rejected() {
    let Some(ctx) = common::gpu() else { return };
    if has_sync_file_import(ctx) {
        eprintln!("SKIPPED: device imports sync files");
        return;
    }
    let owner = Arc::new(Producer::default());
    let mut pool = TexturePool::new();
    let mut importer = Importer::new();
    let file = std::fs::File::open("/proc/self/exe").unwrap();
    let f = frame_with_sync(
        file.as_raw_fd(),
        owner.lease(),
        SurfaceSync::sync_file(file.as_raw_fd()),
    );
    assert!(matches!(
        importer.import(ctx, &mut pool, f),
        Err(rbs_gpu::GpuError::Unsupported(_))
    ));
    assert_eq!(owner.held(), 0, "a rejected frame hands its lease back");
    assert_eq!(importer.open_surfaces(), 0);
}

/// `struct dma_buf_export_sync_file` from `linux/dma-buf.h`.
#[repr(C)]
struct ExportSyncFile {
    flags: u32,
    fd: i32,
}
/// `_IOWR('b', 2, struct dma_buf_export_sync_file)`.
const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: libc::c_ulong = 0xC008_6202;
const DMA_BUF_SYNC_READ: u32 = 1;

/// The buffer's implicit fences as a sync file (Linux 6.0+). A udmabuf has none, so the
/// file is already signalled.
fn exported_sync_file(buf: &OwnedFd) -> Option<OwnedFd> {
    let mut arg = ExportSyncFile {
        flags: DMA_BUF_SYNC_READ,
        fd: -1,
    };
    // SAFETY: a plain ioctl on a DMA-BUF owned by the caller with a valid argument struct.
    let rc = unsafe { libc::ioctl(buf.as_raw_fd(), DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &mut arg) };
    if rc < 0 || arg.fd < 0 {
        return None;
    }
    // SAFETY: the ioctl returned a new descriptor that nothing else owns.
    Some(unsafe { OwnedFd::from_raw_fd(arg.fd) })
}

/// `struct sw_sync_create_fence_data` from the kernel's `sync_debug.h`.
#[repr(C)]
struct SwSyncCreateFence {
    value: u32,
    name: [u8; 32],
    fence: i32,
}
/// `_IOWR('W', 0, struct sw_sync_create_fence_data)`.
const SW_SYNC_IOC_CREATE_FENCE: libc::c_ulong = 0xC028_5700;
/// `_IOW('W', 1, __u32)`.
const SW_SYNC_IOC_INC: libc::c_ulong = 0x4004_5701;

/// A software sync timeline and a pending sync file at point 1, or `None` without
/// `sw_sync` (needs debugfs, `CONFIG_SW_SYNC` and usually root).
fn pending_sync_file() -> Option<(OwnedFd, OwnedFd)> {
    // SAFETY: plain syscalls on descriptors created here; results are checked and each
    // descriptor is owned right after creation.
    unsafe {
        let timeline = libc::open(
            c"/sys/kernel/debug/sync/sw_sync".as_ptr(),
            libc::O_RDWR | libc::O_CLOEXEC,
        );
        if timeline < 0 {
            return None;
        }
        let timeline = OwnedFd::from_raw_fd(timeline);
        let mut data = SwSyncCreateFence {
            value: 1,
            name: [0; 32],
            fence: -1,
        };
        if libc::ioctl(timeline.as_raw_fd(), SW_SYNC_IOC_CREATE_FENCE, &mut data) < 0
            || data.fence < 0
        {
            return None;
        }
        Some((timeline, OwnedFd::from_raw_fd(data.fence)))
    }
}

/// Signals the timeline once, also when the test panics first, so a failing test never
/// leaves the shared queue blocked.
struct Signaller(Option<OwnedFd>);

impl Signaller {
    fn signal(&mut self) {
        if let Some(timeline) = self.0.take() {
            let step: u32 = 1;
            // SAFETY: a plain ioctl on the test's own sw_sync timeline.
            let rc = unsafe { libc::ioctl(timeline.as_raw_fd(), SW_SYNC_IOC_INC, &step) };
            assert_eq!(rc, 0, "SW_SYNC_IOC_INC");
        }
    }
}

impl Drop for Signaller {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.signal();
        } else if let Some(timeline) = self.0.take() {
            let step: u32 = 1;
            // SAFETY: as in `signal`; the result is ignored while unwinding.
            let _ = unsafe { libc::ioctl(timeline.as_raw_fd(), SW_SYNC_IOC_INC, &step) };
        }
    }
}

/// Device with DMA-BUF and sync-file import plus a udmabuf, or `None` (skip).
fn sync_setup() -> Option<(&'static rbs_gpu::GpuContext, OwnedFd)> {
    let ctx = common::gpu()?;
    if !has_dmabuf_import(ctx) || !has_sync_file_import(ctx) {
        assert!(
            !required(),
            "RBS_REQUIRE_DMABUF is set but the device has no DMA-BUF or sync-file import"
        );
        eprintln!("SKIPPED: device has no DMA-BUF or sync-file import");
        return None;
    }
    let Some(buf) = red_dmabuf() else {
        assert!(
            !required(),
            "RBS_REQUIRE_DMABUF is set but /dev/udmabuf is not usable"
        );
        eprintln!("SKIPPED: /dev/udmabuf is not usable");
        return None;
    };
    Some((ctx, buf))
}

/// Imports `buf` with `sync` and renders it; returns the imported frame and the canvas.
fn import_and_render(
    ctx: &rbs_gpu::GpuContext,
    pool: &mut TexturePool,
    importer: &mut Importer,
    buf: &OwnedFd,
    lease: Lease,
    sync: SurfaceSync,
) -> (VideoFrame, rbs_core::media::TextureId) {
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
    let imported = importer
        .import(ctx, pool, frame_with_sync(buf.as_raw_fd(), lease, sync))
        .unwrap();
    let rbs_core::media::FramePayload::Texture(id) = *imported.payload() else {
        panic!("import must return a texture frame");
    };
    comp.render(
        ctx,
        pool,
        &[Layer::new(id, 0.0, 0.0, W as f32, H as f32)],
        canvas,
    )
    .unwrap();
    (imported, canvas)
}

#[test]
fn signalled_sync_file_is_accepted() {
    let Some((ctx, buf)) = sync_setup() else {
        return;
    };
    let Some(sync) = exported_sync_file(&buf) else {
        assert!(
            !required(),
            "RBS_REQUIRE_DMABUF is set but DMA_BUF_IOCTL_EXPORT_SYNC_FILE failed"
        );
        eprintln!("SKIPPED: DMA_BUF_IOCTL_EXPORT_SYNC_FILE (Linux 6.0+) failed");
        return;
    };
    let owner = Arc::new(Producer::default());
    let mut pool = TexturePool::new();
    let mut importer = Importer::new();
    let (imported, canvas) = import_and_render(
        ctx,
        &mut pool,
        &mut importer,
        &buf,
        owner.lease(),
        SurfaceSync::sync_file(sync.as_raw_fd()),
    );
    let px = readback::read_rgba16f(ctx, &pool, canvas).unwrap();
    assert_close(at(&px, W, 4, 4), [1.0, 0.0, 0.0, 1.0], 0.01);
    drop(imported);
    importer.retire(ctx, &mut pool, Timestamp::ZERO);
    ctx.wait_idle().unwrap();
    importer.retire(ctx, &mut pool, Timestamp::ZERO);
    assert_eq!(owner.held(), 0, "lease returned after the GPU finished");
}

#[test]
fn pending_sync_file_holds_the_gpu_until_signalled() {
    let Some((ctx, buf)) = sync_setup() else {
        return;
    };
    // sw_sync is a debug interface; it is optional even where DMA-BUF is required.
    let Some((timeline, fence)) = pending_sync_file() else {
        eprintln!("SKIPPED: sw_sync is not available (debugfs, CONFIG_SW_SYNC, root)");
        return;
    };
    let mut timeline = Signaller(Some(timeline));
    let owner = Arc::new(Producer::default());
    let mut pool = TexturePool::new();
    let mut importer = Importer::new();
    let (imported, canvas) = import_and_render(
        ctx,
        &mut pool,
        &mut importer,
        &buf,
        owner.lease(),
        SurfaceSync::sync_file(fence.as_raw_fd()),
    );
    // The render cannot finish while the producer's fence is pending.
    let done = ctx
        .device()
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(std::time::Duration::from_millis(200)),
        })
        .is_ok();
    assert!(!done, "GPU work finished before the sync file signalled");
    drop(fence); // The importer imported a duplicate; the producer may close its own.
    timeline.signal();
    let px = readback::read_rgba16f(ctx, &pool, canvas).unwrap();
    assert_close(at(&px, W, 4, 4), [1.0, 0.0, 0.0, 1.0], 0.01);
    drop(imported);
    importer.retire(ctx, &mut pool, Timestamp::ZERO);
    ctx.wait_idle().unwrap();
    importer.retire(ctx, &mut pool, Timestamp::ZERO);
    assert_eq!(owner.held(), 0, "lease returned after the GPU finished");
}
