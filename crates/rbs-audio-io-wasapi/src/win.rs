//! The Windows backend. Overview in the crate documentation.
//!
//! Threads: `enumerate`, `open_input`, `start` and `stop` run on a control thread; the
//! packets are read on the stream's own capture thread; `read` runs on the audio
//! thread and only touches the lock-free ring and an atomic status.
//!
//! COM: the adapter keeps the process's implicit multithreaded apartment alive
//! (`CoIncrementMTAUsage`, like the Media Foundation and WGC adapters), so control
//! threads that never initialised COM use the MMDevice objects in the MTA. The capture
//! thread joins the MTA itself.

use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Once};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{
    CloseHandle, E_ACCESSDENIED, E_FAIL, E_INVALIDARG, E_OUTOFMEMORY, ERROR_NOT_FOUND,
    GetLastError, HANDLE, PROPERTYKEY, STILL_ACTIVE, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_E_DEVICE_IN_USE, AUDCLNT_E_DEVICE_INVALIDATED,
    AUDCLNT_E_RESOURCES_INVALIDATED, AUDCLNT_E_UNSUPPORTED_FORMAT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, AudioSessionStateExpired, DEVICE_STATE_ACTIVE, EDataFlow,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, IMMEndpoint,
    MMDeviceEnumerator, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture, eConsole,
    eRender,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PropVariantClear, PropVariantToStringAlloc,
};
use windows::Win32::System::Com::{
    BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoIncrementMTAUsage, CoInitializeEx,
    CoTaskMemAlloc, CoTaskMemFree, CoUninitialize, STGM_READ,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    GetCurrentProcessId, GetExitCodeProcess, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, SetEvent,
    WaitForMultipleObjects,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, HSTRING, Interface, PWSTR, w};
use windows_core::{Ref, implement};

use rbs_audio_io::ring::{self, I24, RingConsumer};
use rbs_audio_io::{
    AudioBackend, AudioEndpoint, AudioInputStream, AudioIoError, AudioRead, EndpointKind,
    ErrorDomain, InputRequest, InputStats, caps,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{AudioFormat, ChannelLayout, MAX_CHANNELS};
use rbs_core::registry::{AdapterFactory, AdapterId, HARDWARE, ProbeError};
use rbs_core::time::Clock;
use rbs_core::timing::SampleRate;

use crate::packets::{Packet, Packets, Samples};
use crate::wave::{SampleKind, TAG_EXTENSIBLE, TAG_FLOAT, WaveFields, WaveFormat};
use crate::{ADAPTER_ID, Owned, PROCESS_LOOPBACK_BUILD, ids};

/// Longest wait in `start` for the capture thread to open the stream (the port
/// allows 500 ms). Opening takes a few milliseconds.
const START_TIMEOUT: Duration = Duration::from_millis(450);

/// Longest wait for process loopback activation, inside `START_TIMEOUT`.
const ACTIVATE_TIMEOUT: Duration = Duration::from_millis(400);

/// The audio engine's buffer for a capture stream, in 100 ns units (100 ms): room for
/// ten periods if the capture thread is held up. Packets stay one period long.
const ENGINE_BUFFER: i64 = 1_000_000;

/// Period when the client does not report one (process loopback).
const DEFAULT_PERIOD: Duration = Duration::from_millis(10);

/// Silence is filled once no packet arrived for this many periods.
const FILL_PERIODS: u32 = 3;

/// Rates an application stream may ask process loopback for.
const APP_RATES: [SampleRate; 3] = [
    SampleRate::HZ_44100,
    SampleRate::HZ_48000,
    SampleRate::HZ_96000,
];

/// `PKEY_Device_FriendlyName`.
const FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

static MTA: Once = Once::new();

/// Keeps the implicit MTA alive for the process (once).
fn mta() {
    MTA.call_once(|| {
        // SAFETY: no preconditions; the cookie is deliberately never released, so the
        // MTA lives as long as the process.
        let _ = unsafe { CoIncrementMTAUsage() };
    });
}

fn hresult(hr: HRESULT) -> AudioIoError {
    match hr {
        E_ACCESSDENIED => AudioIoError::PermissionDenied,
        AUDCLNT_E_DEVICE_INVALIDATED | AUDCLNT_E_RESOURCES_INVALIDATED => {
            AudioIoError::EndpointLost
        }
        AUDCLNT_E_DEVICE_IN_USE => AudioIoError::DeviceBusy,
        AUDCLNT_E_UNSUPPORTED_FORMAT => AudioIoError::FormatUnsupported,
        hr if hr == ERROR_NOT_FOUND.to_hresult() => AudioIoError::EndpointNotFound,
        hr => AudioIoError::Backend {
            domain: ErrorDomain::HResult,
            code: i64::from(hr.0),
            recoverable: true,
        },
    }
}

fn error(e: windows::core::Error) -> AudioIoError {
    hresult(e.code())
}

/// The Windows build number (`RtlGetVersion` is not subject to manifest lies).
fn windows_build() -> u32 {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a valid OSVERSIONINFOW with its size set, as required.
    let status = unsafe { RtlGetVersion(&mut info) };
    if status.is_ok() {
        info.dwBuildNumber
    } else {
        0
    }
}

/// QPC now, in nanoseconds: the clock of WASAPI's `u64QPCPosition`.
fn qpc_nanos() -> u64 {
    let mut counter = 0_i64;
    let mut frequency = 0_i64;
    // SAFETY: both are valid out pointers; the calls cannot fail on Windows XP and later.
    unsafe {
        let _ = QueryPerformanceCounter(&mut counter);
        let _ = QueryPerformanceFrequency(&mut frequency);
    }
    let frequency = u128::try_from(frequency.max(1)).unwrap_or(1);
    let counter = u128::try_from(counter.max(0)).unwrap_or(0);
    u64::try_from(counter * 1_000_000_000 / frequency).unwrap_or(u64::MAX)
}

fn enumerator() -> Result<IMMDeviceEnumerator, AudioIoError> {
    mta();
    // SAFETY: plain COM activation of the system MMDevice enumerator.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(error)
}

/// Takes ownership of a COM task-memory string.
fn take_string(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: `p` is a NUL-terminated string allocated by COM, freed right after.
    let s = unsafe { p.to_string() }.unwrap_or_default();
    // SAFETY: `p` came from CoTaskMemAlloc (the API contract of its source).
    unsafe { CoTaskMemFree(Some(p.0 as _)) };
    s
}

fn device_id(device: &IMMDevice) -> Result<String, AudioIoError> {
    // SAFETY: plain COM call on a live interface.
    let id = unsafe { device.GetId() }.map_err(error)?;
    Ok(take_string(id))
}

fn friendly_name(device: &IMMDevice) -> Option<String> {
    // SAFETY: plain COM calls on live interfaces; the PROPVARIANT is cleared after use.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut value = store.GetValue(&FRIENDLY_NAME).ok()?;
        let name = PropVariantToStringAlloc(&value).ok().map(take_string);
        let _ = PropVariantClear(&mut value);
        name.filter(|n| !n.is_empty())
    }
}

/// Copies the fields of the wave format at `p`.
///
/// # Safety
///
/// `p` points to a valid `WAVEFORMATEX` whose `cbSize` bytes of extension follow it.
unsafe fn wave_fields(p: *const WAVEFORMATEX) -> WaveFields {
    // SAFETY: the caller guarantees `p` is valid; the struct is packed, so it is read
    // unaligned.
    let base = unsafe { p.read_unaligned() };
    let extension =
        std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>();
    let extensible = (base.wFormatTag == TAG_EXTENSIBLE && usize::from(base.cbSize) >= extension)
        .then(|| {
            // SAFETY: the tag and `cbSize` say a WAVEFORMATEXTENSIBLE is there.
            let ext = unsafe { p.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
            // SAFETY: every member of the `Samples` union is a u16.
            let valid = unsafe { ext.Samples.wValidBitsPerSample };
            let subformat = ext.SubFormat;
            (valid, ext.dwChannelMask, subformat.to_u128())
        });
    WaveFields {
        tag: base.wFormatTag,
        channels: base.nChannels,
        rate: base.nSamplesPerSec,
        block_align: base.nBlockAlign,
        bits: base.wBitsPerSample,
        extensible,
    }
}

/// The shared-mode mix format of `client`.
fn mix_format(client: &IAudioClient) -> Result<WaveFormat, AudioIoError> {
    // SAFETY: GetMixFormat returns a CoTaskMemAlloc'ed WAVEFORMATEX(TENSIBLE), read and
    // then freed.
    unsafe {
        let p = client.GetMixFormat().map_err(error)?;
        let fields = wave_fields(p);
        CoTaskMemFree(Some(p as _));
        WaveFormat::parse(fields).ok_or(AudioIoError::FormatUnsupported)
    }
}

fn activate_client(device: &IMMDevice) -> Result<IAudioClient, AudioIoError> {
    // SAFETY: plain COM activation on a live device.
    unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }.map_err(error)
}

fn flow_of(kind: EndpointKind) -> EDataFlow {
    if kind == EndpointKind::Input {
        eCapture
    } else {
        eRender
    }
}

/// An active device of `kind` by its id.
fn device(
    enumerator: &IMMDeviceEnumerator,
    kind: EndpointKind,
    id: &str,
) -> Result<IMMDevice, AudioIoError> {
    // SAFETY: plain COM calls on live interfaces.
    unsafe {
        let device =
            enumerator
                .GetDevice(&HSTRING::from(id))
                .map_err(|e| match hresult(e.code()) {
                    // A malformed id is as unknown as a missing one.
                    AudioIoError::Backend { .. } if e.code() == E_INVALIDARG => {
                        AudioIoError::EndpointNotFound
                    }
                    other => other,
                })?;
        let active = device.GetState().is_ok_and(|s| s == DEVICE_STATE_ACTIVE);
        let flow = device
            .cast::<IMMEndpoint>()
            .and_then(|e| e.GetDataFlow())
            .ok();
        if !active || flow != Some(flow_of(kind)) {
            return Err(AudioIoError::EndpointNotFound);
        }
        Ok(device)
    }
}

/// Every active device of `kind` (Input or OutputLoopback) with its format.
fn device_endpoints(kind: EndpointKind) -> Result<Vec<(AudioEndpoint, IMMDevice)>, AudioIoError> {
    let enumerator = enumerator()?;
    let flow = flow_of(kind);
    // SAFETY: plain COM calls on live interfaces.
    unsafe {
        let default = enumerator
            .GetDefaultAudioEndpoint(flow, eConsole)
            .ok()
            .and_then(|d| device_id(&d).ok());
        let collection = enumerator
            .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
            .map_err(error)?;
        let count = collection.GetCount().map_err(error)?;
        let mut found = Vec::new();
        for i in 0..count {
            let Ok(device) = collection.Item(i) else {
                continue;
            };
            let Ok(id) = device_id(&device) else {
                continue;
            };
            // Endpoints whose format cannot be delivered are not listed.
            let Ok(wave) = activate_client(&device).and_then(|c| mix_format(&c)) else {
                continue;
            };
            let name = friendly_name(&device).unwrap_or_else(|| id.clone());
            let is_default = default.as_deref() == Some(id.as_str());
            let endpoint = AudioEndpoint::new(ids::endpoint(kind, &id), kind, name, wave.format)
                .with_default(is_default);
            found.push((endpoint, device));
        }
        Ok(found)
    }
}

/// The rate of the default render endpoint's mix format, for application streams.
fn default_render_rate(enumerator: &IMMDeviceEnumerator) -> SampleRate {
    // SAFETY: plain COM call on a live interface.
    unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
        .ok()
        .and_then(|d| activate_client(&d).ok())
        .and_then(|c| mix_format(&c).ok())
        .map_or(SampleRate::HZ_48000, |w| w.format.sample_rate)
}

fn application_endpoint(pid: u32, name: String, rate: SampleRate) -> AudioEndpoint {
    let format = AudioFormat::new(rate, ChannelLayout::Stereo).unwrap_or(AudioFormat::STEREO_48K);
    AudioEndpoint::new(
        ids::endpoint(EndpointKind::Application, &pid.to_string()),
        EndpointKind::Application,
        name,
        format,
    )
    .with_sample_rates(&APP_RATES)
}

/// An open process handle, closed on drop.
struct Handle(HANDLE);

// SAFETY: kernel object handles may be used and closed from any thread.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value and closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn open_process(
    pid: u32,
    access: windows::Win32::System::Threading::PROCESS_ACCESS_RIGHTS,
) -> Option<Handle> {
    // SAFETY: OpenProcess has no preconditions; the handle is owned by `Handle`.
    unsafe { OpenProcess(access, false, pid) }.ok().map(Handle)
}

fn process_alive(pid: u32) -> bool {
    let Some(process) = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION) else {
        return false;
    };
    let mut code = 0_u32;
    // SAFETY: valid process handle and out pointer.
    let ok = unsafe { GetExitCodeProcess(process.0, &mut code) }.is_ok();
    ok && code == STILL_ACTIVE.0 as u32
}

/// The executable file name of `pid`.
fn process_name(pid: u32) -> Option<String> {
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut buffer = [0_u16; 1_024];
    let mut len = buffer.len() as u32;
    // SAFETY: valid handle; `buffer` holds `len` UTF-16 units.
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    let path = String::from_utf16_lossy(&buffer[..len as usize]);
    path.rsplit(['\\', '/']).next().map(str::to_owned)
}

/// Processes with a live audio session on some render endpoint.
fn applications() -> Result<Vec<AudioEndpoint>, AudioIoError> {
    let enumerator = enumerator()?;
    let rate = default_render_rate(&enumerator);
    // SAFETY: GetCurrentProcessId has no preconditions.
    let own = unsafe { GetCurrentProcessId() };
    let mut pids: Vec<u32> = Vec::new();
    // SAFETY: plain COM calls on live interfaces.
    unsafe {
        let collection = enumerator
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .map_err(error)?;
        for i in 0..collection.GetCount().map_err(error)? {
            let Ok(device) = collection.Item(i) else {
                continue;
            };
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };
            let Ok(sessions) = manager.GetSessionEnumerator() else {
                continue;
            };
            for s in 0..sessions.GetCount().unwrap_or(0) {
                let Ok(session) = sessions.GetSession(s) else {
                    continue;
                };
                if session
                    .GetState()
                    .is_ok_and(|state| state == AudioSessionStateExpired)
                {
                    continue;
                }
                let Ok(session) = session.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                // S_OK means it is the system sounds session.
                if session.IsSystemSoundsSession() == HRESULT(0) {
                    continue;
                }
                let Ok(pid) = session.GetProcessId() else {
                    continue;
                };
                if pid != 0 && pid != own && !pids.contains(&pid) {
                    pids.push(pid);
                }
            }
        }
    }
    Ok(pids
        .into_iter()
        .filter(|pid| process_alive(*pid))
        .map(|pid| {
            let name = process_name(pid).unwrap_or_else(|| format!("Process {pid}"));
            application_endpoint(pid, name, rate)
        })
        .collect())
}

// ---------------------------------------------------------------------------------
// Backend

/// Backend of the WASAPI audio adapter. Creating it does nothing.
#[derive(Debug, Default)]
pub struct WasapiAudioBackend {
    _private: (),
}

impl WasapiAudioBackend {
    /// Creates the backend. Does nothing until used.
    pub fn new() -> Self {
        Self::default()
    }

    fn kinds() -> Vec<EndpointKind> {
        let mut kinds = vec![EndpointKind::Input, EndpointKind::OutputLoopback];
        if windows_build() >= PROCESS_LOOPBACK_BUILD {
            kinds.push(EndpointKind::Application);
        }
        kinds
    }
}

impl AdapterFactory for WasapiAudioBackend {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "WASAPI audio"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        // Cheap: WASAPI is part of every supported Windows; only the build decides
        // whether process loopback exists. No COM calls.
        let kinds = || {
            CapabilityValue::Set(
                Self::kinds()
                    .iter()
                    .map(|k| k.as_str().to_owned())
                    .collect(),
            )
        };
        Ok(CapabilitySet::new()
            .with(caps::INPUT_KINDS, kinds())
            .with(caps::INPUT_ENUMERATE, kinds())
            .with(
                caps::INPUT_CHANNELS_MAX,
                CapabilityValue::Int(MAX_CHANNELS as i64),
            )
            .with(caps::INPUT_PERMISSION, CapabilityValue::Flag(false))
            .with(HARDWARE, CapabilityValue::Flag(true)))
    }
}

impl AudioBackend for WasapiAudioBackend {
    fn enumerate(&self, kind: EndpointKind) -> Result<Vec<AudioEndpoint>, AudioIoError> {
        if !Self::kinds().contains(&kind) {
            return Err(AudioIoError::Unsupported(
                "process loopback needs Windows 10 version 2004",
            ));
        }
        match kind {
            EndpointKind::Application => applications(),
            _ => Ok(device_endpoints(kind)?
                .into_iter()
                .map(|(endpoint, _)| endpoint)
                .collect()),
        }
    }

    fn open_input(
        &self,
        request: &InputRequest,
    ) -> Result<Box<dyn AudioInputStream>, AudioIoError> {
        let (kind, native) = ids::parse(&request.endpoint).ok_or(AudioIoError::EndpointNotFound)?;
        if !Self::kinds().contains(&kind) {
            return Err(AudioIoError::Unsupported(
                "process loopback needs Windows 10 version 2004",
            ));
        }
        let enumerator = enumerator()?;
        let (target, wave) = match kind {
            EndpointKind::Application => {
                let pid: u32 = native.parse().map_err(|_| AudioIoError::EndpointNotFound)?;
                if !process_alive(pid) {
                    return Err(AudioIoError::EndpointNotFound);
                }
                let rate = default_render_rate(&enumerator);
                let endpoint = application_endpoint(pid, String::new(), rate);
                let format = request.format_for(&endpoint);
                let wave = WaveFormat {
                    kind: SampleKind::F32,
                    source_channels: 2,
                    format,
                };
                (Target::Process(pid), wave)
            }
            _ => {
                let device = device(&enumerator, kind, native)?;
                let wave = mix_format(&activate_client(&device)?)?;
                let endpoint = AudioEndpoint::new(request.endpoint.clone(), kind, "", wave.format);
                // Device endpoints list only their mix rate, so this is it.
                let wave = WaveFormat {
                    format: request.format_for(&endpoint),
                    ..wave
                };
                (
                    Target::Device {
                        id: native.to_owned(),
                        loopback: kind == EndpointKind::OutputLoopback,
                    },
                    wave,
                )
            }
        };
        Ok(Box::new(WasapiStream {
            target,
            wave,
            clock: Arc::clone(&request.clock),
            buffer: request.clamped_buffer(),
            last_end: Arc::new(AtomicU64::new(0)),
            active: None,
            totals: InputStats::default(),
        }))
    }
}

// ---------------------------------------------------------------------------------
// Stream

/// What a stream captures.
#[derive(Debug, Clone)]
enum Target {
    Device { id: String, loopback: bool },
    Process(u32),
}

/// What the capture thread reports to `read`.
#[derive(Default)]
struct Status {
    /// One of the `STATE_*` values.
    state: AtomicU8,
    /// The HRESULT of `STATE_FAILED`.
    code: AtomicI64,
}

const STATE_RUNNING: u8 = 0;
const STATE_LOST: u8 = 1;
const STATE_FORMAT_CHANGED: u8 = 2;
const STATE_FAILED: u8 = 3;
const STATE_BUSY: u8 = 4;
const STATE_DENIED: u8 = 5;

impl Status {
    fn set(&self, e: AudioIoError) {
        let state = match e {
            AudioIoError::EndpointLost => STATE_LOST,
            AudioIoError::FormatChanged => STATE_FORMAT_CHANGED,
            AudioIoError::DeviceBusy => STATE_BUSY,
            AudioIoError::PermissionDenied => STATE_DENIED,
            AudioIoError::Backend { code, .. } => {
                self.code.store(code, Ordering::Relaxed);
                STATE_FAILED
            }
            _ => {
                self.code
                    .store(i64::from(E_INVALIDARG.0), Ordering::Relaxed);
                STATE_FAILED
            }
        };
        self.state.store(state, Ordering::Release);
    }

    /// **Real-time**: an atomic load.
    fn error(&self) -> Option<AudioIoError> {
        match self.state.load(Ordering::Acquire) {
            STATE_RUNNING => None,
            STATE_LOST => Some(AudioIoError::EndpointLost),
            STATE_FORMAT_CHANGED => Some(AudioIoError::FormatChanged),
            STATE_BUSY => Some(AudioIoError::DeviceBusy),
            STATE_DENIED => Some(AudioIoError::PermissionDenied),
            _ => Some(AudioIoError::Backend {
                domain: ErrorDomain::HResult,
                code: self.code.load(Ordering::Relaxed),
                recoverable: true,
            }),
        }
    }
}

/// An event object, closed on drop.
struct Event(HANDLE);

// SAFETY: kernel event handles may be signalled, waited on and closed from any thread.
unsafe impl Send for Event {}
// SAFETY: as above; SetEvent and waits are thread-safe.
unsafe impl Sync for Event {}

impl Event {
    fn new() -> Result<Self, AudioIoError> {
        // SAFETY: an unnamed auto-reset event with default security.
        unsafe { CreateEventW(None, false, false, None) }
            .map(Event)
            .map_err(error)
    }

    fn set(&self) {
        // SAFETY: the handle is a live event owned by this value.
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: owned handle, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct Active {
    stop: Arc<Event>,
    thread: Option<JoinHandle<()>>,
    consumer: RingConsumer,
    status: Arc<Status>,
}

struct WasapiStream {
    target: Target,
    wave: WaveFormat,
    clock: Arc<dyn Clock>,
    buffer: Duration,
    /// Master time at which the last packet ended, in nanoseconds, so a restarted
    /// stream starts later.
    last_end: Arc<AtomicU64>,
    active: Option<Active>,
    /// Counters of rings closed by `stop`.
    totals: InputStats,
}

/// What the capture thread needs.
struct Setup {
    target: Target,
    wave: WaveFormat,
    clock: Arc<dyn Clock>,
    buffer: Duration,
    last_end: Arc<AtomicU64>,
    status: Arc<Status>,
    stop: Arc<Event>,
}

type Started = SyncSender<Result<RingConsumer, AudioIoError>>;

impl WasapiStream {
    fn activate(&mut self) -> Result<Active, AudioIoError> {
        let stop = Arc::new(Event::new()?);
        let status = Arc::new(Status::default());
        let setup = Setup {
            target: self.target.clone(),
            wave: self.wave,
            clock: Arc::clone(&self.clock),
            buffer: self.buffer,
            last_end: Arc::clone(&self.last_end),
            status: Arc::clone(&status),
            stop: Arc::clone(&stop),
        };
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("rbs-wasapi-audio".into())
            .spawn(move || capture_thread(setup, started_tx))
            .map_err(|e| AudioIoError::Backend {
                domain: ErrorDomain::HResult,
                code: i64::from(e.raw_os_error().unwrap_or(0)),
                recoverable: true,
            })?;
        let started = match started_rx.recv_timeout(START_TIMEOUT) {
            Ok(started) => started,
            Err(_) => {
                // The OS is slow to open the stream: give up within the port's budget.
                // The thread sees the stop event or the closed channel and ends on
                // its own; it is not joined here, which could block past the budget.
                // Until it does, it may briefly still hold the device, its COM objects
                // and its `owned_resources` count; a quick retry can overlap with it.
                stop.set();
                return Err(AudioIoError::Backend {
                    domain: ErrorDomain::HResult,
                    // HRESULT_FROM_WIN32(ERROR_TIMEOUT)
                    code: i64::from(0x8007_05B4_u32 as i32),
                    recoverable: true,
                });
            }
        };
        match started {
            Ok(consumer) => Ok(Active {
                stop,
                thread: Some(thread),
                consumer,
                status,
            }),
            Err(e) => {
                // The thread reported and is returning.
                let _ = thread.join();
                Err(e)
            }
        }
    }
}

impl AudioInputStream for WasapiStream {
    fn format(&self) -> AudioFormat {
        self.wave.format
    }

    fn start(&mut self) -> Result<(), AudioIoError> {
        if self.active.is_some() {
            return Ok(());
        }
        let active = self.activate()?;
        self.active = Some(active);
        Ok(())
    }

    fn stop(&mut self) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        active.stop.set();
        if let Some(thread) = active.thread.take() {
            let _ = thread.join();
        }
        self.totals = self.totals.plus(active.consumer.close());
    }

    fn read(&mut self, out: &mut [&mut [f32]]) -> Result<AudioRead, AudioIoError> {
        let channels = self.wave.channels();
        if out.len() != channels || out.iter().any(|c| c.len() != out[0].len()) {
            return Err(AudioIoError::BufferMismatch);
        }
        let Some(active) = self.active.as_mut() else {
            return Ok(AudioRead::EMPTY);
        };
        if let Some(e) = active.status.error() {
            return Err(e);
        }
        active.consumer.read(out)
    }

    fn available(&self) -> usize {
        self.active.as_ref().map_or(0, |a| a.consumer.available())
    }

    fn stats(&self) -> InputStats {
        match &self.active {
            Some(active) => self.totals.plus(active.consumer.stats()),
            None => self.totals,
        }
    }
}

impl Drop for WasapiStream {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------------
// Process loopback activation

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Completion {
    done: SyncSender<()>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let _ = self.done.try_send(());
        Ok(())
    }
}

/// An audio client capturing the process tree of `pid`.
fn activate_process(pid: u32) -> Result<IAudioClient, AudioIoError> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // The activation takes ownership of the blob and frees it later: a boxed block the
    // adapter freed after activation was freed twice and corrupted the heap, and a
    // block it kept was reused by the heap after the stream stopped. So the blob comes
    // from the COM task allocator and is never freed here. If the OS kept it on some
    // path, the cost is one 12-byte block per activation.
    let size = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>();
    // SAFETY: plain allocation; checked for null below.
    let blob = unsafe { CoTaskMemAlloc(size) }.cast::<AUDIOCLIENT_ACTIVATION_PARAMS>();
    if blob.is_null() {
        return Err(AudioIoError::Backend {
            domain: ErrorDomain::HResult,
            code: i64::from(E_OUTOFMEMORY.0),
            recoverable: true,
        });
    }
    // SAFETY: `blob` is a fresh allocation of `size` bytes; CoTaskMemAlloc aligns for
    // any fundamental type.
    unsafe { blob.write(params) };
    let mut variant = PROPVARIANT::default();
    // SAFETY: writes the VT_BLOB members of a zeroed PROPVARIANT. The variant is never
    // passed to PropVariantClear: the blob belongs to the activation (see above).
    unsafe {
        let inner = &mut *variant.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: size as u32,
            pBlobData: blob.cast(),
        };
    }
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler = Completion { done: done_tx }.into();
    // SAFETY: valid device path, IID, activation parameters (owned by the activation,
    // see above) and handler.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&variant),
            &handler,
        )
    }
    .map_err(error)?;
    if done_rx.recv_timeout(ACTIVATE_TIMEOUT).is_err() {
        return Err(AudioIoError::Backend {
            domain: ErrorDomain::HResult,
            code: i64::from(0x8007_05B4_u32 as i32),
            recoverable: true,
        });
    }
    let mut result = HRESULT(0);
    let mut unknown = None;
    // SAFETY: the operation completed; both out pointers are valid.
    unsafe { operation.GetActivateResult(&mut result, &mut unknown) }.map_err(error)?;
    result.ok().map_err(error)?;
    unknown
        .ok_or(AudioIoError::EndpointNotFound)?
        .cast::<IAudioClient>()
        .map_err(error)
}

// ---------------------------------------------------------------------------------
// Capture thread

/// A WASAPI capture stream opened on the capture thread.
struct Opened {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
    /// The captured process, signalled when it exits.
    process: Option<Handle>,
    period: Duration,
    /// Frames the engine buffer holds; no packet is larger.
    buffer_frames: usize,
    /// The device's id, to tell a lost device from a changed format.
    device: Option<String>,
}

fn open_stream(target: &Target, wave: &WaveFormat) -> Result<Opened, AudioIoError> {
    let (client, flags, format, device, process) = match target {
        Target::Device { id, loopback } => {
            let kind = if *loopback {
                EndpointKind::OutputLoopback
            } else {
                EndpointKind::Input
            };
            let device = device(&enumerator()?, kind, id)?;
            let client = activate_client(&device)?;
            let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
            if *loopback {
                flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
            }
            (client, flags, None, Some(id.clone()), None)
        }
        Target::Process(pid) => {
            let process =
                open_process(*pid, PROCESS_SYNCHRONIZE).ok_or(AudioIoError::EndpointNotFound)?;
            let client = activate_process(*pid)?;
            let rate = wave.format.sample_rate.hz();
            let format = WAVEFORMATEX {
                wFormatTag: TAG_FLOAT,
                nChannels: 2,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: rate * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            (client, flags, Some(format), None, Some(process))
        }
    };
    // SAFETY: plain COM calls on a live client; the format pointers are valid for the
    // duration of Initialize, and the mix format is freed after it.
    unsafe {
        match format {
            Some(format) => client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    ENGINE_BUFFER,
                    0,
                    &format,
                    None,
                )
                .map_err(error)?,
            None => {
                // The mix format may have changed since `open_input`. The packets are
                // read as `wave`'s sample type and channel count, so the format the
                // stream is initialised with must be exactly that one.
                let mix = client.GetMixFormat().map_err(error)?;
                let same = WaveFormat::parse(wave_fields(mix)).as_ref() == Some(wave);
                let result = if same {
                    client
                        .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, ENGINE_BUFFER, 0, mix, None)
                        .map_err(error)
                } else {
                    Err(AudioIoError::FormatChanged)
                };
                CoTaskMemFree(Some(mix as _));
                result?;
            }
        }
        let mut period = 0_i64;
        let period = match client.GetDevicePeriod(Some(&mut period), None) {
            Ok(()) if period > 0 => Duration::from_nanos(period.unsigned_abs() * 100),
            _ => DEFAULT_PERIOD,
        };
        // Process loopback clients may not implement every IAudioClient method.
        let buffer_frames = client.GetBufferSize().map_or_else(
            |_| ring::frames_for(wave.format.sample_rate, Duration::from_millis(100)),
            |frames| frames as usize,
        );
        let capture = client.GetService::<IAudioCaptureClient>().map_err(error)?;
        let event = Event::new()?;
        client.SetEventHandle(event.0).map_err(error)?;
        Ok(Opened {
            client,
            capture,
            event,
            process,
            period,
            buffer_frames,
            device,
        })
    }
}

/// The samples of a packet as the device's sample type, or `None` if the buffer is
/// not aligned for it (counted as lost).
///
/// # Safety
///
/// `data` points to `frames` frames of `wave`'s source format, valid until the
/// packet is released.
unsafe fn samples<'a>(data: *const u8, frames: usize, wave: &WaveFormat) -> Option<Samples<'a>> {
    let n = frames * wave.source_channels;
    fn cast<'a, T>(data: *const u8, n: usize) -> Option<&'a [T]> {
        let p = data.cast::<T>();
        if !p.is_aligned() {
            return None;
        }
        // SAFETY: the caller of `samples` guarantees `n` samples of `T` at `data`,
        // and the pointer is aligned for `T`.
        Some(unsafe { std::slice::from_raw_parts(p, n) })
    }
    Some(match wave.kind {
        SampleKind::F32 => Samples::F32(cast(data, n)?),
        SampleKind::F64 => Samples::F64(cast(data, n)?),
        SampleKind::U8 => Samples::U8(cast(data, n)?),
        SampleKind::I16 => Samples::I16(cast(data, n)?),
        SampleKind::I24 => Samples::I24(cast::<I24>(data, n)?),
        SampleKind::I32 => Samples::I32(cast(data, n)?),
    })
}

/// Reads every pending packet. **Real-time**: no allocation, locks, I/O or logging.
fn drain(
    opened: &Opened,
    wave: &WaveFormat,
    packets: &mut Packets,
    clock: &dyn Clock,
) -> windows::core::Result<()> {
    loop {
        // SAFETY: plain COM calls on a live capture client; the buffer is read only
        // between GetBuffer and ReleaseBuffer.
        unsafe {
            if opened.capture.GetNextPacketSize()? == 0 {
                return Ok(());
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0_u32;
            let mut flags = 0_u32;
            let mut qpc = 0_u64;
            opened
                .capture
                .GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
            if frames == 0 {
                return Ok(());
            }
            let flags = flags as i32;
            let count = frames as usize;
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 != 0;
            let samples = if silent {
                Some(Samples::Silent)
            } else {
                samples(data, count, wave)
            };
            let now = clock.now();
            match samples {
                Some(samples) => packets.push(
                    Packet {
                        samples,
                        frames: count,
                        device_time: (flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 == 0
                            && qpc != 0)
                            .then(|| qpc.saturating_mul(100)),
                        discontinuity: flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 != 0,
                    },
                    now,
                ),
                None => packets.lose(u64::from(frames)),
            }
            opened.capture.ReleaseBuffer(frames)?;
        }
    }
}

/// Why the stream ended, for a failed WASAPI call.
fn classify(e: &windows::core::Error, opened: &Opened) -> AudioIoError {
    let code = e.code();
    if code != AUDCLNT_E_DEVICE_INVALIDATED && code != AUDCLNT_E_RESOURCES_INVALIDATED {
        return hresult(code);
    }
    match &opened.device {
        // Still active: the stream was invalidated by a format change.
        Some(id) => {
            let still = enumerator().is_ok_and(|e| {
                [EndpointKind::Input, EndpointKind::OutputLoopback]
                    .into_iter()
                    .any(|kind| device(&e, kind, id).is_ok())
            });
            if still {
                AudioIoError::FormatChanged
            } else {
                AudioIoError::EndpointLost
            }
        }
        None => AudioIoError::EndpointLost,
    }
}

fn capture_thread(setup: Setup, started: Started) {
    let _thread = Owned::new();
    // SAFETY: joins the MTA on this thread; balanced by CoUninitialize below.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let mut task = 0_u32;
    // SAFETY: registers this thread with MMCSS; reverted below.
    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task) }.ok();
    run(&setup, started);
    if let Some(mmcss) = mmcss {
        // SAFETY: the handle from AvSetMmThreadCharacteristicsW on this thread.
        let _ = unsafe { AvRevertMmThreadCharacteristics(mmcss) };
    }
    if com {
        // SAFETY: balances the successful CoInitializeEx above; every COM object of
        // this thread was released by `run`.
        unsafe { CoUninitialize() };
    }
}

fn run(setup: &Setup, started: Started) {
    let opened = match open_stream(&setup.target, &setup.wave) {
        Ok(opened) => opened,
        Err(e) => {
            let _ = started.send(Err(e));
            return;
        }
    };
    let _stream = Owned::new();
    let wave = setup.wave;
    let rate = wave.format.sample_rate;
    let capacity = ring::capacity_frames(rate, setup.buffer, opened.buffer_frames);
    let (producer, consumer) = ring::channel(wave.format, capacity, ring::DEFAULT_MARKS);
    let period_frames = ring::frames_for(rate, opened.period);
    let fill = match setup.target {
        Target::Device {
            loopback: false, ..
        } => None,
        _ => Some((opened.period * FILL_PERIODS, period_frames)),
    };
    let mut packets = Packets::new(
        producer,
        rate,
        wave.source_channels,
        fill,
        setup.clock.now(),
        Arc::clone(&setup.last_end),
    );
    // SAFETY: plain COM call on a live client.
    if let Err(e) = unsafe { opened.client.Start() } {
        let _ = started.send(Err(error(e)));
        return;
    }
    if started.send(Ok(consumer)).is_err() {
        // `start` gave up waiting.
        // SAFETY: plain COM call on a live client.
        let _ = unsafe { opened.client.Stop() };
        return;
    }
    let timeout = u32::try_from(opened.period.as_millis().max(1)).unwrap_or(10);
    let mut handles = vec![setup.stop.0, opened.event.0];
    if let Some(process) = &opened.process {
        handles.push(process.0);
    }
    loop {
        // SAFETY: every handle is live for the duration of the wait.
        let woke = unsafe { WaitForMultipleObjects(&handles, false, timeout) };
        match woke.0 {
            // The stop event.
            0 => break,
            // The audio event, or a period without one (loopback while silent).
            1 => {}
            w if w == WAIT_TIMEOUT.0 => {}
            // The captured process exited.
            2 => {
                setup.status.set(AudioIoError::EndpointLost);
                break;
            }
            // WAIT_FAILED or an abandoned handle: never spin on it.
            _ => {
                // SAFETY: no preconditions.
                let mut code = unsafe { GetLastError() }.to_hresult();
                if code.is_ok() {
                    // An abandoned handle sets no error code.
                    code = E_FAIL;
                }
                setup.status.set(hresult(code));
                break;
            }
        }
        let os = qpc_nanos();
        let now = setup.clock.now();
        packets.observe(os, now);
        if let Err(e) = drain(&opened, &wave, &mut packets, setup.clock.as_ref()) {
            setup.status.set(classify(&e, &opened));
            break;
        }
        packets.fill(setup.clock.now());
    }
    // SAFETY: plain COM call on a live client.
    let _ = unsafe { opened.client.Stop() };
}
