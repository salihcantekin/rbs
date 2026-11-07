//! The source's one NV12 stream: answers each `RequestSample` with the picture the
//! [`FramePump`] chooses, copied into a sample from a pooled allocator.

use std::sync::{Mutex, MutexGuard};

use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER, ERROR_SET_NOT_FOUND, S_OK, WIN32_ERROR};
use windows::Win32::Media::KernelStreaming::{
    IKsControl, IKsControl_Impl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows_core::{GUID, IUnknown, Interface, Ref, Result, implement};

use super::com::{Alive, guard};
use super::section::Section;
use crate::announced::Announced;
use crate::pump::{self, FramePump};

/// Samples the allocator keeps in its pool.
const POOL: u32 = 8;

/// What a running stream holds. Field order is drop order: the pump (and its view of
/// the ring) goes before the section is unmapped.
struct Running {
    pump: FramePump,
    _section: Option<Section>,
}

struct State {
    queue: Option<IMFMediaEventQueue>,
    source: Option<IMFMediaSource>,
    stream_state: MF_STREAM_STATE,
    allocator: Option<IMFVideoSampleAllocatorEx>,
    running: Option<Running>,
}

/// The stream object.
#[implement(IMFMediaStream2, IKsControl)]
pub(crate) struct MediaStream {
    descriptor: IMFStreamDescriptor,
    attributes: IMFAttributes,
    media_type: IMFMediaType,
    format: Announced,
    sid: String,
    state: Mutex<State>,
    _alive: Alive,
}

impl MediaStream {
    /// The stream for `format`, writing user `sid`'s section, with its descriptor and
    /// device-stream attributes. Not real-time.
    pub(crate) fn new(format: Announced, sid: String) -> Result<Self> {
        let media_type = media_type(&format)?;
        // SAFETY: plain object creation; the array holds one valid media type.
        let descriptor = unsafe { MFCreateStreamDescriptor(0, &[Some(media_type.clone())])? };
        // SAFETY: the descriptor's own handler; setting the type it was created with.
        unsafe {
            descriptor
                .GetMediaTypeHandler()?
                .SetCurrentMediaType(&media_type)?
        };
        let mut attributes = None;
        // SAFETY: creates an empty attribute store.
        unsafe { MFCreateAttributes(&mut attributes, 4)? };
        let attributes = attributes.ok_or(windows_core::Error::from(E_POINTER))?;
        for store in [&attributes, &descriptor.cast::<IMFAttributes>()?] {
            // SAFETY: setting plain values on our own attribute stores.
            unsafe {
                store.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
                store.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
                store.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
                store.SetUINT32(
                    &MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES,
                    MFFrameSourceTypes_Color.0 as u32,
                )?;
            }
        }
        // SAFETY: plain object creation.
        let queue = unsafe { MFCreateEventQueue()? };
        Ok(Self {
            descriptor,
            attributes,
            media_type,
            format,
            sid,
            state: Mutex::new(State {
                queue: Some(queue),
                source: None,
                stream_state: MF_STREAM_STATE_STOPPED,
                allocator: None,
                running: None,
            }),
            _alive: Alive::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock is contained by `guard`; the state stays usable.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn queue(&self) -> Result<IMFMediaEventQueue> {
        self.lock()
            .queue
            .clone()
            .ok_or_else(|| MF_E_SHUTDOWN.into())
    }

    /// The descriptor of this stream.
    pub(crate) fn descriptor(&self) -> &IMFStreamDescriptor {
        &self.descriptor
    }

    /// The stream's device-stream attributes (`IMFMediaSourceEx::GetStreamAttributes`).
    pub(crate) fn attributes(&self) -> &IMFAttributes {
        &self.attributes
    }

    /// Links the stream to its source (cleared on shutdown, breaking the cycle).
    pub(crate) fn set_source(&self, source: IMFMediaSource) {
        self.lock().source = Some(source);
    }

    /// The allocator Frame Server provides (`IMFSampleAllocatorControl`).
    pub(crate) fn set_allocator(&self, allocator: Option<IMFVideoSampleAllocatorEx>) {
        self.lock().allocator = allocator;
    }

    /// Starts the stream: creates the section and the pump, prepares the allocator and
    /// queues `MEStreamStarted`. Not real-time: everything a sample needs is made here.
    pub(crate) fn start(&self) -> Result<()> {
        let mut st = self.lock();
        let queue = st
            .queue
            .clone()
            .ok_or(windows_core::Error::from(MF_E_SHUTDOWN))?;
        if st.running.is_none() {
            // No section (squatted name, no rights): the still picture only.
            let ring = pump::layout(&self.format)
                .ok()
                .and_then(|layout| Section::create(&self.sid, &layout).ok());
            let (section, region) = match ring {
                Some((s, r)) => (Some(s), Some(r)),
                None => (None, None),
            };
            let allocator = match st.allocator.clone() {
                Some(a) => a,
                None => own_allocator()?,
            };
            // SAFETY: a pool of `POOL` samples of our media type.
            unsafe { allocator.InitializeSampleAllocator(POOL, &self.media_type)? };
            st.allocator = Some(allocator);
            st.running = Some(Running {
                pump: FramePump::new(&self.format, region),
                _section: section,
            });
        }
        st.stream_state = MF_STREAM_STATE_RUNNING;
        drop(st);
        // SAFETY: our own queue; an event with no value.
        unsafe {
            queue.QueueEventParamVar(
                MEStreamStarted.0 as u32,
                &GUID::zeroed(),
                S_OK,
                std::ptr::null(),
            )?
        };
        Ok(())
    }

    /// Stops the stream: releases the pump, the section and the pool.
    pub(crate) fn stop(&self) -> Result<()> {
        let mut st = self.lock();
        let queue = st
            .queue
            .clone()
            .ok_or(windows_core::Error::from(MF_E_SHUTDOWN))?;
        st.running = None;
        if let Some(a) = &st.allocator {
            // SAFETY: our pool; samples still out are released by their holders.
            let _ = unsafe { a.UninitializeSampleAllocator() };
        }
        st.stream_state = MF_STREAM_STATE_STOPPED;
        drop(st);
        // SAFETY: as in `start`.
        unsafe {
            queue.QueueEventParamVar(
                MEStreamStopped.0 as u32,
                &GUID::zeroed(),
                S_OK,
                std::ptr::null(),
            )?
        };
        Ok(())
    }

    /// Shuts the stream down for good.
    pub(crate) fn shutdown(&self) {
        let mut st = self.lock();
        st.running = None;
        st.source = None;
        if let Some(a) = st.allocator.take() {
            // SAFETY: our pool.
            let _ = unsafe { a.UninitializeSampleAllocator() };
        }
        if let Some(q) = st.queue.take() {
            // SAFETY: our own queue.
            let _ = unsafe { q.Shutdown() };
        }
    }

    /// One sample: the pump's picture in a pooled sample, timestamped now.
    fn sample(&self, token: Option<&IUnknown>) -> Result<()> {
        let mut st = self.lock();
        let queue = st
            .queue
            .clone()
            .ok_or(windows_core::Error::from(MF_E_SHUTDOWN))?;
        if st.stream_state != MF_STREAM_STATE_RUNNING {
            return Err(MF_E_INVALIDREQUEST.into());
        }
        let allocator = st
            .allocator
            .clone()
            .ok_or(windows_core::Error::from(MF_E_NOT_INITIALIZED))?;
        let running = st
            .running
            .as_mut()
            .ok_or(windows_core::Error::from(MF_E_NOT_INITIALIZED))?;
        // SAFETY: no preconditions.
        let now_ms = unsafe { GetTickCount64() };
        let (picture, _shown) = running.pump.next(now_ms);
        // SAFETY: a sample from our initialised pool.
        let sample = unsafe { allocator.AllocateSample()? };
        // SAFETY: the pooled sample has one buffer.
        let buffer = unsafe { sample.GetBufferByIndex(0)? };
        copy_nv12(&buffer, picture, &self.format)?;
        drop(st);
        // SAFETY: setting times and the token on our sample, then handing it to the
        // queue, which keeps a reference.
        unsafe {
            sample.SetSampleTime(MFGetSystemTime())?;
            sample.SetSampleDuration(self.format.frame_duration_hns())?;
            if let Some(t) = token {
                sample.SetUnknown(&MFSampleExtension_Token, t)?;
            }
            queue.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, &sample)?;
        }
        Ok(())
    }
}

/// Copies tightly packed NV12 `picture` into `buffer`, honouring a 2D buffer's pitch.
fn copy_nv12(buffer: &IMFMediaBuffer, picture: &[u8], format: &Announced) -> Result<()> {
    let (w, h) = (format.width() as usize, format.height() as usize);
    let rows = h + h / 2;
    if picture.len() < w * rows {
        return Err(E_INVALIDARG.into());
    }
    if let Ok(two_d) = buffer.cast::<IMF2DBuffer2>() {
        let mut scan0: *mut u8 = std::ptr::null_mut();
        let mut pitch = 0i32;
        let mut start: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        // SAFETY: locks our pooled buffer for writing; unlocked below.
        unsafe {
            two_d.Lock2DSize(
                MF2DBuffer_LockFlags_Write,
                &mut scan0,
                &mut pitch,
                &mut start,
                &mut len,
            )?
        };
        let pitch_u = pitch.unsigned_abs() as usize;
        let fits = pitch > 0 && pitch_u >= w && !scan0.is_null() && {
            let end = scan0 as usize + pitch_u * (rows - 1) + w;
            end <= start as usize + len as usize
        };
        if fits {
            for (row, src) in picture.chunks_exact(w).take(rows).enumerate() {
                // SAFETY: row `row` starts `row * pitch` bytes after `scan0` and holds
                // `w` bytes inside the locked buffer (checked by `fits`); `src` is our
                // own memory.
                unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), scan0.add(row * pitch_u), w) };
            }
        }
        // SAFETY: locked above.
        unsafe { two_d.Unlock2D()? };
        return if fits {
            Ok(())
        } else {
            Err(E_INVALIDARG.into())
        };
    }
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut max = 0u32;
    // SAFETY: locks our pooled buffer; unlocked below.
    unsafe { buffer.Lock(&mut ptr, Some(&mut max), None)? };
    let n = w * rows;
    let fits = !ptr.is_null() && max as usize >= n;
    if fits {
        // SAFETY: `ptr` holds `max >= n` bytes; `picture` has `n`.
        unsafe { std::ptr::copy_nonoverlapping(picture.as_ptr(), ptr, n) };
    }
    // SAFETY: locked above.
    unsafe {
        buffer.Unlock()?;
        if fits {
            buffer.SetCurrentLength(n as u32)?;
        }
    }
    if fits {
        Ok(())
    } else {
        Err(E_INVALIDARG.into())
    }
}

/// A pooled allocator of our own, when Frame Server provides none.
fn own_allocator() -> Result<IMFVideoSampleAllocatorEx> {
    let mut out: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: asks for the interface whose IID we pass; `out` receives a reference we
    // take ownership of below.
    unsafe {
        MFCreateVideoSampleAllocatorEx(&IMFVideoSampleAllocatorEx::IID, &mut out)?;
        Ok(IMFVideoSampleAllocatorEx::from_raw(out))
    }
}

/// The NV12 media type for `format`.
pub(crate) fn media_type(format: &Announced) -> Result<IMFMediaType> {
    let (w, h) = (format.width(), format.height());
    // SAFETY: plain object creation and setting values on it.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(w) << 32) | u64::from(h))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(format.rate()) << 32) | 1)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        t.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        t.SetUINT32(&MF_MT_DEFAULT_STRIDE, w)?;
        t.SetUINT32(&MF_MT_SAMPLE_SIZE, format.frame_len() as u32)?;
        t.SetUINT32(
            &MF_MT_AVG_BITRATE,
            (format.frame_len() as u64 * 8 * u64::from(format.rate())).min(u64::from(u32::MAX))
                as u32,
        )?;
        Ok(t)
    }
}

/// `HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND)`: the source exposes no KS property.
pub(crate) fn no_ks_set() -> windows_core::Error {
    WIN32_ERROR(ERROR_SET_NOT_FOUND.0).to_hresult().into()
}

impl IMFMediaEventGenerator_Impl for MediaStream_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        // The queue is taken out of the lock: a blocking `GetEvent` must not hold it.
        guard(|| {
            let q = self.queue()?;
            // SAFETY: forwards to our queue.
            unsafe { q.GetEvent(flags.0) }
        })
    }

    fn BeginGetEvent(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> Result<()> {
        guard(|| {
            let q = self.queue()?;
            // SAFETY: forwards to our queue with the caller's references.
            unsafe { q.BeginGetEvent(callback.as_ref(), state.as_ref()) }
        })
    }

    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
        guard(|| {
            let q = self.queue()?;
            // SAFETY: as above.
            unsafe { q.EndGetEvent(result.as_ref()) }
        })
    }

    fn QueueEvent(
        &self,
        met: u32,
        ext: *const GUID,
        status: windows_core::HRESULT,
        value: *const windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
    ) -> Result<()> {
        guard(|| {
            let q = self.queue()?;
            // SAFETY: forwards the caller's pointers unchanged.
            unsafe { q.QueueEventParamVar(met, ext, status, value) }
        })
    }
}

impl IMFMediaStream_Impl for MediaStream_Impl {
    fn GetMediaSource(&self) -> Result<IMFMediaSource> {
        guard(|| {
            self.lock()
                .source
                .clone()
                .ok_or_else(|| MF_E_SHUTDOWN.into())
        })
    }

    fn GetStreamDescriptor(&self) -> Result<IMFStreamDescriptor> {
        guard(|| Ok(self.descriptor.clone()))
    }

    fn RequestSample(&self, token: Ref<IUnknown>) -> Result<()> {
        guard(|| self.sample(token.as_ref()))
    }
}

impl IMFMediaStream2_Impl for MediaStream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> Result<()> {
        guard(|| match value {
            MF_STREAM_STATE_RUNNING => self.start(),
            MF_STREAM_STATE_STOPPED => self.stop(),
            MF_STREAM_STATE_PAUSED => Err(MF_E_INVALID_STATE_TRANSITION.into()),
            _ => Err(E_INVALIDARG.into()),
        })
    }

    fn GetStreamState(&self) -> Result<MF_STREAM_STATE> {
        guard(|| Ok(self.lock().stream_state))
    }
}

impl IKsControl_Impl for MediaStream_Impl {
    fn KsProperty(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut core::ffi::c_void,
        _: u32,
        _: *mut u32,
    ) -> Result<()> {
        guard(|| Err(no_ks_set()))
    }

    fn KsMethod(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut core::ffi::c_void,
        _: u32,
        _: *mut u32,
    ) -> Result<()> {
        guard(|| Err(no_ks_set()))
    }

    fn KsEvent(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut core::ffi::c_void,
        _: u32,
        _: *mut u32,
    ) -> Result<()> {
        guard(|| Err(no_ks_set()))
    }
}
