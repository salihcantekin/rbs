//! The macOS backend. Overview in the crate documentation.
//!
//! Threads: `enumerate`, `open_input`, `start` and `stop` run on a control thread; the
//! IO proc runs on the HAL's real-time IO thread and only touches its own `Capture`
//! (stamping and the ring's producer); property listeners run on the HAL's
//! notification thread and only store an atomic status; `read` runs on the audio
//! thread and only touches the ring's consumer and that status.

use std::ffi::{CStr, c_void};
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::Duration;

use block2::RcBlock;
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use objc2_core_audio::{
    AudioConvertHostTimeToNanos, AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID,
    AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareDestroyAggregateDevice, AudioObjectAddPropertyListener,
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertyScope, AudioObjectPropertySelector,
    AudioObjectRemovePropertyListener, CATapDescription, kAudioAggregateDeviceIsPrivateKey,
    kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey,
    kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyBufferFrameSizeRange,
    kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyStreamConfiguration,
    kAudioDevicePropertyStreams, kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioHardwarePropertyProcessObjectList, kAudioHardwarePropertyTranslatePIDToProcessObject,
    kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID,
    kAudioStreamPropertyVirtualFormat, kAudioSubDeviceUIDKey, kAudioSubTapDriftCompensationKey,
    kAudioSubTapUIDKey,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, AudioTimeStampFlags,
    AudioValueRange, kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat, kAudioFormatLinearPCM,
};
use objc2_core_foundation::{CFDictionary, CFRetained, CFString};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSProcessInfo, NSString};

use rbs_audio_io::ring::{self, RingConsumer};
use rbs_audio_io::{
    AudioBackend, AudioEndpoint, AudioInputStream, AudioIoError, AudioRead, EndpointKind,
    ErrorDomain, InputRequest, InputStats, Permission, caps,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{AudioFormat, ChannelLayout, MAX_CHANNELS};
use rbs_core::registry::{AdapterFactory, AdapterId, HARDWARE, ProbeError};
use rbs_core::time::Clock;
use rbs_core::timing::SampleRate;

use crate::packets::{Buffer, Cycle, Packets};
use crate::{ADAPTER_ID, Owned, PROCESS_TAP_VERSION, ids, layout};

const SYSTEM: AudioObjectID = kAudioObjectSystemObject as AudioObjectID;

/// UID prefix of the private aggregate devices the adapter creates. They are listed
/// to this process while they exist, so `enumerate` skips them.
const AGGREGATE_PREFIX: &str = "org.rbs.audio-io.coreaudio.";

/// Largest IO buffer assumed when the device does not report its range.
const DEFAULT_MAX_FRAMES: usize = 4_096;

/// Largest IO buffer the ring is sized for (the HAL's limit is far larger, but no
/// device in practice runs above it).
const MAX_FRAMES: usize = 16_384;

/// `kAudioHardwareIllegalOperationError` ('nope').
const ILLEGAL_OPERATION: i32 = 0x6e6f_7065;

fn os(status: i32) -> AudioIoError {
    AudioIoError::Backend {
        domain: ErrorDomain::OsStatus,
        code: i64::from(status),
        recoverable: true,
    }
}

fn check(status: i32) -> Result<(), AudioIoError> {
    if status == 0 { Ok(()) } else { Err(os(status)) }
}

fn address(
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// A fixed-size property, with an optional qualifier.
fn get_with<T: Copy>(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
    qualifier: Option<(*const c_void, u32)>,
) -> Option<T> {
    let address = address(selector, scope);
    let mut value = MaybeUninit::<T>::zeroed();
    let mut size = std::mem::size_of::<T>() as u32;
    let (q, q_size) = qualifier.unwrap_or((std::ptr::null(), 0));
    // SAFETY: `value` has room for `size` bytes and the qualifier points to `q_size`
    // readable bytes (or is null with size 0).
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            q_size,
            q,
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    // SAFETY: the HAL filled exactly `size_of::<T>()` bytes of a plain-data `T`.
    (status == 0 && size as usize == std::mem::size_of::<T>())
        .then(|| unsafe { value.assume_init() })
}

fn get<T: Copy>(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Option<T> {
    get_with(object, selector, scope, None)
}

/// A variable-size property as 8-byte aligned words, and its size in bytes.
fn get_bytes(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Option<(Vec<u64>, usize)> {
    let address = address(selector, scope);
    let mut size = 0_u32;
    // SAFETY: valid address and out pointer, no qualifier.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 {
        return None;
    }
    let mut words = vec![0_u64; (size as usize).div_ceil(8).max(1)];
    // SAFETY: `words` holds at least `size` bytes.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(words.as_mut_ptr().cast()).unwrap_or(NonNull::dangling()),
        )
    };
    (status == 0).then_some((words, size as usize))
}

fn get_ids(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Vec<AudioObjectID> {
    let Some((words, size)) = get_bytes(object, selector, scope) else {
        return Vec::new();
    };
    let count = size / std::mem::size_of::<AudioObjectID>();
    // SAFETY: the words hold `size` bytes of AudioObjectIDs, 8-byte aligned.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<AudioObjectID>(), count) }.to_vec()
}

fn get_string(object: AudioObjectID, selector: AudioObjectPropertySelector) -> Option<String> {
    let raw: *const CFString = get(object, selector, kAudioObjectPropertyScopeGlobal)?;
    // SAFETY: the HAL returns a +1 CFString the caller releases.
    let string = unsafe { CFRetained::from_raw(NonNull::new(raw.cast_mut())?) };
    Some(string.to_string())
}

/// Channels of each buffer of a device's stream configuration in `scope`.
fn buffer_channels(device: AudioObjectID, scope: AudioObjectPropertyScope) -> Vec<usize> {
    let Some((words, size)) = get_bytes(device, kAudioDevicePropertyStreamConfiguration, scope)
    else {
        return Vec::new();
    };
    let list = words.as_ptr().cast::<AudioBufferList>();
    if size < std::mem::size_of::<u32>() {
        return Vec::new();
    }
    // SAFETY: the words hold an AudioBufferList of `size` bytes, aligned for it.
    let count = unsafe { (*list).mNumberBuffers } as usize;
    let header = std::mem::offset_of!(AudioBufferList, mBuffers);
    let count = count.min(size.saturating_sub(header) / std::mem::size_of::<AudioBuffer>());
    // SAFETY: as above; `count` buffers fit in `size` bytes.
    let buffers = unsafe { (&raw const (*list).mBuffers).cast::<AudioBuffer>() };
    (0..count)
        // SAFETY: `i < count`.
        .map(|i| unsafe { (*buffers.add(i)).mNumberChannels } as usize)
        .collect()
}

fn channels(device: AudioObjectID, scope: AudioObjectPropertyScope) -> usize {
    buffer_channels(device, scope).iter().sum()
}

fn nominal_rate(device: AudioObjectID) -> Option<SampleRate> {
    let rate: f64 = get(
        device,
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal,
    )?;
    SampleRate::new(rate.round() as u32)
}

fn uid(device: AudioObjectID) -> Option<String> {
    get_string(device, kAudioDevicePropertyDeviceUID)
}

fn device_by_uid(uid: &str) -> Option<AudioObjectID> {
    let string = CFString::from_str(uid);
    let pointer: *const CFString = &*string;
    let device: AudioObjectID = get_with(
        SYSTEM,
        kAudioHardwarePropertyTranslateUIDToDevice,
        kAudioObjectPropertyScopeGlobal,
        Some((
            (&raw const pointer).cast(),
            std::mem::size_of::<*const CFString>() as u32,
        )),
    )?;
    (device != 0).then_some(device)
}

fn default_device(selector: AudioObjectPropertySelector) -> Option<AudioObjectID> {
    get::<AudioObjectID>(SYSTEM, selector, kAudioObjectPropertyScopeGlobal).filter(|d| *d != 0)
}

fn process_object(pid: i32) -> Option<AudioObjectID> {
    let object: AudioObjectID = get_with(
        SYSTEM,
        kAudioHardwarePropertyTranslatePIDToProcessObject,
        kAudioObjectPropertyScopeGlobal,
        Some(((&raw const pid).cast(), std::mem::size_of::<i32>() as u32)),
    )?;
    (object != 0).then_some(object)
}

fn process_name(pid: i32) -> Option<String> {
    let mut buffer = [0_u8; 256];
    // SAFETY: `buffer` holds `len` bytes; proc_name writes a NUL-terminated name.
    let len = unsafe { libc::proc_name(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    let len = usize::try_from(len).ok().filter(|l| *l > 0)?;
    Some(String::from_utf8_lossy(&buffer[..len]).into_owned())
}

/// Whether every input stream of `device` hands IO procs 32-bit native float, the
/// only sample type the IO proc reads.
fn input_is_float(device: AudioObjectID) -> bool {
    get_ids(
        device,
        kAudioDevicePropertyStreams,
        kAudioObjectPropertyScopeInput,
    )
    .into_iter()
    .all(|stream| {
        get::<AudioStreamBasicDescription>(
            stream,
            kAudioStreamPropertyVirtualFormat,
            kAudioObjectPropertyScopeGlobal,
        )
        .is_some_and(|f| {
            f.mFormatID == kAudioFormatLinearPCM
                && f.mFormatFlags & kAudioFormatFlagIsFloat != 0
                && f.mFormatFlags & kAudioFormatFlagIsBigEndian == 0
                && f.mBitsPerChannel == 32
        })
    })
}

/// The real devices (not this adapter's aggregates) with channels in `scope`.
fn devices(scope: AudioObjectPropertyScope) -> Vec<(AudioObjectID, String, usize)> {
    get_ids(
        SYSTEM,
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
    )
    .into_iter()
    .filter_map(|device| {
        let uid = uid(device)?;
        if uid.starts_with(AGGREGATE_PREFIX) {
            return None;
        }
        let channels = channels(device, scope);
        (channels > 0).then_some((device, uid, channels))
    })
    .collect()
}

// ---------------------------------------------------------------------------------
// Process taps (macOS 14.2+, looked up at run time so the binary loads on older
// systems)

type CreateTap = unsafe extern "C" fn(*mut AnyObject, *mut AudioObjectID) -> i32;
type DestroyTap = unsafe extern "C" fn(AudioObjectID) -> i32;

#[derive(Clone, Copy)]
struct TapApi {
    create: CreateTap,
    destroy: DestroyTap,
}

fn tap_api() -> Option<TapApi> {
    static API: OnceLock<Option<TapApi>> = OnceLock::new();
    *API.get_or_init(|| {
        let version = NSProcessInfo::processInfo().operatingSystemVersion();
        if (version.majorVersion, version.minorVersion) < PROCESS_TAP_VERSION {
            return None;
        }
        let symbol = |name: &CStr| {
            // SAFETY: dlsym with RTLD_DEFAULT and a NUL-terminated name.
            let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
            (!p.is_null()).then_some(p)
        };
        let create = symbol(c"AudioHardwareCreateProcessTap")?;
        let destroy = symbol(c"AudioHardwareDestroyProcessTap")?;
        // SAFETY: the symbols are CoreAudio's functions with these C signatures
        // (AudioHardware.h, macOS 14.2).
        unsafe {
            Some(TapApi {
                create: std::mem::transmute::<*mut c_void, CreateTap>(create),
                destroy: std::mem::transmute::<*mut c_void, DestroyTap>(destroy),
            })
        }
    })
}

fn taps_supported() -> bool {
    tap_api().is_some()
}

// ---------------------------------------------------------------------------------
// Permissions

fn microphone() -> Permission {
    // SAFETY: `AVMediaTypeAudio` is a framework constant; the call only reads the
    // authorization state.
    let status = unsafe {
        match AVMediaTypeAudio {
            Some(audio) => AVCaptureDevice::authorizationStatusForMediaType(audio),
            None => AVAuthorizationStatus::Denied,
        }
    };
    match status {
        AVAuthorizationStatus::Authorized => Permission::Granted,
        AVAuthorizationStatus::NotDetermined => Permission::NotDetermined,
        _ => Permission::Denied,
    }
}

// ---------------------------------------------------------------------------------
// Backend

/// Backend of the CoreAudio adapter. Creating it does nothing.
#[derive(Debug, Default)]
pub struct CoreAudioBackend {
    _private: (),
}

impl CoreAudioBackend {
    /// Creates the backend. Does nothing until used.
    pub fn new() -> Self {
        Self::default()
    }

    fn kinds() -> Vec<EndpointKind> {
        let mut kinds = vec![EndpointKind::Input];
        if taps_supported() {
            kinds.extend([EndpointKind::OutputLoopback, EndpointKind::Application]);
        }
        kinds
    }

    fn stereo(rate: SampleRate) -> AudioFormat {
        AudioFormat::new(rate, ChannelLayout::Stereo).unwrap_or(AudioFormat::STEREO_48K)
    }

    fn input_endpoints() -> Vec<AudioEndpoint> {
        let default = default_device(kAudioHardwarePropertyDefaultInputDevice);
        devices(kAudioObjectPropertyScopeInput)
            .into_iter()
            .filter_map(|(device, uid, channels)| {
                let format = AudioFormat::new(nominal_rate(device)?, layout(channels)?).ok()?;
                let name = get_string(device, kAudioObjectPropertyName).unwrap_or(uid.clone());
                Some(
                    AudioEndpoint::new(
                        ids::endpoint(EndpointKind::Input, &uid),
                        EndpointKind::Input,
                        name,
                        format,
                    )
                    .with_default(Some(device) == default),
                )
            })
            .collect()
    }

    fn output_endpoints() -> Vec<AudioEndpoint> {
        let default = default_device(kAudioHardwarePropertyDefaultOutputDevice);
        devices(kAudioObjectPropertyScopeOutput)
            .into_iter()
            .filter_map(|(device, uid, _)| {
                let format = Self::stereo(nominal_rate(device)?);
                let name = get_string(device, kAudioObjectPropertyName).unwrap_or(uid.clone());
                Some(
                    AudioEndpoint::new(
                        ids::endpoint(EndpointKind::OutputLoopback, &uid),
                        EndpointKind::OutputLoopback,
                        name,
                        format,
                    )
                    .with_default(Some(device) == default),
                )
            })
            .collect()
    }

    fn application_format() -> AudioFormat {
        Self::stereo(
            default_device(kAudioHardwarePropertyDefaultOutputDevice)
                .and_then(nominal_rate)
                .unwrap_or(SampleRate::HZ_48000),
        )
    }

    fn application_endpoints() -> Vec<AudioEndpoint> {
        let format = Self::application_format();
        let own = std::process::id() as i32;
        let mut pids: Vec<i32> = Vec::new();
        let mut found = Vec::new();
        for process in get_ids(
            SYSTEM,
            kAudioHardwarePropertyProcessObjectList,
            kAudioObjectPropertyScopeGlobal,
        ) {
            let Some(pid) = get::<i32>(
                process,
                kAudioProcessPropertyPID,
                kAudioObjectPropertyScopeGlobal,
            ) else {
                continue;
            };
            let playing = get::<u32>(
                process,
                kAudioProcessPropertyIsRunningOutput,
                kAudioObjectPropertyScopeGlobal,
            )
            .is_some_and(|v| v != 0);
            if !playing || pid <= 0 || pid == own || pids.contains(&pid) {
                continue;
            }
            pids.push(pid);
            let name = process_name(pid)
                .or_else(|| get_string(process, kAudioProcessPropertyBundleID))
                .unwrap_or_else(|| format!("Process {pid}"));
            found.push(AudioEndpoint::new(
                ids::endpoint(EndpointKind::Application, &pid.to_string()),
                EndpointKind::Application,
                name,
                format,
            ));
        }
        found
    }
}

impl AdapterFactory for CoreAudioBackend {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "CoreAudio"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        // Cheap: the OS version and two symbol lookups; no HAL calls.
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
            .with(caps::INPUT_PERMISSION, CapabilityValue::Flag(true))
            .with(HARDWARE, CapabilityValue::Flag(true)))
    }
}

impl AudioBackend for CoreAudioBackend {
    fn enumerate(&self, kind: EndpointKind) -> Result<Vec<AudioEndpoint>, AudioIoError> {
        if !Self::kinds().contains(&kind) {
            return Err(AudioIoError::Unsupported("process taps need macOS 14.4"));
        }
        Ok(match kind {
            EndpointKind::Input => Self::input_endpoints(),
            EndpointKind::OutputLoopback => Self::output_endpoints(),
            _ => Self::application_endpoints(),
        })
    }

    fn open_input(
        &self,
        request: &InputRequest,
    ) -> Result<Box<dyn AudioInputStream>, AudioIoError> {
        let (kind, native) = ids::parse(&request.endpoint).ok_or(AudioIoError::EndpointNotFound)?;
        if !Self::kinds().contains(&kind) {
            return Err(AudioIoError::Unsupported("process taps need macOS 14.4"));
        }
        let (target, native_format) = match kind {
            EndpointKind::Input => {
                let device = device_by_uid(native).ok_or(AudioIoError::EndpointNotFound)?;
                let channels = channels(device, kAudioObjectPropertyScopeInput);
                let format = nominal_rate(device)
                    .zip(layout(channels))
                    .and_then(|(rate, layout)| AudioFormat::new(rate, layout).ok())
                    .ok_or(AudioIoError::EndpointNotFound)?;
                if !input_is_float(device) {
                    return Err(AudioIoError::FormatUnsupported);
                }
                match microphone() {
                    Permission::Granted => {}
                    Permission::NotDetermined => {
                        return Err(AudioIoError::PermissionNotDetermined);
                    }
                    _ => return Err(AudioIoError::PermissionDenied),
                }
                (Target::Input(native.to_owned()), format)
            }
            EndpointKind::OutputLoopback => {
                let device = device_by_uid(native)
                    .filter(|d| channels(*d, kAudioObjectPropertyScopeOutput) > 0)
                    .ok_or(AudioIoError::EndpointNotFound)?;
                let rate = nominal_rate(device).ok_or(AudioIoError::EndpointNotFound)?;
                (Target::Output(native.to_owned()), Self::stereo(rate))
            }
            _ => {
                let pid: i32 = native.parse().map_err(|_| AudioIoError::EndpointNotFound)?;
                process_object(pid).ok_or(AudioIoError::EndpointNotFound)?;
                (Target::Application(pid), Self::application_format())
            }
        };
        let endpoint = AudioEndpoint::new(request.endpoint.clone(), kind, "", native_format);
        Ok(Box::new(CoreAudioStream {
            target,
            format: request.format_for(&endpoint),
            clock: Arc::clone(&request.clock),
            buffer: request.clamped_buffer(),
            last_end: Arc::new(AtomicU64::new(0)),
            active: None,
            totals: InputStats::default(),
        }))
    }

    fn permission(&self, kind: EndpointKind) -> Permission {
        match kind {
            EndpointKind::Input => microphone(),
            // No public query exists for the system audio recording permission.
            _ => Permission::Granted,
        }
    }

    fn request_permission(&self, kind: EndpointKind) -> Result<Permission, AudioIoError> {
        if kind != EndpointKind::Input || microphone() != Permission::NotDetermined {
            return Ok(self.permission(kind));
        }
        // SAFETY: `AVMediaTypeAudio` is a framework constant.
        let Some(audio) = (unsafe { AVMediaTypeAudio }) else {
            return Ok(Permission::Denied);
        };
        let (tx, rx) = mpsc::sync_channel::<bool>(1);
        let block = RcBlock::new(move |granted: Bool| {
            let _ = tx.try_send(granted.as_bool());
        });
        // SAFETY: a valid media type and a completion block that only sends on a
        // channel.
        unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &block) };
        // Waits for the user's answer (control thread only, audio-io-port.md).
        match rx.recv() {
            Ok(true) => Ok(Permission::Granted),
            Ok(false) => Ok(Permission::Denied),
            Err(_) => Ok(microphone()),
        }
    }
}

// ---------------------------------------------------------------------------------
// Stream

/// What a stream captures.
#[derive(Debug, Clone)]
enum Target {
    /// An input device, by UID.
    Input(String),
    /// An output device's loopback, by UID.
    Output(String),
    /// A process, by pid.
    Application(i32),
}

/// What the HAL's threads report to `read`.
#[derive(Default)]
struct Status {
    state: AtomicU8,
    code: AtomicI64,
}

const STATE_RUNNING: u8 = 0;
const STATE_LOST: u8 = 1;
const STATE_FORMAT_CHANGED: u8 = 2;

impl Status {
    fn set(&self, state: u8) {
        let _ =
            self.state
                .compare_exchange(STATE_RUNNING, state, Ordering::AcqRel, Ordering::Acquire);
    }

    /// **Real-time**: an atomic load.
    fn error(&self) -> Option<AudioIoError> {
        match self.state.load(Ordering::Acquire) {
            STATE_RUNNING => None,
            STATE_LOST => Some(AudioIoError::EndpointLost),
            STATE_FORMAT_CHANGED => Some(AudioIoError::FormatChanged),
            _ => Some(os(self.code.load(Ordering::Relaxed) as i32)),
        }
    }
}

/// Which input buffers of a cycle hold the stream's channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Buffers {
    /// All of them, from the first (an input device).
    All,
    /// The last one (the tap's, after the output device's own inputs in the
    /// aggregate).
    Last,
}

/// The IO proc's state. Lives in a `Box` handed to the HAL as client data; touched
/// only by the IO proc between `AudioDeviceStart` and `AudioDeviceStop`.
struct Capture {
    packets: Packets,
    clock: Arc<dyn Clock>,
    buffers: Buffers,
    channels: usize,
}

impl Capture {
    /// One IO cycle. **Real-time**: no allocation, locks, I/O or logging.
    fn cycle(&mut self, now: &AudioTimeStamp, list: &AudioBufferList, time: &AudioTimeStamp) {
        // SAFETY: plain conversions of host time, no preconditions.
        let os_now = unsafe { AudioConvertHostTimeToNanos(now.mHostTime) };
        self.packets.observe(os_now, self.clock.now());
        let count = list.mNumberBuffers as usize;
        let first = match self.buffers {
            Buffers::All => 0,
            Buffers::Last => count.saturating_sub(1),
        };
        let base = (&raw const list.mBuffers).cast::<AudioBuffer>();
        let mut buffers = [Buffer::default(); MAX_CHANNELS];
        let mut used = 0;
        let mut channels = 0;
        let mut frames = usize::MAX;
        for i in first..count {
            if used == MAX_CHANNELS || channels >= self.channels {
                break;
            }
            // SAFETY: the HAL passes `count` buffers in the list.
            let buffer = unsafe { *base.add(i) };
            let data = buffer.mData.cast::<f32>().cast_const();
            let n = buffer.mNumberChannels as usize;
            if n == 0 || data.is_null() || !data.is_aligned() {
                continue;
            }
            let len = buffer.mDataByteSize as usize / std::mem::size_of::<f32>();
            // SAFETY: the buffer holds `mDataByteSize` bytes of native f32 samples
            // (checked at start) for the duration of this call.
            let samples = unsafe { std::slice::from_raw_parts(data, len) };
            frames = frames.min(len / n);
            buffers[used] = Buffer {
                samples,
                channels: n,
            };
            used += 1;
            channels += n;
        }
        if used == 0 || frames == 0 || frames == usize::MAX {
            return;
        }
        let host_nanos = time
            .mFlags
            .contains(AudioTimeStampFlags::HostTimeValid)
            // SAFETY: as above.
            .then(|| unsafe { AudioConvertHostTimeToNanos(time.mHostTime) });
        let sample_time = time
            .mFlags
            .contains(AudioTimeStampFlags::SampleTimeValid)
            .then_some(time.mSampleTime);
        self.packets.push(
            Cycle {
                buffers: &buffers[..used],
                frames,
                host_nanos,
                sample_time,
            },
            self.clock.now(),
        );
    }
}

unsafe extern "C-unwind" fn io_proc(
    _device: AudioObjectID,
    now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    input_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is the `Capture` registered with this IO proc, used by nothing
    // else until the proc is stopped; the HAL's pointers are valid for this call.
    unsafe {
        let capture = &mut *client.cast::<Capture>();
        capture.cycle(now.as_ref(), input.as_ref(), input_time.as_ref());
    }
    0
}

/// What the property listener watches.
struct Watch {
    status: Arc<Status>,
    /// The device whose life and rate end the stream.
    device: AudioObjectID,
    rate: SampleRate,
    /// The captured process object.
    process: Option<AudioObjectID>,
}

#[allow(non_upper_case_globals)]
unsafe extern "C-unwind" fn on_property(
    _object: AudioObjectID,
    count: u32,
    addresses: NonNull<AudioObjectPropertyAddress>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is the session's `Watch`, freed only after every listener was
    // removed; the HAL passes `count` addresses.
    let (watch, addresses) = unsafe {
        (
            &*client.cast::<Watch>(),
            std::slice::from_raw_parts(addresses.as_ptr(), count as usize),
        )
    };
    for address in addresses {
        match address.mSelector {
            kAudioDevicePropertyDeviceIsAlive => {
                let alive = get::<u32>(
                    watch.device,
                    kAudioDevicePropertyDeviceIsAlive,
                    kAudioObjectPropertyScopeGlobal,
                );
                if alive.is_none_or(|a| a == 0) {
                    watch.status.set(STATE_LOST);
                }
            }
            kAudioDevicePropertyNominalSampleRate => {
                if nominal_rate(watch.device) != Some(watch.rate) {
                    watch.status.set(STATE_FORMAT_CHANGED);
                }
            }
            kAudioHardwarePropertyProcessObjectList => {
                if let Some(process) = watch.process {
                    let list = get_ids(
                        SYSTEM,
                        kAudioHardwarePropertyProcessObjectList,
                        kAudioObjectPropertyScopeGlobal,
                    );
                    if !list.contains(&process) {
                        watch.status.set(STATE_LOST);
                    }
                }
            }
            _ => {}
        }
    }
    0
}

/// Everything an Active stream holds in the HAL, released in reverse on drop.
#[derive(Default)]
struct Session {
    tap: Option<(AudioObjectID, Owned)>,
    aggregate: Option<(AudioObjectID, Owned)>,
    /// The IO device, its proc and the proc's state.
    io: Option<(AudioObjectID, AudioDeviceIOProcID, *mut Capture, Owned)>,
    started: bool,
    watch: Option<*mut Watch>,
    listeners: Vec<(AudioObjectID, AudioObjectPropertyAddress)>,
}

// SAFETY: the raw pointers are owned by the session (boxes it frees itself) and the
// HAL objects are process-wide ids usable from any thread.
unsafe impl Send for Session {}

impl Session {
    fn listen(
        &mut self,
        object: AudioObjectID,
        selector: AudioObjectPropertySelector,
    ) -> Result<(), AudioIoError> {
        let Some(watch) = self.watch else {
            return Ok(());
        };
        let address = address(selector, kAudioObjectPropertyScopeGlobal);
        // SAFETY: a valid address and listener; `watch` lives until the listener is
        // removed in `drop`.
        check(unsafe {
            AudioObjectAddPropertyListener(
                object,
                NonNull::from(&address),
                Some(on_property),
                watch.cast(),
            )
        })?;
        self.listeners.push((object, address));
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: each id and pointer was created by this session and is released
        // once, the IO proc first (AudioDeviceStop from a non-IO thread returns after
        // the last cycle), so no HAL thread uses a box when it is freed.
        unsafe {
            if let Some((device, proc_id, capture, _owned)) = self.io.take() {
                if self.started {
                    let _ = AudioDeviceStop(device, proc_id);
                }
                let _ = AudioDeviceDestroyIOProcID(device, proc_id);
                drop(Box::from_raw(capture));
            }
            if let Some(watch) = self.watch.take() {
                for (object, address) in self.listeners.drain(..) {
                    let _ = AudioObjectRemovePropertyListener(
                        object,
                        NonNull::from(&address),
                        Some(on_property),
                        watch.cast(),
                    );
                }
                drop(Box::from_raw(watch));
            }
            if let Some((aggregate, _owned)) = self.aggregate.take() {
                let _ = AudioHardwareDestroyAggregateDevice(aggregate);
            }
            if let Some((tap, _owned)) = self.tap.take()
                && let Some(api) = tap_api()
            {
                let _ = (api.destroy)(tap);
            }
        }
    }
}

/// A private process tap: of an output device's stream (mixed down to stereo) or of
/// one process (stereo). Returns the tap and its UID.
fn create_tap(
    output: Option<&str>,
    process: Option<AudioObjectID>,
) -> Result<(AudioObjectID, String), AudioIoError> {
    let api = tap_api().ok_or(AudioIoError::Unsupported("process taps need macOS 14.4"))?;
    // SAFETY: CATapDescription initialisers and setters on a fresh object; the class
    // exists (macOS 14.2+, checked by `tap_api`).
    let description = unsafe {
        let description = match (output, process) {
            (Some(uid), _) => {
                let d = CATapDescription::initExcludingProcesses_andDeviceUID_withStream(
                    CATapDescription::alloc(),
                    &NSArray::new(),
                    &NSString::from_str(uid),
                    0,
                );
                d.setMixdown(true);
                d.setMono(false);
                d
            }
            (None, Some(process)) => CATapDescription::initStereoMixdownOfProcesses(
                CATapDescription::alloc(),
                &NSArray::from_retained_slice(&[NSNumber::new_u32(process)]),
            ),
            (None, None) => return Err(AudioIoError::EndpointNotFound),
        };
        description.setPrivate(true);
        description.setName(&NSString::from_str("RBS audio capture"));
        description
    };
    // SAFETY: reads the description's UUID.
    let tap_uid = unsafe { description.UUID().UUIDString() }.to_string();
    let mut tap: AudioObjectID = 0;
    // SAFETY: a valid tap description and out pointer.
    let status =
        unsafe { (api.create)(Retained::as_ptr(&description).cast_mut().cast(), &mut tap) };
    check(status)?;
    if tap == 0 {
        return Err(os(ILLEGAL_OPERATION));
    }
    Ok((tap, tap_uid))
}

/// A private aggregate device clocked by `output` that reads the tap `tap_uid`.
fn create_aggregate(output: &str, tap_uid: &str) -> Result<AudioObjectID, AudioIoError> {
    let key = |k: &CStr| NSString::from_str(k.to_str().unwrap_or_default());
    let sub_device = NSDictionary::from_slices(
        &[&*key(kAudioSubDeviceUIDKey)],
        &[&*NSString::from_str(output) as &AnyObject],
    );
    let yes = NSNumber::new_bool(true);
    let tap = NSDictionary::from_slices(
        &[
            &*key(kAudioSubTapUIDKey),
            &*key(kAudioSubTapDriftCompensationKey),
        ],
        &[&*NSString::from_str(tap_uid) as &AnyObject, &*yes],
    );
    let sub_devices = NSArray::from_retained_slice(&[sub_device]);
    let taps = NSArray::from_retained_slice(&[tap]);
    let name = NSString::from_str("RBS audio capture");
    let uid = NSString::from_str(&format!("{AGGREGATE_PREFIX}{tap_uid}"));
    let output = NSString::from_str(output);
    let keys = [
        key(kAudioAggregateDeviceNameKey),
        key(kAudioAggregateDeviceUIDKey),
        key(kAudioAggregateDeviceMainSubDeviceKey),
        key(kAudioAggregateDeviceIsPrivateKey),
        key(kAudioAggregateDeviceTapAutoStartKey),
        key(kAudioAggregateDeviceSubDeviceListKey),
        key(kAudioAggregateDeviceTapListKey),
    ];
    let values: [&AnyObject; 7] = [&name, &uid, &output, &yes, &yes, &sub_devices, &taps];
    let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let description = NSDictionary::from_slices(&keys, &values);
    // SAFETY: NSDictionary is toll-free bridged to CFDictionary.
    let description: &CFDictionary =
        unsafe { &*Retained::as_ptr(&description).cast::<CFDictionary>() };
    let mut device: AudioObjectID = 0;
    // SAFETY: a valid description and out pointer.
    check(unsafe { AudioHardwareCreateAggregateDevice(description, NonNull::from(&mut device)) })?;
    if device == 0 {
        return Err(os(ILLEGAL_OPERATION));
    }
    Ok(device)
}

struct Active {
    session: Session,
    consumer: RingConsumer,
    status: Arc<Status>,
}

struct CoreAudioStream {
    target: Target,
    format: AudioFormat,
    clock: Arc<dyn Clock>,
    buffer: Duration,
    /// Master time at which the last cycle ended, in nanoseconds, so a restarted
    /// stream starts later.
    last_end: Arc<AtomicU64>,
    active: Option<Active>,
    /// Counters of rings closed by `stop`.
    totals: InputStats,
}

impl CoreAudioStream {
    fn activate(&mut self) -> Result<Active, AudioIoError> {
        let status = Arc::new(Status::default());
        let mut session = Session::default();
        let rate = self.format.sample_rate;
        let (io_device, watched, process, buffers) = match &self.target {
            Target::Input(uid) => {
                let device = device_by_uid(uid).ok_or(AudioIoError::EndpointLost)?;
                let channels = channels(device, kAudioObjectPropertyScopeInput);
                if nominal_rate(device) != Some(rate)
                    || layout(channels) != Some(self.format.channels)
                {
                    return Err(AudioIoError::FormatChanged);
                }
                (device, device, None, Buffers::All)
            }
            Target::Output(uid) => {
                let device = device_by_uid(uid).ok_or(AudioIoError::EndpointLost)?;
                if nominal_rate(device) != Some(rate) {
                    return Err(AudioIoError::FormatChanged);
                }
                let (tap, tap_uid) = create_tap(Some(uid), None)?;
                session.tap = Some((tap, Owned::new()));
                let aggregate = create_aggregate(uid, &tap_uid)?;
                session.aggregate = Some((aggregate, Owned::new()));
                (aggregate, device, None, Buffers::Last)
            }
            Target::Application(pid) => {
                let process = process_object(*pid).ok_or(AudioIoError::EndpointLost)?;
                let output = default_device(kAudioHardwarePropertyDefaultOutputDevice)
                    .ok_or(AudioIoError::EndpointLost)?;
                let output_uid = uid(output).ok_or(AudioIoError::EndpointLost)?;
                if nominal_rate(output) != Some(rate) {
                    return Err(AudioIoError::FormatChanged);
                }
                let (tap, tap_uid) = create_tap(None, Some(process))?;
                session.tap = Some((tap, Owned::new()));
                let aggregate = create_aggregate(&output_uid, &tap_uid)?;
                session.aggregate = Some((aggregate, Owned::new()));
                (aggregate, output, Some(process), Buffers::Last)
            }
        };
        if buffers == Buffers::Last {
            let tap_channels = buffer_channels(io_device, kAudioObjectPropertyScopeInput)
                .last()
                .copied()
                .unwrap_or(0);
            if nominal_rate(io_device) != Some(rate) || tap_channels != 2 {
                return Err(AudioIoError::FormatChanged);
            }
        }

        let max_frames = get::<AudioValueRange>(
            io_device,
            kAudioDevicePropertyBufferFrameSizeRange,
            kAudioObjectPropertyScopeGlobal,
        )
        .map_or(DEFAULT_MAX_FRAMES, |r| r.mMaximum as usize)
        .clamp(1, MAX_FRAMES);
        let frames = get::<u32>(
            io_device,
            kAudioDevicePropertyBufferFrameSize,
            kAudioObjectPropertyScopeGlobal,
        )
        .map_or(512, |f| f as usize)
        .max(1);
        let capacity = ring::capacity_frames(rate, self.buffer, max_frames);
        // One mark per IO cycle for the whole ring, so small buffers keep their times.
        let marks = (capacity / frames + 16).max(ring::DEFAULT_MARKS);
        let (producer, consumer) = ring::channel(self.format, capacity, marks);
        let capture = Box::into_raw(Box::new(Capture {
            packets: Packets::new(producer, rate, Arc::clone(&self.last_end)),
            clock: Arc::clone(&self.clock),
            buffers,
            channels: self.format.channels.channels(),
        }));

        session.watch = Some(Box::into_raw(Box::new(Watch {
            status: Arc::clone(&status),
            device: watched,
            rate,
            process,
        })));
        session.listen(watched, kAudioDevicePropertyDeviceIsAlive)?;
        session.listen(watched, kAudioDevicePropertyNominalSampleRate)?;
        if process.is_some() {
            session.listen(SYSTEM, kAudioHardwarePropertyProcessObjectList)?;
        }

        let mut proc_id: AudioDeviceIOProcID = None;
        // SAFETY: a valid device, IO proc and out pointer; `capture` stays valid
        // until the session destroys the proc.
        let created = check(unsafe {
            AudioDeviceCreateIOProcID(
                io_device,
                Some(io_proc),
                capture.cast(),
                NonNull::from(&mut proc_id),
            )
        });
        if let Err(e) = created {
            // SAFETY: the box was never handed to the HAL.
            drop(unsafe { Box::from_raw(capture) });
            return Err(e);
        }
        session.io = Some((io_device, proc_id, capture, Owned::new()));
        // SAFETY: the proc was created on this device above.
        check(unsafe { AudioDeviceStart(io_device, proc_id) })?;
        session.started = true;
        Ok(Active {
            session,
            consumer,
            status,
        })
    }
}

impl AudioInputStream for CoreAudioStream {
    fn format(&self) -> AudioFormat {
        self.format
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
        let Some(active) = self.active.take() else {
            return;
        };
        let Active {
            session, consumer, ..
        } = active;
        // The IO proc stops before the ring's counters are closed.
        drop(session);
        self.totals = self.totals.plus(consumer.close());
    }

    fn read(&mut self, out: &mut [&mut [f32]]) -> Result<AudioRead, AudioIoError> {
        let channels = self.format.channels.channels();
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

impl Drop for CoreAudioStream {
    fn drop(&mut self) {
        self.stop();
    }
}
