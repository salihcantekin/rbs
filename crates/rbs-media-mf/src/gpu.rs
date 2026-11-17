//! Native pictures: Media Foundation decodes on the render adapter and each picture is
//! copied on the GPU into a shared D3D11 texture (ADR-0108 decision 4, capture port
//! native surface rules).
//!
//! - The decoder runs on a D3D11 device of the adapter named by the `DecodeTarget`
//!   LUID, behind an `IMFDXGIDeviceManager`. The source reader's video processor gives
//!   `B8G8R8A8` textures from its own pool, which cannot be shared, so each picture is
//!   copied (cropped to the display aperture) into a texture of [`Pool`], created with
//!   an NT handle. The worker waits on a D3D11 fence for the copy, so the picture is
//!   handed out `SurfaceSync::Ready`.
//! - A track the source reader leaves on its side or upside down (the decoder's
//!   `left_to_turn`) is turned upright in that same GPU pass: a D3D11 video processor
//!   blit with the stream's rotation replaces the copy (media-decode port invariant
//!   12). Only then are the slot textures render targets. A device whose video
//!   processor cannot rotate `B8G8R8A8`, or any failure of that blit, falls back to the
//!   copy path for the rest of the decoder, which turns the picture in memory. An
//!   upright track keeps the plain copy and makes no video processor.
//! - Slots are counted by `rbs_capture::handoff::Ring`: the worker claims a slot
//!   (reference 1) and hands that reference to the frame's lease; the importer and the
//!   picture node release it. Only a free slot is written. Neither `retain` nor
//!   `release` locks or frees.
//! - A slot texture replaced for another size, and the pool of a dropped decoder, go to
//!   a graveyard: kept while a lease refers to them, then for [`HANDLE_GRACE`], so a
//!   handle value an importer may have cached is not reused; freed on the janitor
//!   thread, never on the render thread.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rbs_capture::handoff::{Grave, Graveyard, Ring};
use rbs_core::media::{
    ColorSpace, D3dApi, Lease, LeaseOwner, NativeSurface, PixelFormat, Size, SurfaceSync,
    VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use windows::Win32::Foundation::RECT;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, LUID, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_FENCE_FLAG_NONE,
    D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CAPS,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_ROTATION, D3D11_VIDEO_PROCESSOR_ROTATION_90,
    D3D11_VIDEO_PROCESSOR_ROTATION_180, D3D11_VIDEO_PROCESSOR_ROTATION_270,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11CreateDevice,
    ID3D11Device, ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Multithread,
    ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter,
    IDXGIFactory4, IDXGIResource1,
};
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIBuffer, IMFDXGIDeviceManager, IMFSample, MFCreateDXGIDeviceManager,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{Interface, PCWSTR};

use rbs_media_decode::{DecodeError, Picture, Rotation};

use crate::Crop;

/// How long a handed-out texture handle stays open after its last use (capture-port.md,
/// native surface rules: at least 10 s).
const HANDLE_GRACE: Duration = Duration::from_secs(10);

/// How often the graveyard's janitor checks stopped decoders.
const JANITOR_PERIOD: Duration = Duration::from_secs(1);

/// Slots of one decoder: more than the media source's surface budget (18), so a claim
/// fails only when consumers keep more pictures than the source allows.
const SLOTS: usize = 24;

/// How long the worker waits for one GPU copy before it gives up on the picture.
const COPY_WAIT_MS: u32 = 2_000;

static GRAVEYARD: Graveyard = Graveyard::new("rbs-media-mf-janitor", HANDLE_GRACE, JANITOR_PERIOD);

fn failed(what: &str, e: &windows::core::Error) -> DecodeError {
    DecodeError::Failed(format!("{what}: {e}"))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn close_handle(value: u64) {
    if value != 0 {
        // SAFETY: `value` is an NT handle this adapter created and has not closed.
        let _ = unsafe { CloseHandle(HANDLE(value as usize as *mut _)) };
    }
}

/// The adapter of `luid` (the `GpuContext::adapter_luid` encoding: high part in the
/// upper 32 bits).
fn adapter_for(luid: u64) -> windows::core::Result<IDXGIAdapter> {
    let luid = LUID {
        LowPart: luid as u32,
        HighPart: (luid >> 32) as u32 as i32,
    };
    // SAFETY: plain factory calls; the LUID is passed by value.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1()?;
        factory.EnumAdapterByLuid(luid)
    }
}

/// One slot's texture and its NT handle. Closes the handle when dropped.
struct SlotTexture {
    texture: ID3D11Texture2D,
    handle: u64,
    size: (u32, u32),
}

impl Drop for SlotTexture {
    fn drop(&mut self) {
        close_handle(self.handle);
    }
}

/// A D3D11 object moved between the worker and the janitor.
///
/// D3D11 objects of a device created with multithread protection may be released on
/// any thread, and the adapter never uses a wrapped object from two threads at once.
struct Sendable<T>(T);

// SAFETY: see the type's doc.
unsafe impl<T> Send for Sendable<T> {}

/// The slots and the textures they name. Frames hold an `Arc` of it in their lease.
struct Pool {
    ring: Ring,
    /// One entry per slot. Only the worker locks it, never a lease.
    textures: Mutex<Vec<Option<Sendable<SlotTexture>>>>,
}

// SAFETY: delegates to `Ring`, whose counts are exact: the worker claims a slot (one
// reference) and hands exactly that reference to one `Lease`; every clone retains and
// every drop releases once. A slot's texture and handle stay in `textures` while the
// slot is referenced: the worker writes or replaces a texture only in a slot it has
// claimed (refs 0 to 1), and the pool lives as long as any lease (which holds an
// `Arc<Pool>`). Neither method locks.
unsafe impl LeaseOwner for Pool {
    fn retain(&self, token: u64) {
        self.ring.retain(token);
    }

    fn release(&self, token: u64) {
        self.ring.release(token);
    }
}

/// The decode device, the device manager the source reader uses, and the pool.
pub(crate) struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext4,
    manager: IMFDXGIDeviceManager,
    fence: ID3D11Fence,
    fence_value: u64,
    event: HANDLE,
    luid: u64,
    pool: Arc<Pool>,
    owner: Arc<dyn LeaseOwner>,
    /// The video processor that turns pictures upright, made on the first rotated
    /// picture and kept while the sizes stay the same.
    rotator: Option<Rotator>,
    /// The video processor could not turn a picture: rotated pictures are copied
    /// (and turned in memory) for the rest of this decoder.
    rotation_failed: bool,
}

/// A D3D11 video processor for one input and output size.
struct Rotator {
    sizes: [u32; 4],
    video: ID3D11VideoDevice,
    context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
}

fn d3d_rotation(rotation: Rotation) -> D3D11_VIDEO_PROCESSOR_ROTATION {
    // The D3D11 video processor turns clockwise, as `Rotation` does.
    match rotation {
        Rotation::Clockwise90 => D3D11_VIDEO_PROCESSOR_ROTATION_90,
        Rotation::Half => D3D11_VIDEO_PROCESSOR_ROTATION_180,
        Rotation::Clockwise270 => D3D11_VIDEO_PROCESSOR_ROTATION_270,
        _ => windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
    }
}

impl Gpu {
    /// A decode device on the adapter of `luid`, or an error when the adapter is gone
    /// or lacks D3D11 video support or fences (the caller copies instead).
    pub(crate) fn new(luid: u64) -> Result<Self, DecodeError> {
        let adapter = adapter_for(luid).map_err(|e| failed("render adapter", &e))?;
        let mut device = None;
        let mut immediate = None;
        // SAFETY: valid out pointers; the adapter is live.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut immediate),
            )
        }
        .map_err(|e| failed("D3D11 device", &e))?;
        let missing = || DecodeError::Failed("D3D11CreateDevice gave nothing".into());
        let device: ID3D11Device = device.ok_or_else(missing)?;
        let context: ID3D11DeviceContext4 = immediate
            .ok_or_else(missing)?
            .cast()
            .map_err(|e| failed("D3D11 fences", &e))?;
        // Media Foundation uses the device on its own threads.
        let multithread: ID3D11Multithread =
            device.cast().map_err(|e| failed("D3D11 multithread", &e))?;
        // SAFETY: plain call on a live device; returns the previous setting.
        let _ = unsafe { multithread.SetMultithreadProtected(true) };
        let device5: ID3D11Device5 = device.cast().map_err(|e| failed("D3D11 fences", &e))?;
        let mut fence: Option<ID3D11Fence> = None;
        // SAFETY: a valid out pointer on a live device.
        unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence) }
            .map_err(|e| failed("D3D11 fence", &e))?;
        let fence = fence.ok_or_else(missing)?;
        let mut token = 0u32;
        let mut manager = None;
        // SAFETY: valid out pointers; the device outlives the manager's use of it (both
        // are owned here and the manager holds its own reference).
        unsafe {
            MFCreateDXGIDeviceManager(&mut token, &mut manager)
                .map_err(|e| failed("DXGI device manager", &e))?;
        }
        let manager: IMFDXGIDeviceManager = manager.ok_or_else(missing)?;
        // SAFETY: the token is the one the manager just gave.
        unsafe { manager.ResetDevice(&device, token) }
            .map_err(|e| failed("DXGI device manager", &e))?;
        // SAFETY: an unnamed auto-reset event; closed in `Drop`.
        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|e| failed("event", &e))?;
        let pool = Arc::new(Pool {
            ring: Ring::new(&[0; SLOTS]),
            textures: Mutex::new((0..SLOTS).map(|_| None).collect()),
        });
        let owner: Arc<dyn LeaseOwner> = Arc::clone(&pool) as Arc<dyn LeaseOwner>;
        Ok(Self {
            device,
            context,
            manager,
            fence,
            fence_value: 0,
            event,
            luid,
            pool,
            owner,
            rotator: None,
            rotation_failed: false,
        })
    }

    /// The manager to set as the source reader's `MF_SOURCE_READER_D3D_MANAGER`.
    pub(crate) fn manager(&self) -> &IMFDXGIDeviceManager {
        &self.manager
    }

    /// Copies the picture of `sample` into a free slot and returns it as a native
    /// picture, or `Ok(None)` when the sample is not a `B8G8R8A8` texture on this
    /// device or every slot is referenced (the caller copies to system memory instead).
    pub(crate) fn picture(
        &mut self,
        sample: &IMFSample,
        crop: Crop,
        rotation: Rotation,
    ) -> Result<Option<Picture>, DecodeError> {
        // SAFETY: plain calls on a live sample and its buffer; `GetResource` writes an
        // owned reference of the requested interface, which `from_raw` takes over.
        let (source, subresource) = unsafe {
            let Ok(buffer) = sample.GetBufferByIndex(0) else {
                return Ok(None);
            };
            let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() else {
                return Ok(None);
            };
            let mut raw = std::ptr::null_mut();
            if dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw).is_err() || raw.is_null() {
                return Ok(None);
            }
            let texture = ID3D11Texture2D::from_raw(raw);
            let Ok(index) = dxgi.GetSubresourceIndex() else {
                return Ok(None);
            };
            (texture, index)
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a valid out pointer on a live texture.
        unsafe { source.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            || desc.Width < crop.x + crop.width
            || desc.Height < crop.y + crop.height
        {
            return Ok(None);
        }
        // The video processor's input view names an array slice, which is the
        // subresource index only with one mip level.
        if rotation != Rotation::None && (self.rotation_failed || desc.MipLevels != 1) {
            return Ok(None);
        }
        let size = rotation.upright(crop.width, crop.height);
        let Some(slot) = self.pool.ring.claim() else {
            return Ok(None);
        };
        let full = (desc.Width, desc.Height);
        let handle = match self.write_slot(slot, &source, subresource, full, crop, rotation) {
            Ok(handle) => handle,
            Err(_) if rotation != Rotation::None => {
                // The video processor cannot turn this picture: copy it instead.
                self.pool.ring.abandon(slot);
                self.rotator = None;
                self.rotation_failed = true;
                return Ok(None);
            }
            Err(e) => {
                self.pool.ring.abandon(slot);
                return Err(e);
            }
        };
        // SAFETY: `claim` counted exactly one reference to `slot`, handed over here.
        let lease = unsafe { Lease::adopt(Arc::clone(&self.owner), slot as u64) };
        let format = VideoFormat::new(
            Size::new(size.0, size.1),
            PixelFormat::Bgra8,
            ColorSpace::SRGB,
        );
        let surface = NativeSurface::d3d_shared_handle(handle, true, D3dApi::D3d11, self.luid);
        // SAFETY: the handle names the slot's texture, which the pool keeps (with the
        // handle open) while the lease's slot is referenced; the worker writes it only
        // after every reference is released, and a replaced or dropped texture waits
        // out the grace in the graveyard. The copy has completed (fence waited).
        let frame = unsafe {
            VideoFrame::native(format, Timestamp::ZERO, surface, SurfaceSync::Ready, lease)
        };
        Picture::native(frame)
            .map(Some)
            .ok_or_else(|| DecodeError::Failed("a native picture".into()))
    }

    /// Writes the cropped picture, turned upright, into the claimed `slot`, replacing
    /// its texture first when the size changed, and waits for the GPU. Returns the
    /// slot's handle.
    fn write_slot(
        &mut self,
        slot: usize,
        source: &ID3D11Texture2D,
        subresource: u32,
        full: (u32, u32),
        crop: Crop,
        rotation: Rotation,
    ) -> Result<u64, DecodeError> {
        let size = rotation.upright(crop.width, crop.height);
        let mut textures = lock(&self.pool.textures);
        let entry = textures
            .get_mut(slot)
            .ok_or_else(|| DecodeError::Failed("no picture slot".into()))?;
        if entry.as_ref().is_none_or(|t| t.0.size != size) {
            let (texture, handle) =
                create_shared_texture(&self.device, size, rotation != Rotation::None)
                    .map_err(|e| failed("texture", &e))?;
            if let Some(old) = entry.replace(Sendable(SlotTexture {
                texture,
                handle,
                size,
            })) {
                // The old handle may be cached by an importer: keep it open for the
                // grace so its value is not reused.
                GRAVEYARD.bury(Box::new(TextureGrave(old)));
            }
        }
        let target = entry
            .as_ref()
            .ok_or_else(|| DecodeError::Failed("no picture slot".into()))?;
        if rotation != Rotation::None {
            let texture = target.0.texture.clone();
            let handle = target.0.handle;
            drop(textures);
            self.rotate(source, subresource, full, crop, &texture, rotation)?;
            self.wait_for_gpu()?;
            return Ok(handle);
        }
        let region = D3D11_BOX {
            left: crop.x,
            top: crop.y,
            front: 0,
            right: crop.x + crop.width,
            bottom: crop.y + crop.height,
            back: 1,
        };
        // SAFETY: both textures are live on this device; the destination slot is
        // claimed (no reader) and sized to the crop, and the crop lies inside the
        // decoded frame (`Crop::of` clamps it). The device is multithread protected, so
        // the immediate context is serialized with Media Foundation's use of it.
        unsafe {
            self.context.CopySubresourceRegion(
                &target.0.texture,
                0,
                0,
                0,
                0,
                source,
                subresource,
                Some(&region),
            );
        }
        let handle = target.0.handle;
        drop(textures);
        self.wait_for_gpu()?;
        Ok(handle)
    }

    /// Signals the fence after the work queued so far and waits for it.
    fn wait_for_gpu(&mut self) -> Result<(), DecodeError> {
        self.fence_value += 1;
        // SAFETY: plain calls on the live context, fence and event; the device is
        // multithread protected.
        unsafe {
            self.context
                .Signal(&self.fence, self.fence_value)
                .map_err(|e| failed("fence", &e))?;
            self.context.Flush();
            self.fence
                .SetEventOnCompletion(self.fence_value, self.event)
                .map_err(|e| failed("fence", &e))?;
            if WaitForSingleObject(self.event, COPY_WAIT_MS) != WAIT_OBJECT_0 {
                return Err(DecodeError::Failed(
                    "the GPU did not finish copying a picture".into(),
                ));
            }
        }
        Ok(())
    }

    /// Queues one video processor blit of `crop` of `source` into `target`, turned by
    /// `rotation`. Makes the processor on first use and again when a size changes.
    fn rotate(
        &mut self,
        source: &ID3D11Texture2D,
        subresource: u32,
        full: (u32, u32),
        crop: Crop,
        target: &ID3D11Texture2D,
        rotation: Rotation,
    ) -> Result<(), DecodeError> {
        let (out_w, out_h) = rotation.upright(crop.width, crop.height);
        let sizes = [full.0, full.1, out_w, out_h];
        if self.rotator.as_ref().is_none_or(|r| r.sizes != sizes) {
            self.rotator = None;
            let video: ID3D11VideoDevice = self
                .device
                .cast()
                .map_err(|e| failed("D3D11 video device", &e))?;
            let context: ID3D11VideoContext = self
                .context
                .cast()
                .map_err(|e| failed("D3D11 video context", &e))?;
            let rate = DXGI_RATIONAL {
                Numerator: 0,
                Denominator: 1,
            };
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: full.0,
                InputHeight: full.1,
                OutputFrameRate: rate,
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            // SAFETY: a valid description on a live video device.
            let enumerator = unsafe { video.CreateVideoProcessorEnumerator(&content) }
                .map_err(|e| failed("video processor", &e))?;
            let mut caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
            // SAFETY: plain queries on a live enumerator with valid out pointers.
            let (input_ok, output_ok) = unsafe {
                enumerator
                    .GetVideoProcessorCaps(&mut caps)
                    .map_err(|e| failed("video processor", &e))?;
                let support = enumerator
                    .CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)
                    .map_err(|e| failed("video processor", &e))?;
                (
                    support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 != 0,
                    support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 != 0,
                )
            };
            let rotates =
                caps.FeatureCaps & D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION.0 as u32 != 0;
            if !(rotates && input_ok && output_ok) {
                return Err(DecodeError::Failed(
                    "the video processor cannot turn this picture".into(),
                ));
            }
            // SAFETY: a live enumerator of this device.
            let processor = unsafe { video.CreateVideoProcessor(&enumerator, 0) }
                .map_err(|e| failed("video processor", &e))?;
            self.rotator = Some(Rotator {
                sizes,
                video,
                context,
                enumerator,
                processor,
            });
        }
        let r = self
            .rotator
            .as_ref()
            .ok_or_else(|| DecodeError::Failed("no video processor".into()))?;
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    // `picture` checked that the texture has one mip level, so the
                    // subresource index is the array slice.
                    ArraySlice: subresource,
                },
            },
        };
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut input: Option<ID3D11VideoProcessorInputView> = None;
        let mut output: Option<ID3D11VideoProcessorOutputView> = None;
        // SAFETY: live textures of this device and valid descriptions; the views are
        // released when they drop, after the blit is queued.
        unsafe {
            r.video
                .CreateVideoProcessorInputView(source, &r.enumerator, &input_desc, Some(&mut input))
                .map_err(|e| failed("video processor input", &e))?;
            r.video
                .CreateVideoProcessorOutputView(
                    target,
                    &r.enumerator,
                    &output_desc,
                    Some(&mut output),
                )
                .map_err(|e| failed("video processor output", &e))?;
        }
        let output =
            output.ok_or_else(|| DecodeError::Failed("no video processor output".into()))?;
        let rect = |x: u32, y: u32, w: u32, h: u32| RECT {
            left: x as i32,
            top: y as i32,
            right: (x + w) as i32,
            bottom: (y + h) as i32,
        };
        let source_rect = rect(crop.x, crop.y, crop.width, crop.height);
        let target_rect = rect(0, 0, out_w, out_h);
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(input),
            ..Default::default()
        };
        let streams = [stream];
        // SAFETY: the processor, views and context belong to this device; the stream
        // array lives across the call. The device is multithread protected.
        let blit = unsafe {
            r.context
                .VideoProcessorSetStreamAutoProcessingMode(&r.processor, 0, false);
            r.context.VideoProcessorSetStreamFrameFormat(
                &r.processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            r.context
                .VideoProcessorSetStreamSourceRect(&r.processor, 0, true, Some(&source_rect));
            r.context
                .VideoProcessorSetStreamDestRect(&r.processor, 0, true, Some(&target_rect));
            r.context
                .VideoProcessorSetOutputTargetRect(&r.processor, true, Some(&target_rect));
            r.context.VideoProcessorSetStreamRotation(
                &r.processor,
                0,
                true,
                d3d_rotation(rotation),
            );
            r.context
                .VideoProcessorBlt(&r.processor, &output, 0, &streams)
        };
        // The stream array held our only reference to the input view.
        let [stream] = streams;
        drop(std::mem::ManuallyDrop::into_inner(stream.pInputSurface));
        blit.map_err(|e| failed("video processor blit", &e))
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        GRAVEYARD.bury(Box::new(PoolGrave(Arc::clone(&self.pool))));
        // SAFETY: the event was created in `new` and is closed once, here.
        let _ = unsafe { CloseHandle(self.event) };
    }
}

fn create_shared_texture(
    device: &ID3D11Device,
    (width, height): (u32, u32),
    render_target: bool,
) -> windows::core::Result<(ID3D11Texture2D, u64)> {
    // A render target only for a turned picture, written by the video processor.
    let bind = if render_target {
        D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0
    } else {
        D3D11_BIND_SHADER_RESOURCE.0
    };
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width.max(1),
        Height: height.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
    };
    let mut texture = None;
    // SAFETY: `desc` is a valid description and `texture` a valid out pointer.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    let texture = texture
        .ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER))?;
    let resource: IDXGIResource1 = texture.cast()?;
    // SAFETY: plain call on a live resource; no name or security attributes.
    let handle = unsafe {
        resource.CreateSharedHandle(
            None,
            DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
            PCWSTR::null(),
        )?
    };
    Ok((texture, handle.0 as usize as u64))
}

/// A slot texture replaced for another size. Its slot was claimed by the worker, so
/// nobody references it; it is kept a full grace from burial.
struct TextureGrave(Sendable<SlotTexture>);

impl Grave for TextureGrave {
    fn is_idle(&self) -> bool {
        true
    }
    fn bytes(&self) -> u64 {
        let (w, h) = self.0.0.size;
        u64::from(w) * u64::from(h) * 4
    }
}

/// The pool of a dropped decoder. Kept while pictures reference it (so the last lease
/// never frees anything on the render thread) and for the grace after that.
struct PoolGrave(Arc<Pool>);

impl Grave for PoolGrave {
    fn is_idle(&self) -> bool {
        self.0.ring.is_idle()
    }
    fn bytes(&self) -> u64 {
        0
    }
}
