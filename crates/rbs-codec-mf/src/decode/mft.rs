//! Thin wrappers over Microsoft's H.264 decoder MFT (`CLSID_MSH264DecoderMFT`).
//!
//! Low-level module (principle P11): every `unsafe` block states its invariant. Every
//! call here is a plain COM call on objects this crate created and holds a reference
//! to; the MTA is kept alive by [`crate::win::ensure_mta`].

use rbs_codec::VideoDecodeError;
use windows::Win32::Foundation::E_NOTIMPL;
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, CODECAPI_AVDecVideoAcceleration_H264, CODECAPI_AVLowLatencyMode,
    IMFDXGIDeviceManager, IMFMediaType, IMFSample, IMFTransform, MF_LOW_LATENCY,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SA_D3D11_AWARE, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
    MFSampleExtension_CleanPoint, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive,
};
use windows::Win32::Media::MediaFoundation::{
    MF_E_HW_MFT_FAILED_START_STREAMING, MF_E_INVALIDMEDIATYPE, MF_E_INVALIDTYPE,
    MF_E_UNSUPPORTED_D3D_TYPE, MF_E_UNSUPPORTED_FORMAT, MF_E_UNSUPPORTED_RATE,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::HRESULT;
use windows::core::{GUID, Interface};

/// A decode failure the session survives (keyframe gating follows).
/// A removed, reset or hung D3D device is `DeviceLost` (the session must be recreated).
pub(crate) fn failed(what: &str, e: &windows::core::Error) -> VideoDecodeError {
    let code = e.code();
    if code == DXGI_ERROR_DEVICE_REMOVED
        || code == DXGI_ERROR_DEVICE_RESET
        || code == DXGI_ERROR_DEVICE_HUNG
    {
        return VideoDecodeError::DeviceLost;
    }
    VideoDecodeError::Failed {
        reason: format!("{what}: {e}"),
        recoverable: true,
    }
}

/// HRESULTs with which a decoder refuses the stream itself (profile, level, size or
/// format it cannot decode), as opposed to a bad packet.
const REFUSALS: [HRESULT; 7] = [
    MF_E_UNSUPPORTED_FORMAT,
    MF_E_INVALIDMEDIATYPE,
    MF_E_INVALIDTYPE,
    MF_E_UNSUPPORTED_D3D_TYPE,
    MF_E_UNSUPPORTED_RATE,
    MF_E_HW_MFT_FAILED_START_STREAMING,
    E_NOTIMPL,
];

/// True when `code` says the decoder refuses the stream (see [`REFUSALS`]).
pub(crate) fn is_refusal(code: HRESULT) -> bool {
    REFUSALS.contains(&code)
}

/// [`failed`] for a decode call (`ProcessInput`, `ProcessOutput`): a refusal of the
/// stream is `Unsupported`, so the engine falls back to the next adapter.
pub(crate) fn decode_failed(e: &windows::core::Error) -> VideoDecodeError {
    if is_refusal(e.code()) {
        return VideoDecodeError::Unsupported(format!(
            "the H.264 decoder MFT refused the stream: {e}"
        ));
    }
    failed("decode", e)
}

/// A new instance of the decoder MFT.
pub(crate) fn create() -> windows::core::Result<IMFTransform> {
    crate::win::ensure_mta();
    // SAFETY: plain COM activation of an in-process class; the MTA is alive.
    unsafe { CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER) }
}

/// Asks for low-latency decode, hands the MFT `manager` (DXVA on that device) when
/// given and the MFT is D3D11 aware, sets the H.264 input type and an NV12 output when
/// the MFT offers one already, and starts streaming. Returns whether the MFT took the
/// device manager (hardware decode).
pub(crate) fn configure(
    mft: &IMFTransform,
    manager: Option<&IMFDXGIDeviceManager>,
) -> windows::core::Result<bool> {
    // SAFETY: plain attribute calls on the live MFT's attribute store.
    let attributes = unsafe { mft.GetAttributes()? };
    // SAFETY: as above. Failures leave the defaults, which still decode.
    unsafe {
        let _ = attributes.SetUINT32(&CODECAPI_AVLowLatencyMode, 1);
        let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
    }
    let mut hardware = false;
    if let Some(manager) = manager {
        // SAFETY: as above.
        let aware = unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) != 0;
        if aware {
            // SAFETY: as above; the manager pointer is live for the call, and the MFT
            // takes its own reference.
            unsafe {
                let _ = attributes.SetUINT32(&CODECAPI_AVDecVideoAcceleration_H264, 1);
                hardware = mft
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                    .is_ok();
            }
        }
    }
    let input = video_type(&MFVideoFormat_H264)?;
    // SAFETY: a live MFT and media type.
    unsafe { mft.SetInputType(0, &input, 0)? };
    // Before the first SPS some versions offer no output type; the stream change on
    // the first keyframe sets it then.
    let _ = set_nv12_output(mft);
    // SAFETY: plain messages to the live MFT.
    unsafe {
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
    }
    Ok(hardware)
}

fn video_type(subtype: &GUID) -> windows::core::Result<IMFMediaType> {
    // SAFETY: plain attribute calls on a new media type.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        Ok(t)
    }
}

/// Picks the first NV12 output type the MFT offers.
pub(crate) fn set_nv12_output(mft: &IMFTransform) -> windows::core::Result<()> {
    for index in 0..64 {
        // SAFETY: plain enumeration on a live MFT; ends with MF_E_NO_MORE_TYPES.
        let offered = unsafe { mft.GetOutputAvailableType(0, index)? };
        // SAFETY: a plain attribute read on a live media type.
        if unsafe { offered.GetGUID(&MF_MT_SUBTYPE)? } == MFVideoFormat_NV12 {
            // SAFETY: a type the MFT itself offered.
            return unsafe { mft.SetOutputType(0, &offered, 0) };
        }
    }
    Err(windows::core::Error::from_hresult(
        windows::Win32::Media::MediaFoundation::MF_E_INVALIDMEDIATYPE,
    ))
}

/// The current NV12 output: coded size and row pitch of its system-memory form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: u32,
}

/// Output layout and stream flags after an output type was set: the layout, whether
/// the MFT provides its output samples, and the output buffer size otherwise.
pub(crate) fn output_layout(mft: &IMFTransform) -> windows::core::Result<(Layout, bool, u32)> {
    // SAFETY: plain reads on a live MFT and its current output type.
    unsafe {
        let t = mft.GetOutputCurrentType(0)?;
        let size = t.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let width = (size >> 32) as u32;
        let height = size as u32;
        let stride = t
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .ok()
            .filter(|&s| (s as i32) > 0)
            .unwrap_or(width);
        let info = mft.GetOutputStreamInfo(0)?;
        let provides = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0;
        Ok((
            Layout {
                width,
                height,
                stride,
            },
            provides,
            info.cbSize,
        ))
    }
}

/// A sample with one memory buffer of `bytes`.
pub(crate) fn empty_sample(bytes: u32) -> windows::core::Result<IMFSample> {
    // SAFETY: plain Media Foundation constructors; the runtime is started.
    unsafe {
        let buffer = MFCreateMemoryBuffer(bytes.max(1))?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}

/// Makes our reused output sample empty again before the MFT writes into it.
pub(crate) fn rewind(sample: &IMFSample) {
    // SAFETY: plain calls on a live sample and its buffer.
    unsafe {
        let _ = sample.DeleteAllItems();
        if let Ok(buffer) = sample.GetBufferByIndex(0) {
            let _ = buffer.SetCurrentLength(0);
        }
    }
}

/// A sample holding a copy of `data`, at Media Foundation time `time`.
pub(crate) fn input_sample(
    data: &[u8],
    time: i64,
    keyframe: bool,
) -> windows::core::Result<IMFSample> {
    let len = u32::try_from(data.len()).map_err(|_| {
        windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG)
    })?;
    // SAFETY: the buffer is created with `len` bytes; `Lock` gives a pointer to at
    // least that many writable bytes until `Unlock`, and `data` does not overlap it.
    unsafe {
        let buffer = MFCreateMemoryBuffer(len.max(1))?;
        let mut ptr = std::ptr::null_mut();
        let mut max = 0u32;
        buffer.Lock(&mut ptr, Some(&mut max), None)?;
        if ptr.is_null() || max < len {
            let _ = buffer.Unlock();
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_POINTER,
            ));
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(len)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        sample.SetUINT32(&MFSampleExtension_CleanPoint, u32::from(keyframe))?;
        Ok(sample)
    }
}

/// Drops everything the MFT holds.
pub(crate) fn flush(mft: &IMFTransform) -> windows::core::Result<()> {
    // SAFETY: a plain message to the live MFT.
    unsafe { mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::E_FAIL;
    use windows::Win32::Media::MediaFoundation::MF_E_TRANSFORM_NEED_MORE_INPUT;

    #[test]
    fn stream_refusals_are_unsupported_and_other_failures_are_not() {
        let err = |code: HRESULT| windows::core::Error::from_hresult(code);
        for code in REFUSALS {
            assert!(matches!(
                decode_failed(&err(code)),
                VideoDecodeError::Unsupported(_)
            ));
        }
        assert!(matches!(
            decode_failed(&err(E_FAIL)),
            VideoDecodeError::Failed {
                recoverable: true,
                ..
            }
        ));
        assert!(!is_refusal(MF_E_TRANSFORM_NEED_MORE_INPUT));
        assert_eq!(
            decode_failed(&err(DXGI_ERROR_DEVICE_REMOVED)),
            VideoDecodeError::DeviceLost
        );
    }
}
