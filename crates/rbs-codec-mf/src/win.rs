//! Windows engine over Microsoft's AAC encoder MFT.
//!
//! This is the adapter's low-level module (principle P11): every `unsafe` block states
//! its invariant.
//!
//! The MFT is synchronous: `ProcessInput` takes one staged frame, then `ProcessOutput`
//! is called until it needs more input, all on the calling thread.
//!
//! COM: the adapter keeps the process's implicit multithreaded apartment alive
//! (`CoIncrementMTAUsage`, like the other Media Foundation adapters), so threads that
//! never initialised COM use the MFT in the MTA without changing their own apartment.
//! The MFT and Media Foundation samples are free-threaded.

use std::mem::ManuallyDrop;
use std::sync::OnceLock;

use rbs_codec::{AudioEncoder, AudioEncoderConfig, AudioEncoderFactory, CreateError, EncodeError};
use rbs_core::capability::CapabilitySet;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_media_format::aac::AudioSpecificConfig;
use windows::Win32::Foundation::REGDB_E_CLASSNOTREG;
use windows::Win32::Media::MediaFoundation::{
    AACMFTEncoder, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION,
    MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_USER_DATA, MF_VERSION, MFAudioFormat_AAC,
    MFAudioFormat_PCM, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Audio,
    MFSTARTUP_LITE, MFShutdown, MFStartup, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoIncrementMTAUsage};
use windows::core::{IUnknown, Interface};

use crate::session::{self, Checked, Engine, FRAME, Output, Session, delegate_audio_encoder};

/// `MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION` for AAC profile, level 2 (the encoder's
/// default; levels only bound the decoder, the stream is plain AAC-LC).
const PROFILE_LEVEL: u32 = 0x29;

/// Bytes of the `HEAACWAVEINFO` tail before the `AudioSpecificConfig` in
/// `MF_MT_USER_DATA`.
const WAVEINFO_TAIL: usize = 12;

/// Largest raw AAC-LC access unit per channel (6144 bits).
const MAX_AU_PER_CHANNEL: u32 = 768;

/// 100 ns units per second (Media Foundation time).
const HNS_PER_SEC: u64 = 10_000_000;

/// Keeps the process's implicit multithreaded apartment alive (see the module doc).
fn ensure_mta() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| {
        // SAFETY: plain FFI call. The cookie is never released: the MTA stays alive
        // for the process lifetime, which is the intent.
        let _ = unsafe { CoIncrementMTAUsage() };
    });
}

/// One `MFStartup`, balanced by `MFShutdown` on drop, held by each session.
struct Runtime;

impl Runtime {
    fn start() -> Result<Self, CreateError> {
        ensure_mta();
        // SAFETY: plain FFI call, balanced by `Drop`.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }
            .map_err(|e| CreateError::Device(format!("MFStartup: {e}")))?;
        Ok(Self)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: balances the `MFStartup` of `start`.
        let _ = unsafe { MFShutdown() };
    }
}

fn create_mft() -> windows::core::Result<IMFTransform> {
    ensure_mta();
    // SAFETY: plain COM activation of an in-process class; the MTA is alive.
    unsafe { CoCreateInstance(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER) }
}

/// Factory of the Media Foundation AAC adapter.
#[derive(Debug, Default)]
pub struct MfAacFactory;

impl MfAacFactory {
    /// Creates the factory. Does nothing until used.
    pub fn new() -> Self {
        Self
    }

    /// Creates a session with its concrete type.
    pub fn create_session(&self, config: &AudioEncoderConfig) -> Result<MfAacSession, CreateError> {
        let checked = session::check(config)?;
        let (engine, extradata) = MfEngine::new(&checked)?;
        Ok(MfAacSession(Session::new(engine, &checked, &extradata)))
    }
}

impl AdapterFactory for MfAacFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(crate::ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "Media Foundation AAC"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        // Windows editions without Media Foundation (N editions without the Media
        // Feature Pack, Server without the feature) have no encoder. The answer cannot
        // change while the process runs.
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        let available = *AVAILABLE.get_or_init(|| create_mft().is_ok());
        Ok(if available {
            session::capabilities()
        } else {
            CapabilitySet::new()
        })
    }
}

impl AudioEncoderFactory for MfAacFactory {
    fn create(&self, config: &AudioEncoderConfig) -> Result<Box<dyn AudioEncoder>, CreateError> {
        Ok(Box::new(self.create_session(config)?))
    }
}

/// One AAC encoder session over Media Foundation.
pub struct MfAacSession(Session<MfEngine>);

delegate_audio_encoder!(MfAacSession);

impl MfAacSession {
    /// Times a staged frame went into a new input sample because the MFT still held
    /// the previous one (0 with Microsoft's encoder, which copies its input).
    pub fn input_samples_replaced(&self) -> u64 {
        self.0.engine().replaced
    }
}

/// A sample with one memory buffer, owned by the session.
struct OwnedSample {
    sample: IMFSample,
    buffer: IMFMediaBuffer,
}

impl OwnedSample {
    fn new(bytes: u32) -> windows::core::Result<Self> {
        // SAFETY: plain Media Foundation constructors; the runtime is started.
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            Ok(Self { sample, buffer })
        }
    }

    /// True when someone besides this struct (the MFT) still references the sample or
    /// its buffer, so writing into it could change data the MFT has not read yet.
    fn held_elsewhere(&self) -> bool {
        // Ours, plus the sample's own reference to its buffer.
        refcount(&self.sample) != 1 || refcount(&self.buffer) != 2
    }
}

/// The current reference count of a COM object.
fn refcount<T: Interface>(object: &T) -> u32 {
    let raw = object.as_raw();
    // SAFETY: `raw` is a live COM object (we hold a reference), and every COM
    // interface starts with the `IUnknown` vtable. The AddRef is balanced by the
    // Release, whose result is the count without it.
    unsafe {
        let Some(unknown) = IUnknown::from_raw_borrowed(&raw) else {
            return 0;
        };
        let vtable = unknown.vtable();
        (vtable.AddRef)(raw);
        (vtable.Release)(raw)
    }
}

fn failed(what: &str, e: &windows::core::Error) -> EncodeError {
    EncodeError::Failed {
        reason: format!("{what}: {e}"),
        recoverable: false,
    }
}

fn audio_type(
    subtype: &windows::core::GUID,
    rate: u32,
    channels: u32,
    avg_bytes: u32,
) -> windows::core::Result<IMFMediaType> {
    // SAFETY: plain attribute calls on a new media type.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, avg_bytes)?;
        Ok(t)
    }
}

/// The encoder engine. Field order is drop order: the runtime shuts down last.
pub(crate) struct MfEngine {
    mft: IMFTransform,
    input: OwnedSample,
    input_bytes: u32,
    /// The output sample, or `None` when the MFT provides its own.
    output: Option<OwnedSample>,
    /// Media Foundation time of the next input frame.
    time: i64,
    frame_hns: i64,
    replaced: u64,
    _runtime: Runtime,
}

// SAFETY: the AAC MFT (ThreadingModel Both) and Media Foundation samples are agile,
// free-threaded in-process objects without thread affinity, so the apartment of the
// creating thread does not matter (the MTA when it never initialised COM, otherwise
// its own); the session (and with it the engine) is used by one thread at a time
// (port rule).
unsafe impl Send for MfEngine {}

impl MfEngine {
    fn new(checked: &Checked) -> Result<(Self, Vec<u8>), CreateError> {
        let runtime = Runtime::start()?;
        let mft = create_mft().map_err(|e| {
            if e.code() == REGDB_E_CLASSNOTREG {
                CreateError::Unsupported {
                    what: "this Windows has no Media Foundation AAC encoder".into(),
                }
            } else {
                CreateError::Device(format!("creating the AAC encoder: {e}"))
            }
        })?;
        let device = |what: &str| {
            let what = what.to_owned();
            move |e: windows::core::Error| CreateError::Device(format!("{what}: {e}"))
        };
        let rate = checked.format.sample_rate.hz();
        let channels = checked.channels as u32;
        let avg = u32::try_from(checked.bitrate_bps / 8).unwrap_or(u32::MAX);
        // The encoder wants its output type before its input type.
        let output_type = audio_type(&MFAudioFormat_AAC, rate, channels, avg)
            .map_err(device("AAC output type"))?;
        // SAFETY: attribute calls on our media type and type negotiation on the MFT.
        unsafe {
            output_type
                .SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0) // raw access units
                .map_err(device("AAC payload type"))?;
            output_type
                .SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, PROFILE_LEVEL)
                .map_err(device("AAC profile"))?;
            mft.SetOutputType(0, &output_type, 0)
                .map_err(device("SetOutputType"))?;
        }
        let block = channels * 2;
        let input_type = audio_type(&MFAudioFormat_PCM, rate, channels, rate * block)
            .map_err(device("PCM input type"))?;
        // SAFETY: as above.
        unsafe {
            input_type
                .SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block)
                .map_err(device("PCM block alignment"))?;
            mft.SetInputType(0, &input_type, 0)
                .map_err(device("SetInputType"))?;
        }
        let extradata =
            encoder_asc(&mft, checked).unwrap_or_else(|| checked.asc.to_bytes().to_vec());
        // SAFETY: plain MFT query.
        let info = unsafe { mft.GetOutputStreamInfo(0) }.map_err(device("GetOutputStreamInfo"))?;
        let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let output = if provides {
            None
        } else {
            let bytes = info.cbSize.max(MAX_AU_PER_CHANNEL * channels);
            Some(OwnedSample::new(bytes).map_err(device("output sample"))?)
        };
        let input_bytes = FRAME as u32 * block;
        let input = OwnedSample::new(input_bytes).map_err(device("input sample"))?;
        // SAFETY: plain MFT messages; a synchronous MFT accepts them in any state.
        unsafe {
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(device("begin streaming"))?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(device("start of stream"))?;
        }
        let frame_hns = (FRAME as u64 * HNS_PER_SEC / u64::from(rate)) as i64;
        Ok((
            Self {
                mft,
                input,
                input_bytes,
                output,
                time: 0,
                frame_hns,
                replaced: 0,
                _runtime: runtime,
            },
            extradata,
        ))
    }

    /// Calls `ProcessOutput` until the MFT needs more input, pushing each access unit.
    fn pull(&mut self, out: &mut Output) -> Result<(), EncodeError> {
        loop {
            if let Some(o) = &self.output {
                // SAFETY: our buffer, not locked.
                unsafe { o.buffer.SetCurrentLength(0) }
                    .map_err(|e| failed("SetCurrentLength", &e))?;
            }
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(self.output.as_ref().map(|o| o.sample.clone())),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            // SAFETY: one output buffer for stream 0, as the MFT has one stream.
            let result = unsafe { self.mft.ProcessOutput(0, &mut buffers, &mut status) };
            // SAFETY: each field is taken once, so the references the struct holds
            // (our clone, or the MFT's sample, and any event collection) drop once.
            let (sample, _events) = unsafe {
                (
                    ManuallyDrop::take(&mut buffers[0].pSample),
                    ManuallyDrop::take(&mut buffers[0].pEvents),
                )
            };
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(failed("ProcessOutput", &e)),
            }
            let buffer = match (&self.output, sample) {
                (Some(o), _) => o.buffer.clone(),
                (None, Some(s)) => {
                    // SAFETY: plain sample call on the MFT's output sample.
                    unsafe { s.ConvertToContiguousBuffer() }
                        .map_err(|e| failed("ConvertToContiguousBuffer", &e))?
                }
                (None, None) => {
                    return Err(EncodeError::Failed {
                        reason: "ProcessOutput returned no sample".into(),
                        recoverable: false,
                    });
                }
            };
            push_buffer(&buffer, out)?;
        }
    }
}

/// Pushes the bytes of a buffer as one access unit.
fn push_buffer(buffer: &IMFMediaBuffer, out: &mut Output) -> Result<(), EncodeError> {
    let mut data = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: the buffer is valid; on success `data` points at `len` readable bytes
    // until `Unlock`.
    unsafe { buffer.Lock(&mut data, None, Some(&mut len)) }.map_err(|e| failed("Lock", &e))?;
    let bytes = if data.is_null() {
        &[][..]
    } else {
        // SAFETY: see above; the slice is dropped before `Unlock`.
        unsafe { std::slice::from_raw_parts(data, len as usize) }
    };
    let pushed = out.push(bytes);
    // SAFETY: balances the successful `Lock`.
    let unlocked = unsafe { buffer.Unlock() };
    pushed?;
    unlocked.map_err(|e| failed("Unlock", &e))
}

/// The `AudioSpecificConfig` the encoder reports in its output type, if it is a valid
/// AAC-LC one for the session's rate and channels.
fn encoder_asc(mft: &IMFTransform, checked: &Checked) -> Option<Vec<u8>> {
    let mut blob = [0u8; 64];
    let mut len = 0u32;
    // SAFETY: plain attribute reads into a stack buffer of its stated size.
    unsafe {
        let current = mft.GetOutputCurrentType(0).ok()?;
        current
            .GetBlob(&MF_MT_USER_DATA, &mut blob, Some(&mut len))
            .ok()?;
    }
    let asc = blob.get(WAVEINFO_TAIL..len as usize)?;
    let parsed = AudioSpecificConfig::parse(asc).ok()?;
    let matches = parsed.object_type == 2
        && parsed.sample_rate() == checked.format.sample_rate.hz()
        && usize::from(parsed.channels) == checked.channels;
    matches.then(|| asc.to_vec())
}

impl Engine for MfEngine {
    fn encode(&mut self, pcm: &[i16], out: &mut Output) -> Result<(), EncodeError> {
        if self.input.held_elsewhere() {
            self.input =
                OwnedSample::new(self.input_bytes).map_err(|e| failed("input sample", &e))?;
            self.replaced += 1;
        }
        let bytes = std::mem::size_of_val(pcm);
        if bytes > self.input_bytes as usize {
            return Err(EncodeError::InvalidInput(
                "frame larger than the input sample".into(),
            ));
        }
        let mut data = std::ptr::null_mut();
        // SAFETY: our buffer, sized `input_bytes`, not referenced by the MFT (checked
        // above); `data` is writable for that many bytes until `Unlock`.
        unsafe {
            self.input
                .buffer
                .Lock(&mut data, None, None)
                .map_err(|e| failed("Lock", &e))?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr().cast::<u8>(), data, bytes);
            self.input
                .buffer
                .Unlock()
                .map_err(|e| failed("Unlock", &e))?;
            self.input
                .buffer
                .SetCurrentLength(bytes as u32)
                .map_err(|e| failed("SetCurrentLength", &e))?;
            self.input
                .sample
                .SetSampleTime(self.time)
                .map_err(|e| failed("SetSampleTime", &e))?;
            self.input
                .sample
                .SetSampleDuration(self.frame_hns)
                .map_err(|e| failed("SetSampleDuration", &e))?;
        }
        self.time += self.frame_hns;
        // SAFETY: a sample for stream 0; the MFT references it until consumed.
        match unsafe { self.mft.ProcessInput(0, &self.input.sample, 0) } {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                // Output is pending: take it, then the input fits.
                self.pull(out)?;
                // SAFETY: as above.
                unsafe { self.mft.ProcessInput(0, &self.input.sample, 0) }
                    .map_err(|e| failed("ProcessInput", &e))?;
            }
            Err(e) => return Err(failed("ProcessInput", &e)),
        }
        self.pull(out)
    }

    fn finish(&mut self, out: &mut Output) -> Result<(), EncodeError> {
        // SAFETY: plain MFT message.
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .map_err(|e| failed("drain", &e))?;
        self.pull(out)?;
        // SAFETY: plain MFT messages; nothing is buffered after the drain.
        unsafe {
            self.mft
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
                .map_err(|e| failed("flush", &e))?;
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| failed("start of stream", &e))?;
        }
        self.time = 0;
        Ok(())
    }
}

impl Drop for MfEngine {
    fn drop(&mut self) {
        // SAFETY: plain MFT message; failures do not matter while tearing down.
        let _ = unsafe { self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0) };
    }
}
