//! The media source: one live NV12 stream, started and stopped by Frame Server.

use std::sync::{Mutex, MutexGuard};

use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER, S_OK};
use windows::Win32::Media::KernelStreaming::{IKsControl, IKsControl_Impl, KSIDENTIFIER};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{BOOL, ComObject, GUID, IUnknown, Interface, Ref, Result, implement};

use super::com::{Alive, guard};
use super::config::SourceConfig;
use super::stream::{MediaStream, no_ks_set};

struct State {
    queue: Option<IMFMediaEventQueue>,
    started_once: bool,
}

/// The source object.
#[implement(IMFMediaSourceEx, IMFGetService, IKsControl, IMFSampleAllocatorControl)]
pub(crate) struct MediaSource {
    attributes: IMFAttributes,
    presentation: IMFPresentationDescriptor,
    stream: ComObject<MediaStream>,
    state: Mutex<State>,
    _alive: Alive,
}

/// Creates the source for `config` with `attributes` copied in (the activate object's).
pub(crate) fn create(
    config: &SourceConfig,
    from: Option<&IMFAttributes>,
) -> Result<IMFMediaSourceEx> {
    let stream = ComObject::new(MediaStream::new(config.format, config.sid.clone())?);
    let mut attributes = None;
    // SAFETY: creates an empty attribute store.
    unsafe { MFCreateAttributes(&mut attributes, 4)? };
    let attributes = attributes.ok_or(windows_core::Error::from(E_POINTER))?;
    if let Some(from) = from {
        // SAFETY: copies the activate object's items into our own store.
        unsafe { from.CopyAllItems(&attributes)? };
    }
    // SAFETY: one stream descriptor, selected.
    let presentation = unsafe {
        let pd = MFCreatePresentationDescriptor(Some(&[Some(stream.descriptor().clone())]))?;
        pd.SelectStream(0)?;
        pd
    };
    // SAFETY: plain object creation.
    let queue = unsafe { MFCreateEventQueue()? };
    let source: IMFMediaSourceEx = ComObject::new(MediaSource {
        attributes,
        presentation,
        stream: stream.clone(),
        state: Mutex::new(State {
            queue: Some(queue),
            started_once: false,
        }),
        _alive: Alive::new(),
    })
    .into_interface();
    stream.set_source(source.cast()?);
    Ok(source)
}

impl MediaSource {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn queue(&self) -> Result<IMFMediaEventQueue> {
        self.lock()
            .queue
            .clone()
            .ok_or_else(|| MF_E_SHUTDOWN.into())
    }

    fn start(
        &self,
        pd: Option<&IMFPresentationDescriptor>,
        time_format: *const GUID,
        position: *const PROPVARIANT,
    ) -> Result<()> {
        let pd = pd.ok_or(windows_core::Error::from(E_INVALIDARG))?;
        // SAFETY: a null time format means the default; otherwise read the caller's GUID.
        if !time_format.is_null() && unsafe { *time_format } != GUID::zeroed() {
            return Err(MF_E_UNSUPPORTED_TIME_FORMAT.into());
        }
        let mut selected = BOOL::default();
        let mut sd = None;
        // SAFETY: index 0 of the caller's descriptor (ours has exactly one stream).
        unsafe { pd.GetStreamDescriptorByIndex(0, &mut selected, &mut sd)? };
        let queue = self.queue()?;
        if selected.as_bool() {
            // Started first, so a failed start leaves no stream event behind.
            self.stream.start()?;
            let mut st = self.lock();
            let first = !st.started_once;
            st.started_once = true;
            drop(st);
            let unk: IUnknown = self.stream.to_interface();
            // SAFETY: our own queue; the stream's reference is kept by the event.
            unsafe {
                let met = if first { MENewStream } else { MEUpdatedStream };
                queue.QueueEventParamUnk(met.0 as u32, &GUID::zeroed(), S_OK, &unk)?;
            }
        } else {
            // A deselected stream sends nothing.
            self.stream.stop_quietly();
        }
        // SAFETY: the start position is the caller's PROPVARIANT, passed on unchanged.
        unsafe {
            queue.QueueEventParamVar(MESourceStarted.0 as u32, &GUID::zeroed(), S_OK, position)?
        };
        Ok(())
    }
}

impl IMFMediaEventGenerator_Impl for MediaSource_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        guard(|| {
            let q = self.queue()?;
            // SAFETY: forwards to our queue (outside our lock: it may block).
            unsafe { q.GetEvent(flags.0) }
        })
    }

    fn BeginGetEvent(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> Result<()> {
        guard(|| {
            let q = self.queue()?;
            // SAFETY: forwards the caller's references.
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
        value: *const PROPVARIANT,
    ) -> Result<()> {
        guard(|| {
            let q = self.queue()?;
            // SAFETY: forwards the caller's pointers unchanged.
            unsafe { q.QueueEventParamVar(met, ext, status, value) }
        })
    }
}

impl IMFMediaSource_Impl for MediaSource_Impl {
    fn GetCharacteristics(&self) -> Result<u32> {
        guard(|| {
            self.queue()?;
            Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
        })
    }

    fn CreatePresentationDescriptor(&self) -> Result<IMFPresentationDescriptor> {
        guard(|| {
            self.queue()?;
            // SAFETY: clones our own descriptor.
            unsafe { self.presentation.Clone() }
        })
    }

    fn Start(
        &self,
        pd: Ref<IMFPresentationDescriptor>,
        time_format: *const GUID,
        position: *const PROPVARIANT,
    ) -> Result<()> {
        guard(|| self.start(pd.as_ref(), time_format, position))
    }

    fn Stop(&self) -> Result<()> {
        guard(|| {
            let q = self.queue()?;
            self.stream.stop()?;
            // SAFETY: our own queue; no value.
            unsafe {
                q.QueueEventParamVar(
                    MESourceStopped.0 as u32,
                    &GUID::zeroed(),
                    S_OK,
                    std::ptr::null(),
                )
            }
        })
    }

    fn Pause(&self) -> Result<()> {
        guard(|| Err(MF_E_INVALID_STATE_TRANSITION.into()))
    }

    fn Shutdown(&self) -> Result<()> {
        guard(|| {
            let q = self.lock().queue.take();
            self.stream.shutdown();
            if let Some(q) = q {
                // SAFETY: our own queue, shut down once.
                unsafe { q.Shutdown()? };
            }
            Ok(())
        })
    }
}

impl IMFMediaSourceEx_Impl for MediaSource_Impl {
    fn GetSourceAttributes(&self) -> Result<IMFAttributes> {
        guard(|| {
            self.queue()?;
            Ok(self.attributes.clone())
        })
    }

    fn GetStreamAttributes(&self, id: u32) -> Result<IMFAttributes> {
        guard(|| {
            self.queue()?;
            if id != 0 {
                return Err(MF_E_INVALIDSTREAMNUMBER.into());
            }
            Ok(self.stream.attributes().clone())
        })
    }

    fn SetD3DManager(&self, _manager: Ref<IUnknown>) -> Result<()> {
        // Frames are copied on the CPU (ADR-0066 decision 9); a D3D manager is not used.
        guard(|| self.queue().map(drop))
    }
}

impl IMFGetService_Impl for MediaSource_Impl {
    fn GetService(
        &self,
        _: *const GUID,
        _: *const GUID,
        _: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        guard(|| Err(MF_E_UNSUPPORTED_SERVICE.into()))
    }
}

impl IKsControl_Impl for MediaSource_Impl {
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

impl IMFSampleAllocatorControl_Impl for MediaSource_Impl {
    fn SetDefaultAllocator(&self, id: u32, allocator: Ref<IUnknown>) -> Result<()> {
        guard(|| {
            if id != 0 {
                return Err(MF_E_INVALIDSTREAMNUMBER.into());
            }
            let a = allocator
                .as_ref()
                .and_then(|a| a.cast::<IMFVideoSampleAllocatorEx>().ok());
            self.stream.set_allocator(a)
        })
    }

    fn GetAllocatorUsage(
        &self,
        id: u32,
        input: *mut u32,
        usage: *mut MFSampleAllocatorUsage,
    ) -> Result<()> {
        guard(|| {
            if input.is_null() || usage.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: both pointers checked non-null; the caller provides the storage.
            unsafe {
                *input = id;
                *usage = MFSampleAllocatorUsage_UsesProvidedAllocator;
            }
            Ok(())
        })
    }
}
