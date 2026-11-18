//! Native input (ADR-0147 decision 7): NVENC reads exported D3D12 buffers in place.
//!
//! Each shared buffer (`NativeSurface::D3dSharedBuffer`) is imported once into the
//! session's CUDA context as external memory, mapped as one linear device pointer and
//! registered with NVENC as a `CUDADEVICEPTR` resource (luma rows, then the interleaved
//! chroma rows at the same pitch, as NVENC expects). The import is cached by the NT
//! handle value: the exporter keeps its handles open until the session is dropped
//! (ADR-0147 decision 8), so a value names the same buffer for the session's life. Each
//! frame's shared fence is imported once as an external semaphore; before a picture is
//! encoded, the session's CUDA stream waits for the frame's fence value, and NVENC reads
//! its input on that stream (`nvEncSetIOCudaStreams`), so the wait stays on the GPU.
//!
//! A buffer is mapped while at least one slot in flight encodes it (a repeated picture
//! is the same buffer in several slots) and unmapped when the last of those outputs is
//! read. Everything is freed in [`Native::close`], when the session is dropped on the
//! control thread.
//!
//! Hot path: [`Native::attach`] and [`Native::release`] allocate nothing once every
//! buffer of the exporter's ring was seen (the caches are reserved up front).

use core::ffi::{c_char, c_int, c_uint, c_void};
use std::ptr;

use rbs_codec::CreateError;
use rbs_codec_hw::session::EngineError;
use rbs_core::media::{FramePayload, NativeSurface, SurfaceSync, VideoFrame};

use crate::loader::{Api, Interop};
use crate::sys::{self, cuda};

/// Shared buffers one session keeps imported: an exporter's ring is at most 32 buffers.
const MAX_BUFFERS: usize = 32;
/// Shared fences one session keeps imported (one per exporter).
const MAX_FENCES: usize = 4;

/// The LUID of CUDA device `ordinal` as a little-endian `u64` (the `adapter_luid` of a
/// native surface), or `None` when the driver cannot tell.
pub(crate) fn device_luid(api: &Api, ordinal: c_int) -> Option<[u8; 8]> {
    let interop = api.interop.as_ref()?;
    let mut dev: cuda::CUdevice = 0;
    // SAFETY: valid out-pointer; `cuInit` succeeded when the API was loaded.
    if unsafe { (api.cuda.device_get)(&mut dev, ordinal) } != cuda::CUDA_SUCCESS {
        return None;
    }
    let mut luid = [0 as c_char; 8];
    let mut mask: c_uint = 0;
    // SAFETY: `luid` holds the 8 bytes the call writes; `mask` is a valid out-pointer.
    let r = unsafe { (interop.device_get_luid)(luid.as_mut_ptr(), &mut mask, dev) };
    (r == cuda::CUDA_SUCCESS).then(|| luid.map(|b| b as u8))
}

/// The CUDA device whose LUID is `luid`, if any.
pub(crate) fn ordinal_of(api: &Api, luid: [u8; 8]) -> Option<c_int> {
    let mut count: c_int = 0;
    // SAFETY: valid out-pointer; `cuInit` succeeded.
    if unsafe { (api.cuda.device_get_count)(&mut count) } != cuda::CUDA_SUCCESS {
        return None;
    }
    (0..count).find(|&i| device_luid(api, i) == Some(luid))
}

struct Imported {
    handle: u64,
    memory: cuda::CUexternalMemory,
    ptr: cuda::CUdeviceptr,
    registered: sys::NV_ENC_REGISTERED_PTR,
    /// Mapped input while `maps > 0`.
    mapped: sys::NV_ENC_INPUT_PTR,
    maps: u32,
}

struct Fence {
    handle: u64,
    semaphore: cuda::CUexternalSemaphore,
}

/// What a slot encodes: the mapped input and its pitch.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Attached {
    pub handle: u64,
    pub input: sys::NV_ENC_INPUT_PTR,
    pub pitch: u32,
}

/// The native input state of one session. Every method runs with the session's CUDA
/// context current on the calling thread.
pub(crate) struct Native {
    api: &'static Api,
    interop: &'static Interop,
    /// LUID of the session's CUDA device, as a little-endian `u64`.
    luid: u64,
    /// The stream NVENC reads its input on; boxed so the address NVENC was given stays
    /// valid when the engine moves.
    stream: Box<cuda::CUstream>,
    buffers: Vec<Imported>,
    fences: Vec<Fence>,
    width: u32,
    height: u32,
    buffer_format: sys::NV_ENC_BUFFER_FORMAT,
}

impl Native {
    /// Creates the input stream and gives it to the NVENC session `encoder` on CUDA device
    /// `ordinal`.
    pub(crate) fn open(
        api: &'static Api,
        encoder: *mut c_void,
        ordinal: c_int,
        width: u32,
        height: u32,
        buffer_format: sys::NV_ENC_BUFFER_FORMAT,
    ) -> Result<Self, CreateError> {
        let unsupported = |what: &str| CreateError::Unsupported { what: what.into() };
        let interop = api
            .interop
            .as_ref()
            .ok_or_else(|| unsupported("the CUDA driver has no external memory"))?;
        let luid = device_luid(api, ordinal)
            .map(u64::from_le_bytes)
            .ok_or_else(|| unsupported("the CUDA device has no LUID"))?;
        let set_streams = api
            .nvenc
            .0
            .nvEncSetIOCudaStreams
            .ok_or_else(|| unsupported("nvEncSetIOCudaStreams missing"))?;
        let mut stream = Box::new(ptr::null_mut());
        // SAFETY: valid out-pointer; the session's context is current.
        let r = unsafe { (interop.stream_create)(&mut *stream, cuda::CU_STREAM_NON_BLOCKING) };
        if r != cuda::CUDA_SUCCESS {
            return Err(CreateError::Device(format!("cuStreamCreate failed ({r})")));
        }
        let native = Self {
            api,
            interop,
            luid,
            stream,
            buffers: Vec::with_capacity(MAX_BUFFERS),
            fences: Vec::with_capacity(MAX_FENCES),
            width,
            height,
            buffer_format,
        };
        let stream_ptr: *mut cuda::CUstream = ptr::from_ref(&*native.stream).cast_mut();
        // SAFETY: open session; both arguments point at the boxed stream, which lives
        // until `close` and does not move (it is boxed).
        let s = unsafe { set_streams(encoder, stream_ptr.cast(), stream_ptr.cast()) };
        if s != sys::NV_ENC_SUCCESS {
            // Dropping `native` here would leak the stream: destroy it.
            // SAFETY: the stream was created above and nothing uses it.
            let _ = unsafe { (interop.stream_destroy)(*native.stream) };
            return Err(CreateError::Device(format!(
                "nvEncSetIOCudaStreams failed ({s})"
            )));
        }
        Ok(native)
    }

    /// Imports (once), maps and fences the exported buffer of `frame` for the next encode.
    /// `Unsupported` for a frame of another kind, adapter or layout.
    pub(crate) fn attach(
        &mut self,
        encoder: *mut c_void,
        frame: &VideoFrame,
    ) -> Result<Attached, EngineError> {
        let FramePayload::Native { surface, sync, .. } = frame.payload() else {
            return Err(EngineError::Unsupported);
        };
        let NativeSurface::D3dSharedBuffer {
            handle,
            size,
            adapter_luid,
            planes,
            ..
        } = *surface
        else {
            return Err(EngineError::Unsupported);
        };
        let pitch = planes[0].stride;
        // NVENC reads a CUDA pointer as luma rows, then chroma rows right after `height`
        // luma rows, both at the registered pitch (a multiple of 4).
        let layout_ok = planes[0].offset == 0
            && planes[1].stride == pitch
            && pitch % 4 == 0
            && planes[1].offset == pitch * self.height as usize
            && (planes[1].offset + pitch * (self.height as usize / 2)) as u64 <= size;
        if adapter_luid != self.luid || !layout_ok {
            return Err(EngineError::Unsupported);
        }
        let pitch = u32::try_from(pitch).map_err(|_| EngineError::Unsupported)?;
        let index = match self.buffers.iter().position(|b| b.handle == handle) {
            Some(i) => i,
            None => self.import(encoder, handle, size, pitch)?,
        };
        let funcs = &self.api.nvenc.0;
        let b = &mut self.buffers[index];
        if b.maps == 0 {
            let map = funcs
                .nvEncMapInputResource
                .ok_or(EngineError::Failed("nvEncMapInputResource", -1))?;
            let mut params = sys::NV_ENC_MAP_INPUT_RESOURCE {
                version: sys::NV_ENC_MAP_INPUT_RESOURCE_VER,
                registeredResource: b.registered,
                ..Default::default()
            };
            // SAFETY: open session; `registered` was registered with it and is unmapped.
            let s = unsafe { map(encoder, &mut params) };
            if s != sys::NV_ENC_SUCCESS || params.mappedResource.is_null() {
                return Err(EngineError::Failed("nvEncMapInputResource", i64::from(s)));
            }
            b.mapped = params.mappedResource;
        }
        b.maps += 1;
        let attached = Attached {
            handle,
            input: b.mapped,
            pitch,
        };
        if let Err(e) = self.wait(*sync) {
            self.release(encoder, handle);
            return Err(e);
        }
        Ok(attached)
    }

    /// Makes the input stream wait for `sync` on the GPU.
    fn wait(&mut self, sync: SurfaceSync) -> Result<(), EngineError> {
        let (handle, value) = match sync {
            SurfaceSync::Ready => return Ok(()),
            SurfaceSync::TimelineFence { handle, value, .. } => (handle, value),
            _ => return Err(EngineError::Unsupported),
        };
        let semaphore = match self.fences.iter().find(|f| f.handle == handle) {
            Some(f) => f.semaphore,
            None => {
                if self.fences.len() == MAX_FENCES {
                    return Err(EngineError::Broken("too many shared fences"));
                }
                let desc = cuda::ExternalSemaphoreHandleDesc {
                    kind: cuda::CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_D3D12_FENCE,
                    handle: cuda::Win32Handle {
                        handle: handle as usize as *mut c_void,
                        name: ptr::null(),
                    },
                    flags: 0,
                    reserved: [0; 16],
                };
                let mut semaphore: cuda::CUexternalSemaphore = ptr::null_mut();
                // SAFETY: `desc` names a shared D3D12 fence handle that the exporter keeps
                // open while its frames live (VideoFrame::exported contract); CUDA takes its
                // own reference and does not take the handle.
                let r = unsafe { (self.interop.import_semaphore)(&mut semaphore, &desc) };
                if r != cuda::CUDA_SUCCESS {
                    return Err(EngineError::Failed(
                        "cuImportExternalSemaphore",
                        i64::from(r),
                    ));
                }
                // Within the reserved capacity: no allocation.
                self.fences.push(Fence { handle, semaphore });
                semaphore
            }
        };
        let params = cuda::ExternalSemaphoreWaitParams {
            fence_value: value,
            params_reserved: [0; 16],
            flags: 0,
            reserved: [0; 16],
        };
        // SAFETY: one live semaphore and one parameter block; the stream belongs to this
        // session's context. The call only enqueues the wait.
        let r = unsafe { (self.interop.wait_semaphores)(&semaphore, &params, 1, *self.stream) };
        if r != cuda::CUDA_SUCCESS {
            return Err(EngineError::Failed(
                "cuWaitExternalSemaphoresAsync",
                i64::from(r),
            ));
        }
        Ok(())
    }

    /// Imports, maps and registers the buffer `handle`; returns its cache index.
    fn import(
        &mut self,
        encoder: *mut c_void,
        handle: u64,
        size: u64,
        pitch: u32,
    ) -> Result<usize, EngineError> {
        if self.buffers.len() == MAX_BUFFERS {
            // Another exporter's ring (a restarted chain): forget one buffer that no slot
            // encodes.
            let Some(i) = self.buffers.iter().position(|b| b.maps == 0) else {
                return Err(EngineError::Busy);
            };
            let old = self.buffers.swap_remove(i);
            self.free(encoder, &old);
        }
        let desc = cuda::ExternalMemoryHandleDesc {
            kind: cuda::CU_EXTERNAL_MEMORY_HANDLE_TYPE_D3D12_RESOURCE,
            handle: cuda::Win32Handle {
                handle: handle as usize as *mut c_void,
                name: ptr::null(),
            },
            size,
            flags: cuda::CUDA_EXTERNAL_MEMORY_DEDICATED,
            reserved: [0; 16],
        };
        let mut memory: cuda::CUexternalMemory = ptr::null_mut();
        // SAFETY: `desc` names a committed D3D12 buffer of `size` bytes by an NT handle the
        // exporter keeps open while its frames live; CUDA takes its own reference.
        let r = unsafe { (self.interop.import_memory)(&mut memory, &desc) };
        if r != cuda::CUDA_SUCCESS {
            return Err(EngineError::Failed("cuImportExternalMemory", i64::from(r)));
        }
        let buffer = cuda::ExternalMemoryBufferDesc {
            offset: 0,
            size,
            flags: 0,
            reserved: [0; 16],
        };
        let mut dptr: cuda::CUdeviceptr = 0;
        // SAFETY: `memory` was imported above; the range is the whole allocation.
        let r = unsafe { (self.interop.mapped_buffer)(&mut dptr, memory, &buffer) };
        if r != cuda::CUDA_SUCCESS {
            // SAFETY: imported above and not used elsewhere.
            let _ = unsafe { (self.interop.destroy_memory)(memory) };
            return Err(EngineError::Failed(
                "cuExternalMemoryGetMappedBuffer",
                i64::from(r),
            ));
        }
        let register = self.api.nvenc.0.nvEncRegisterResource;
        let mut params = sys::NV_ENC_REGISTER_RESOURCE {
            version: sys::NV_ENC_REGISTER_RESOURCE_VER,
            resourceType: sys::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            width: self.width,
            height: self.height,
            pitch,
            resourceToRegister: dptr as usize as *mut c_void,
            bufferFormat: self.buffer_format,
            bufferUsage: sys::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        let s = match register {
            // SAFETY: open session; `dptr` is a device pointer of this session's context
            // holding `height * 3 / 2` rows of `pitch` bytes (checked against `size`).
            Some(register) => unsafe { register(encoder, &mut params) },
            None => sys::NV_ENC_ERR_INVALID_PTR,
        };
        let imported = Imported {
            handle,
            memory,
            ptr: dptr,
            registered: params.registeredResource,
            mapped: ptr::null_mut(),
            maps: 0,
        };
        if s != sys::NV_ENC_SUCCESS || imported.registered.is_null() {
            self.free(encoder, &imported);
            return Err(EngineError::Failed("nvEncRegisterResource", i64::from(s)));
        }
        // Within the reserved capacity: no allocation.
        self.buffers.push(imported);
        Ok(self.buffers.len() - 1)
    }

    /// The encoder is done with one use of buffer `handle`: unmapped after the last.
    pub(crate) fn release(&mut self, encoder: *mut c_void, handle: u64) {
        let Some(b) = self.buffers.iter_mut().find(|b| b.handle == handle) else {
            return;
        };
        b.maps = b.maps.saturating_sub(1);
        if b.maps == 0 && !b.mapped.is_null() {
            if let Some(unmap) = self.api.nvenc.0.nvEncUnmapInputResource {
                // SAFETY: mapped by `attach` on this session; no slot in flight reads it.
                let _ = unsafe { unmap(encoder, b.mapped) };
            }
            b.mapped = ptr::null_mut();
        }
    }

    /// Unmaps, unregisters and frees one imported buffer.
    fn free(&self, encoder: *mut c_void, b: &Imported) {
        let funcs = &self.api.nvenc.0;
        if !b.mapped.is_null()
            && let Some(unmap) = funcs.nvEncUnmapInputResource
        {
            // SAFETY: mapped on this session; the session no longer encodes it.
            let _ = unsafe { unmap(encoder, b.mapped) };
        }
        if !b.registered.is_null()
            && let Some(unregister) = funcs.nvEncUnregisterResource
        {
            // SAFETY: registered on this session and unmapped above.
            let _ = unsafe { unregister(encoder, b.registered) };
        }
        if b.ptr != 0 {
            // SAFETY: the pointer came from `cuExternalMemoryGetMappedBuffer`, which says
            // to free it with `cuMemFree`; NVENC no longer refers to it.
            let _ = unsafe { (self.interop.mem_free)(b.ptr) };
        }
        // SAFETY: imported by `import`, destroyed once, after its mapping was freed.
        let _ = unsafe { (self.interop.destroy_memory)(b.memory) };
    }

    /// Frees every import, the semaphores and the stream. Called once from the engine's
    /// `Drop`, after every picture was read, with the context current.
    pub(crate) fn close(&mut self, encoder: *mut c_void) {
        for b in std::mem::take(&mut self.buffers) {
            self.free(encoder, &b);
        }
        for f in self.fences.drain(..) {
            // SAFETY: imported by `wait`; no wait on it is pending once every picture
            // was read.
            let _ = unsafe { (self.interop.destroy_semaphore)(f.semaphore) };
        }
        if !self.stream.is_null() {
            // SAFETY: created by `open`; NVENC is done with it.
            let _ = unsafe { (self.interop.stream_destroy)(*self.stream) };
            *self.stream = ptr::null_mut();
        }
    }
}
