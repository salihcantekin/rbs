//! D3D12 native import conformance (spec docs/specs/gpu-compositor.md, "Native import").
//!
//! A D3D11 producer on the render adapter creates an NT shared BGRA texture and a shared
//! fence, as the WGC capture adapter does. The importer aliases it, the compositor
//! samples it, and the producer's lease comes back only after `retire` saw the GPU finish.
//! Skips when the render device is not D3D12.

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use common::{assert_close, at};
use rbs_core::media::{
    ColorSpace, D3dApi, Lease, LeaseOwner, NativeSurface, PixelFormat, Size, SurfaceSync,
    VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_gpu::{CanvasDesc, Compositor, Importer, Layer, TextureDesc, TexturePool, readback};
use windows::Win32::Foundation::{GENERIC_ALL, HANDLE, HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_FENCE_FLAG_SHARED, D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11CreateDevice, ID3D11Device, ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4,
    ID3D11Fence, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter,
    IDXGIFactory4, IDXGIResource1,
};
use windows::core::{Interface, PCWSTR};

#[derive(Default)]
struct Producer {
    count: AtomicI64,
}

// SAFETY: one exact counter; the test keeps the shared texture alive for its whole run.
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
        // SAFETY: the reference counted just above is handed to the lease.
        unsafe { Lease::adopt(Arc::clone(self) as Arc<dyn LeaseOwner>, 0) }
    }
    fn held(&self) -> i64 {
        self.count.load(Ordering::SeqCst)
    }
}

struct D3d11 {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    _texture: ID3D11Texture2D,
    fence: ID3D11Fence,
    texture_handle: HANDLE,
    fence_handle: HANDLE,
}

const W: u32 = 8;
const H: u32 = 8;

fn producer(luid: u64) -> D3d11 {
    // SAFETY: plain D3D11/DXGI calls on objects created here; out-pointers are valid locals.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1().unwrap();
        let adapter: IDXGIAdapter = factory
            .EnumAdapterByLuid(LUID {
                LowPart: luid as u32,
                HighPart: (luid >> 32) as i32,
            })
            .unwrap();
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .unwrap();
        let device = device.unwrap();
        let context = context.unwrap();

        // Pure red in BGRA.
        let pixels: Vec<u8> = (0..W * H).flat_map(|_| [0u8, 0, 255, 255]).collect();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED.0)
                as u32,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: W * 4,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        device
            .CreateTexture2D(&desc, Some(&init), Some(&mut texture))
            .unwrap();
        let texture = texture.unwrap();
        let texture_handle = texture
            .cast::<IDXGIResource1>()
            .unwrap()
            .CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                PCWSTR::null(),
            )
            .unwrap();

        let mut fence: Option<ID3D11Fence> = None;
        device
            .cast::<ID3D11Device5>()
            .unwrap()
            .CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)
            .unwrap();
        let fence = fence.unwrap();
        let fence_handle = fence
            .CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null())
            .unwrap();

        D3d11 {
            _device: device,
            context,
            _texture: texture,
            fence,
            texture_handle,
            fence_handle,
        }
    }
}

impl D3d11 {
    fn signal(&self, value: u64) {
        // SAFETY: the fence and context are live objects created in `producer`.
        unsafe {
            self.context
                .cast::<ID3D11DeviceContext4>()
                .unwrap()
                .Signal(&self.fence, value)
                .unwrap();
            self.context.Flush();
        }
    }

    fn frame(&self, luid: u64, value: u64, lease: Lease) -> VideoFrame {
        let format = VideoFormat::new(Size::new(W, H), PixelFormat::Bgra8, ColorSpace::SRGB);
        // SAFETY: the handles refer to live shared objects of the described kind and
        // format on adapter `luid`; the producer does not write the texture while the
        // lease is held (it never writes again in this test).
        unsafe {
            VideoFrame::native(
                format,
                Timestamp::ZERO,
                NativeSurface::d3d_shared_handle(
                    self.texture_handle.0 as u64,
                    true,
                    D3dApi::D3d11,
                    luid,
                ),
                SurfaceSync::timeline_fence(self.fence_handle.0 as u64, value),
                lease,
            )
        }
    }
}

/// The render adapter's LUID, or `None` (skip) when the device is not D3D12.
/// CI sets `RBS_REQUIRE_D3D12` on Windows so the import test cannot silently skip.
fn d3d12_luid(ctx: &rbs_gpu::GpuContext) -> Option<u64> {
    let luid = ctx.adapter_luid();
    if luid.is_none() {
        assert!(
            std::env::var_os("RBS_REQUIRE_D3D12").is_none_or(|v| v.is_empty()),
            "RBS_REQUIRE_D3D12 is set but the render device is not D3D12"
        );
        eprintln!("SKIPPED: render device is not D3D12");
    }
    luid
}

#[test]
fn shared_texture_is_aliased_rendered_and_handed_back() {
    let Some(ctx) = common::gpu() else { return };
    let Some(luid) = d3d12_luid(ctx) else { return };
    let d3d11 = producer(luid);
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

    for value in 1..=2u64 {
        d3d11.signal(value);
        let frame = importer
            .import(ctx, &mut pool, d3d11.frame(luid, value, owner.lease()))
            .unwrap();
        let rbs_core::media::FramePayload::Texture(id) = *frame.payload() else {
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

        assert_eq!(
            owner.held(),
            1,
            "lease held while a frame references the surface"
        );
        drop(frame);
        importer.retire(ctx, &mut pool); // schedules the hand-back
        ctx.wait_idle().unwrap();
        importer.retire(ctx, &mut pool); // sees it complete
        assert_eq!(owner.held(), 0, "lease returned after the GPU finished");
        assert_eq!(importer.held_leases(), 0);
    }
    assert_eq!(
        ctx.metrics().surfaces_opened() - opened_before,
        1,
        "the second frame on the same surface opens nothing"
    );
    assert_eq!(importer.open_surfaces(), 1);
}

#[test]
fn wrong_adapter_and_keyed_mutex_are_rejected() {
    let Some(ctx) = common::gpu() else { return };
    let Some(luid) = d3d12_luid(ctx) else { return };
    let d3d11 = producer(luid);
    let owner = Arc::new(Producer::default());
    let mut pool = TexturePool::new();
    let mut importer = Importer::new();

    let wrong = d3d11.frame(luid ^ 1, 0, owner.lease());
    assert_eq!(
        importer.import(ctx, &mut pool, wrong).unwrap_err(),
        rbs_gpu::GpuError::WrongAdapter
    );
    assert_eq!(owner.held(), 0, "a rejected frame hands its lease back");

    let format = VideoFormat::new(Size::new(W, H), PixelFormat::Bgra8, ColorSpace::SRGB);
    // SAFETY: same live objects as in `D3d11::frame`.
    let keyed = unsafe {
        VideoFrame::native(
            format,
            Timestamp::ZERO,
            NativeSurface::d3d_shared_handle(
                d3d11.texture_handle.0 as u64,
                true,
                D3dApi::D3d11,
                luid,
            ),
            SurfaceSync::keyed_mutex(0, 0),
            owner.lease(),
        )
    };
    assert!(matches!(
        importer.import(ctx, &mut pool, keyed),
        Err(rbs_gpu::GpuError::Unsupported(_))
    ));
    assert_eq!(owner.held(), 0);
}
