//! One small picture of a display or window for the target list (ADR-0077,
//! capture-port.md `thumbnail`): one WGC frame, scaled down on the GPU by its mip chain,
//! the nearest level read back and box-filtered to the asked size on the CPU.

use std::time::{Duration, Instant};

use rbs_capture::{
    CaptureError, ErrorDomain, REGION_GRAB_LONG, RegionGrab, THUMBNAIL_MAX, TargetId, Thumbnail,
};
use rbs_core::media::Size;

use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_RESOURCE_MISC_GENERATE_MIPS, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::core::{IInspectable, Interface};

use crate::win::{create_item, ensure_winrt, resolve};

/// The whole grab, including device and item creation (capture-port.md: about 1 s).
const BUDGET: Duration = Duration::from_secs(1);
/// How often the pool is asked for its first frame while waiting.
const POLL: Duration = Duration::from_millis(5);

/// A recoverable failure: the grab may work next time (a window that is minimised has
/// no frame; a busy GPU).
fn retry(e: &windows::core::Error) -> CaptureError {
    CaptureError::Backend {
        domain: ErrorDomain::HResult,
        code: i64::from(e.code().0),
        recoverable: true,
    }
}

fn timed_out() -> CaptureError {
    CaptureError::Backend {
        domain: ErrorDomain::HResult,
        // HRESULT_FROM_WIN32(ERROR_TIMEOUT)
        code: 0x8007_05B4_u32 as i32 as i64,
        recoverable: true,
    }
}

fn missing() -> windows::core::Error {
    windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER)
}

pub(crate) fn thumbnail(target: &TargetId, max: Size) -> Result<Thumbnail, CaptureError> {
    let limit = Size::new(
        max.width.min(THUMBNAIL_MAX.width),
        max.height.min(THUMBNAIL_MAX.height),
    );
    let (w, h, rgba) = grab_rgba(target, limit, "thumbnail")?;
    Thumbnail::new(w, h, rgba).ok_or(CaptureError::FormatUnsupported)
}

/// Region-section grab (ADR-0103): same path as a thumbnail, long side ≤ 960.
pub(crate) fn region_grab(target: &TargetId, max_long: u32) -> Result<RegionGrab, CaptureError> {
    let side = max_long.clamp(1, REGION_GRAB_LONG.max(1));
    let limit = Size::new(side, side);
    let (w, h, rgba) = grab_rgba(target, limit, "region_grab")?;
    RegionGrab::new(w, h, rgba).ok_or(CaptureError::FormatUnsupported)
}

fn grab_rgba(
    target: &TargetId,
    limit: Size,
    unsupported: &'static str,
) -> Result<(u32, u32, Vec<u8>), CaptureError> {
    let started = Instant::now();
    if limit.width == 0 || limit.height == 0 {
        return Err(CaptureError::FormatUnsupported);
    }
    let (resolved, _) = resolve(target).ok_or(CaptureError::TargetNotFound)?;
    ensure_winrt();
    if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
        return Err(CaptureError::Unsupported(unsupported));
    }
    let (device, context) = create_device().map_err(|e| retry(&e))?;
    let item = create_item(resolved).map_err(|e| retry(&e))?;
    let size = item.Size().map_err(|e| retry(&e))?;
    let dxgi: IDXGIDevice = device.cast().map_err(|e| retry(&e))?;
    // SAFETY: plain call on a live DXGI device.
    let winrt: IInspectable =
        unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }.map_err(|e| retry(&e))?;
    let winrt_device: IDirect3DDevice = winrt.cast().map_err(|e| retry(&e))?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        1,
        size,
    )
    .map_err(|e| retry(&e))?;
    let session = match pool.CreateCaptureSession(&item) {
        Ok(s) => s,
        Err(e) => {
            let _ = pool.Close();
            return Err(retry(&e));
        }
    };
    let close = || {
        let _ = session.Close();
        let _ = pool.Close();
    };
    // A grab must show nothing (capture-port.md): without a borderless capture the
    // yellow border would flash, so a system that cannot drop it gets no thumbnail.
    if session.SetIsBorderRequired(false).is_err() {
        close();
        return Err(CaptureError::Unsupported(unsupported));
    }
    let _ = session.SetIsCursorCaptureEnabled(false);
    if let Err(e) = session.StartCapture() {
        close();
        return Err(retry(&e));
    }
    let frame = loop {
        if let Ok(frame) = pool.TryGetNextFrame() {
            break Some(frame);
        }
        if started.elapsed() >= BUDGET {
            break None;
        }
        std::thread::sleep(POLL);
    };
    let Some(frame) = frame else {
        close();
        return Err(timed_out());
    };
    let picture = (|| -> windows::core::Result<Option<(u32, u32, Vec<u8>)>> {
        let content = frame.ContentSize()?;
        let (w, h) = (
            u32::try_from(content.Width)
                .unwrap_or(0)
                .min(u32::try_from(size.Width).unwrap_or(0)),
            u32::try_from(content.Height)
                .unwrap_or(0)
                .min(u32::try_from(size.Height).unwrap_or(0)),
        );
        let Some((tw, th)) = crate::fit_size(w, h, limit) else {
            return Ok(None);
        };
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        // SAFETY: the WGC surface is a D3D11 texture on `device`.
        let source: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let (level, lw, lh) = read_level(w, h, tw, th);
        let pixels = shrink_and_read(&device, &context, &source, (w, h), level, (lw, lh))?;
        let (data, pitch) = pixels;
        let rgba = crate::downscale_bgra(&data, lw, lh, pitch, tw, th);
        Ok(Some((tw, th, rgba)))
    })();
    let _ = frame.Close();
    close();
    match picture {
        Ok(Some(t)) => Ok(t),
        Ok(None) => Err(timed_out()),
        Err(e) => Err(retry(&e)),
    }
}

/// The smallest mip level of a `w` x `h` picture still at least `tw` x `th`, and its size.
fn read_level(w: u32, h: u32, tw: u32, th: u32) -> (u32, u32, u32) {
    let mut level = 0;
    let (mut lw, mut lh) = (w, h);
    while lw / 2 >= tw && lh / 2 >= th && lw > 1 && lh > 1 {
        level += 1;
        lw /= 2;
        lh /= 2;
    }
    (level, lw.max(1), lh.max(1))
}

fn create_device() -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    // SAFETY: valid out pointers; no adapter means the default hardware adapter.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((device.ok_or_else(missing)?, context.ok_or_else(missing)?))
}

/// Copies the `size` content of `source` into a mip chain, lets the GPU make the smaller
/// levels, and reads `level` (of `level_size`) back: its bytes and row pitch.
fn shrink_and_read(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    source: &ID3D11Texture2D,
    (w, h): (u32, u32),
    level: u32,
    (lw, lh): (u32, u32),
) -> windows::core::Result<(Vec<u8>, usize)> {
    let sample = DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
    };
    let chain_desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: level + 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: sample,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
    };
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: lw,
        Height: lh,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: sample,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut chain = None;
    let mut staging = None;
    let mut view: Option<ID3D11ShaderResourceView> = None;
    // SAFETY: valid descriptions and out pointers on a live device.
    unsafe {
        device.CreateTexture2D(&chain_desc, None, Some(&mut chain))?;
        device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
    }
    let (chain, staging) = (chain.ok_or_else(missing)?, staging.ok_or_else(missing)?);
    // SAFETY: the chain was made with the shader resource bind flag; no description
    // means the whole chain in its own format.
    unsafe { device.CreateShaderResourceView(&chain, None, Some(&mut view))? };
    let view = view.ok_or_else(missing)?;
    let region = D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: w,
        bottom: h,
        back: 1,
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: every resource lives on `device`, which `context` belongs to and only this
    // thread uses. The box lies inside the frame (the content size, at most the pool
    // size) and inside level 0 of the chain (`w` x `h`); `level` exists in the chain and
    // has the staging texture's size. The mapped memory is read only between Map and
    // Unmap, for `lh` rows of `RowPitch` bytes.
    let bytes = unsafe {
        context.CopySubresourceRegion(&chain, 0, 0, 0, 0, source, 0, Some(&region));
        context.GenerateMips(&view);
        context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &chain, level, None);
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let pitch = mapped.RowPitch as usize;
        let len = pitch * lh as usize;
        let data = std::slice::from_raw_parts(mapped.pData as *const u8, len).to_vec();
        context.Unmap(&staging, 0);
        (data, pitch)
    };
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_read_level_is_the_smallest_still_large_enough() {
        assert_eq!(super::read_level(3840, 2160, 192, 108), (4, 240, 135));
        assert_eq!(super::read_level(200, 100, 192, 96), (0, 200, 100));
        assert_eq!(super::read_level(1, 1, 1, 1), (0, 1, 1));
    }
}
