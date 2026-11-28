//! H.264 clips for the decoder tests, made at test time with Microsoft's H.264
//! encoder MFT (`CLSID_MSH264EncoderMFT`), so no video is committed: Baseline profile
//! (no B-frames), Annex B, one access unit per packet, SPS and PPS in band in the
//! first keyframe, as the live case of the guest source.
//!
//! Test support only: allocates freely. Every `unsafe` block states its invariant.

use std::mem::ManuallyDrop;

use rbs_codec::h264::{self, NAL_IDR, NAL_SPS};
use rbs_core::media::{BitstreamFormat, Bytes, CodecId, EncodedPacket};
use rbs_core::time::Timestamp;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264EncoderMFT, IMFMediaType, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MFCreateMediaType, MFMediaType_Video, MFSampleExtension_CleanPoint, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, eAVEncH264VProfile_Base,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::GUID;

use crate::decode::mft;
use crate::win::{Runtime, ensure_mta};

/// Media Foundation time of one encoded frame (30 fps).
const FRAME_HNS: i64 = 333_333;

fn err(what: &str) -> impl Fn(windows::core::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// `frames` packets of a `width` x `height` clip with stamps `first + i * step_ns`
/// (choose a step that is not a multiple of 100 ns to check bit-exact stamps). `Err`
/// when this machine has no H.264 encoder MFT.
pub fn clip(
    width: u32,
    height: u32,
    frames: usize,
    first: Timestamp,
    step_ns: u64,
) -> Result<Vec<EncodedPacket>, String> {
    let _runtime = Runtime::start().map_err(|e| e.to_string())?;
    ensure_mta();
    // SAFETY: plain COM activation of an in-process class; the MTA is alive.
    let mft: IMFTransform =
        unsafe { CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER) }
            .map_err(err("H.264 encoder MFT"))?;
    let output = video_type(&MFVideoFormat_H264, width, height).map_err(err("type"))?;
    // SAFETY: plain attribute calls on a live media type, then on the live MFT.
    unsafe {
        output
            .SetUINT32(&MF_MT_AVG_BITRATE, (width * height * 4).max(200_000))
            .map_err(err("bitrate"))?;
        output
            .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)
            .map_err(err("profile"))?;
        mft.SetOutputType(0, &output, 0)
            .map_err(err("output type"))?;
        let input = video_type(&MFVideoFormat_NV12, width, height).map_err(err("type"))?;
        mft.SetInputType(0, &input, 0).map_err(err("input type"))?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(err("start"))?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(err("start"))?;
    }
    let mut units: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut picture = vec![0u8; (width * height * 3 / 2) as usize];
    for i in 0..frames {
        paint(&mut picture, width as usize, height as usize, i);
        let sample = mft::input_sample(&picture, i as i64 * FRAME_HNS, false)
            .map_err(err("input sample"))?;
        // SAFETY: a live sample and MFT.
        unsafe { sample.SetSampleDuration(FRAME_HNS) }.map_err(err("duration"))?;
        loop {
            // SAFETY: a live MFT and sample.
            match unsafe { mft.ProcessInput(0, &sample, 0) } {
                Ok(()) => break,
                Err(e) if e.code() == MF_E_NOTACCEPTING => pull(&mft, width, height, &mut units)?,
                Err(e) => return Err(format!("encode: {e}")),
            }
        }
        pull(&mft, width, height, &mut units)?;
    }
    // SAFETY: plain messages to the live MFT.
    unsafe {
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
            .map_err(err("drain"))?;
        mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
            .map_err(err("drain"))?;
    }
    pull(&mft, width, height, &mut units)?;
    if let Some((first_unit, _)) = units.first_mut()
        && !h264::nal_units(first_unit).any(|n| h264::nal_type(n) == Some(NAL_SPS))
    {
        let mut header = sequence_header(&mft)?;
        header.extend_from_slice(first_unit);
        *first_unit = header;
    }
    units
        .into_iter()
        .enumerate()
        .map(|(i, (data, clean))| {
            let key = clean || h264::nal_units(&data).any(|n| h264::nal_type(n) == Some(NAL_IDR));
            let pts = Timestamp::from_nanos(first.as_nanos() + i as u64 * step_ns);
            EncodedPacket::new(
                CodecId::H264,
                BitstreamFormat::AnnexB,
                pts,
                pts,
                key,
                Bytes::from(data),
            )
            .map_err(|e| format!("packet: {e}"))
        })
        .collect()
}

/// Two clips one after the other, each starting with its own keyframe and SPS: the
/// second at another size (an in-band SPS change). Stamps continue.
pub fn two_size_clip(
    first_size: (u32, u32),
    second_size: (u32, u32),
    frames_each: usize,
    step_ns: u64,
) -> Result<Vec<EncodedPacket>, String> {
    let start = Timestamp::from_nanos(1_000_000_007);
    let mut a = clip(first_size.0, first_size.1, frames_each, start, step_ns)?;
    let next = Timestamp::from_nanos(start.as_nanos() + frames_each as u64 * step_ns);
    let b = clip(second_size.0, second_size.1, frames_each, next, step_ns)?;
    a.extend(b);
    Ok(a)
}

/// LUID of the first DXGI adapter (the one the studio renders on by default), in the
/// `GpuContext::adapter_luid` encoding.
pub fn default_adapter_luid() -> Option<u64> {
    // SAFETY: plain DXGI factory calls with valid out values.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter = factory.EnumAdapters1(0).ok()?;
        let desc = adapter.GetDesc1().ok()?;
        let luid = desc.AdapterLuid;
        Some((u64::from(luid.HighPart as u32) << 32) | u64::from(luid.LowPart))
    }
}

fn video_type(subtype: &GUID, width: u32, height: u32) -> windows::core::Result<IMFMediaType> {
    // SAFETY: plain attribute calls on a new media type.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(width) << 32) | u64::from(height),
        )?;
        t.SetUINT64(&MF_MT_FRAME_RATE, (30 << 32) | 1)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        Ok(t)
    }
}

/// A moving gradient, so the encoder sees motion.
fn paint(nv12: &mut [u8], width: usize, height: usize, index: usize) {
    let (luma, chroma) = nv12.split_at_mut(width * height);
    for (i, px) in luma.iter_mut().enumerate() {
        let (x, y) = (i % width, i / width);
        *px = ((x + y + index * 4) & 0xff) as u8;
    }
    for (i, px) in chroma.iter_mut().enumerate() {
        *px = (96 + (i / width + index) % 64) as u8;
    }
}

fn sequence_header(mft: &IMFTransform) -> Result<Vec<u8>, String> {
    // SAFETY: plain reads on the live MFT's output type; the blob is copied into a
    // buffer of the size the type reports.
    unsafe {
        let t = mft.GetOutputCurrentType(0).map_err(err("output type"))?;
        let size = t
            .GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER)
            .map_err(err("sequence header"))?;
        let mut blob = vec![0u8; size as usize];
        t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)
            .map_err(err("sequence header"))?;
        Ok(blob)
    }
}

/// Takes every encoded unit the MFT has ready.
fn pull(
    mft: &IMFTransform,
    width: u32,
    height: u32,
    units: &mut Vec<(Vec<u8>, bool)>,
) -> Result<(), String> {
    loop {
        // SAFETY: a plain query on the live MFT.
        let info = unsafe { mft.GetOutputStreamInfo(0) }.map_err(err("stream info"))?;
        let provides = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0;
        let ours = if provides {
            None
        } else {
            Some(mft::empty_sample(info.cbSize.max(width * height * 2)).map_err(err("sample"))?)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(ours),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        // SAFETY: a live MFT and one output buffer that lives across the call.
        let result = unsafe { mft.ProcessOutput(0, &mut buffers, &mut status) };
        let [buffer] = buffers;
        let sample = ManuallyDrop::into_inner(buffer.pSample);
        drop(ManuallyDrop::into_inner(buffer.pEvents));
        match result {
            Ok(()) => {
                if let Some(sample) = sample {
                    units.push(read_unit(&sample)?);
                }
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // SAFETY: a type the MFT itself offers.
                unsafe {
                    let t = mft
                        .GetOutputAvailableType(0, 0)
                        .map_err(err("output type"))?;
                    mft.SetOutputType(0, &t, 0).map_err(err("output type"))?;
                }
            }
            Err(e) => return Err(format!("encode output: {e}")),
        }
    }
}

fn read_unit(sample: &IMFSample) -> Result<(Vec<u8>, bool), String> {
    // SAFETY: plain calls on a live sample; `Lock` gives `len` readable bytes until
    // `Unlock`.
    unsafe {
        let clean = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(err("output buffer"))?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer
            .Lock(&mut ptr, None, Some(&mut len))
            .map_err(err("output buffer"))?;
        let data = if ptr.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(ptr, len as usize).to_vec()
        };
        let _ = buffer.Unlock();
        Ok((data, clean))
    }
}
