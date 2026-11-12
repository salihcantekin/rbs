//! The macOS backend. Overview in the crate documentation.
//!
//! Threads: `enumerate`, `open_input`, `start` and `stop` run on a control thread; the
//! IO proc runs on the HAL's real-time IO thread and only touches its own `Capture`
//! (stamping and the ring's producer); property listeners run on a private serial
//! dispatch queue per Active stream, read HAL properties (not real-time) and store an
//! atomic status; `read` runs on the audio thread and only touches the ring's
//! consumer and that status.

use std::ffi::{CStr, c_void};
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use objc2_core_audio::{
    AudioConvertHostTimeToNanos, AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID,
    AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop, AudioGetCurrentHostTime,
    AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice,
    AudioObjectAddPropertyListenerBlock, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectPropertyScope, AudioObjectPropertySelector, AudioObjectRemovePropertyListenerBlock,
    CATapDescription, kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceMainSubDeviceKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceSubDeviceListKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyBufferFrameSize,
    kAudioDevicePropertyBufferFrameSizeRange, kAudioDevicePropertyDeviceIsAlive,
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyLatency,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertySafetyOffset,
    kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyStreams,
    kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDevices, kAudioHardwarePropertyProcessObjectList,
    kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioHardwarePropertyTranslateUIDToDevice,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
    kAudioProcessPropertyBundleID, kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID,
    kAudioStreamPropertyVirtualFormat, kAudioSubDeviceUIDKey, kAudioSubTapDriftCompensationKey,
    kAudioSubTapUIDKey,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, AudioTimeStampFlags,
    AudioValueRange, kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat, kAudioFormatLinearPCM,
};
use objc2_core_foundation::{CFDictionary, CFRetained, CFString};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSProcessInfo, NSString};

use rbs_audio_io::output::{self as out_ring, OutputConsumer, OutputProducer, OutputStatsReader};
use rbs_audio_io::ring::{self, RingConsumer};
use rbs_audio_io::{
    AudioBackend, AudioEndpoint, AudioInputStream, AudioIoError, AudioOutput, AudioRead,
    EndpointKind, ErrorDomain, InputRequest, InputStats, OutputRequest, OutputStats, Permission,
    caps,
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

/// Whether every output stream of `device` takes 32-bit native float from IO procs,
/// the only sample type the render proc writes.
fn output_is_float(device: AudioObjectID) -> bool {
    get_ids(
        device,
        kAudioDevicePropertyStreams,
        kAudioObjectPropertyScopeOutput,
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

/// Latency reported for outputs before a device is chosen (a typical built-in
/// output: buffer, safety offset and device latency).
const TYPICAL_OUTPUT_LATENCY_NS: u64 = 20_000_000;

/// The device's output latency: its reported latency, safety offset and IO buffer.
fn output_latency_ns(device: AudioObjectID, rate: SampleRate) -> u64 {
    let frames = |selector, scope| u64::from(get::<u32>(device, selector, scope).unwrap_or(0));
    let total = frames(kAudioDevicePropertyLatency, kAudioObjectPropertyScopeOutput)
        + frames(
            kAudioDevicePropertySafetyOffset,
            kAudioObjectPropertyScopeOutput,
        )
        + frames(
            kAudioDevicePropertyBufferFrameSize,
            kAudioObjectPropertyScopeGlobal,
        );
    total * 1_000_000_000 / u64::from(rate.hz().max(1))
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
                    .with_device_key(uid.clone())
                    .with_default(Some(device) == default),
                )
            })
            .collect()
    }

    /// Playback devices for monitoring (ADR-0097): devices with output channels that
    /// take 32-bit float, played in stereo at their nominal rate.
    fn playback_endpoints() -> Vec<AudioEndpoint> {
        let default = default_device(kAudioHardwarePropertyDefaultOutputDevice);
        devices(kAudioObjectPropertyScopeOutput)
            .into_iter()
            .filter(|(device, _, _)| output_is_float(*device))
            .filter_map(|(device, uid, _)| {
                let format = Self::stereo(nominal_rate(device)?);
                let name = get_string(device, kAudioObjectPropertyName).unwrap_or(uid.clone());
                Some(
                    AudioEndpoint::new(
                        ids::endpoint(EndpointKind::Output, &uid),
                        EndpointKind::Output,
                        name,
                        format,
                    )
                    .with_device_key(uid.clone())
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
            .with(
                caps::LOOPBACK_EXCLUDE_SELF,
                CapabilityValue::Flag(taps_supported()),
            )
            .with(caps::OUTPUT, CapabilityValue::Flag(true))
            .with(caps::OUTPUT_ENUMERATE, CapabilityValue::Flag(true))
            .with(
                caps::OUTPUT_LATENCY_NS,
                CapabilityValue::Int(TYPICAL_OUTPUT_LATENCY_NS as i64),
            )
            .with(HARDWARE, CapabilityValue::Flag(true)))
    }
}

impl AudioBackend for CoreAudioBackend {
    fn enumerate(&self, kind: EndpointKind) -> Result<Vec<AudioEndpoint>, AudioIoError> {
        if kind == EndpointKind::Output {
            return Ok(Self::playback_endpoints());
        }
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
                (
                    Target::Output {
                        uid: native.to_owned(),
                        exclude_self: request.exclude_own_process,
                    },
                    Self::stereo(rate),
                )
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

    fn open_output(&self, request: &OutputRequest) -> Result<Box<dyn AudioOutput>, AudioIoError> {
        let Some((EndpointKind::Output, uid)) = ids::parse(&request.endpoint) else {
            return Err(AudioIoError::EndpointNotFound);
        };
        let device = device_by_uid(uid)
            .filter(|d| channels(*d, kAudioObjectPropertyScopeOutput) > 0)
            .ok_or(AudioIoError::EndpointNotFound)?;
        if !output_is_float(device) {
            return Err(AudioIoError::FormatUnsupported);
        }
        let rate = nominal_rate(device).ok_or(AudioIoError::EndpointNotFound)?;
        let (producer, consumer) = out_ring::channel(2, request.clamped_ring_frames());
        Ok(Box::new(CoreAudioOutput {
            uid: uid.to_owned(),
            format: Self::stereo(rate),
            latency_ns: output_latency_ns(device, rate),
            reader: consumer.reader(),
            producer: Some(producer),
            consumer: Some(consumer),
            status: Arc::new(Status::default()),
            run: None,
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
            // A panic must not unwind into AVFoundation. No answer: the wait below
            // reports whatever permission the system has now.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = tx.try_send(granted.as_bool());
            }));
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
    Output {
        uid: String,
        /// Leave this process's own sound out of the tap (ADR-0096).
        exclude_self: bool,
    },
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
    /// The one at this index (the tap's, after the output device's own input
    /// buffers in the aggregate).
    Tap(usize),
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
    fn cycle(&mut self, _now: &AudioTimeStamp, list: &AudioBufferList, time: &AudioTimeStamp) {
        // The host clock and the master clock read back to back (the cycle's own
        // `_now` is when the HAL woke, earlier by the wake-up latency).
        // SAFETY: plain host-time calls, no preconditions.
        let os_now = unsafe { AudioConvertHostTimeToNanos(AudioGetCurrentHostTime()) };
        self.packets.observe(os_now, self.clock.now());
        let count = list.mNumberBuffers as usize;
        let first = match self.buffers {
            Buffers::All => 0,
            Buffers::Tap(index) => index,
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
    let capture = unsafe { &mut *client.cast::<Capture>() };
    // SAFETY: as above.
    let (now, input, input_time) = unsafe { (now.as_ref(), input.as_ref(), input_time.as_ref()) };
    // A panic must not unwind into the HAL's frames. `cycle` has no panicking paths
    // by design; if one slipped in, the cycle is skipped.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        capture.cycle(now, input, input_time);
    }));
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

/// Handles property changes on the session's private dispatch queue. Not real-time
/// (it reads properties and may allocate); it only stores an atomic status that
/// `read` sees.
#[allow(non_upper_case_globals)]
fn on_property(watch: &Watch, addresses: &[AudioObjectPropertyAddress]) {
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
}

type ListenerBlock = RcBlock<dyn Fn(u32, NonNull<AudioObjectPropertyAddress>)>;

/// Test-only: the property-listener block panics once, inside its `catch_unwind`.
#[cfg(test)]
static LISTENER_PANIC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Everything an Active stream holds in the HAL, released in reverse on drop.
#[derive(Default)]
struct Session {
    tap: Option<(AudioObjectID, Owned)>,
    aggregate: Option<(AudioObjectID, Owned)>,
    /// The IO device, its proc and the proc's state.
    io: Option<(AudioObjectID, AudioDeviceIOProcID, *mut Capture, Owned)>,
    started: bool,
    /// Property listeners run as this block on this private serial queue, so `drop`
    /// can wait for a listener that is running before releasing what it uses.
    listener: Option<(DispatchRetained<DispatchQueue>, ListenerBlock)>,
    listeners: Vec<(AudioObjectID, AudioObjectPropertyAddress)>,
}

// SAFETY: the raw pointer is a box the session owns and frees itself, the queue and
// block are only used by the session's own calls, and the HAL objects are
// process-wide ids usable from any thread.
unsafe impl Send for Session {}

impl Session {
    fn watch(&mut self, watch: Watch) {
        let queue = DispatchQueue::new("org.rbs.audio-io.coreaudio.listener", None);
        let block = RcBlock::new(
            move |count: u32, addresses: NonNull<AudioObjectPropertyAddress>| {
                // A panic must not unwind into the HAL. This queue is not the IO thread.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    #[cfg(test)]
                    if LISTENER_PANIC.swap(false, Ordering::Relaxed) {
                        panic!("property listener");
                    }
                    // SAFETY: the HAL passes `count` addresses, valid for this call.
                    let addresses =
                        unsafe { std::slice::from_raw_parts(addresses.as_ptr(), count as usize) };
                    on_property(&watch, addresses);
                }));
            },
        );
        self.listener = Some((queue, block));
    }

    fn listen(
        &mut self,
        object: AudioObjectID,
        selector: AudioObjectPropertySelector,
    ) -> Result<(), AudioIoError> {
        let Some((queue, block)) = &self.listener else {
            return Ok(());
        };
        let address = address(selector, kAudioObjectPropertyScopeGlobal);
        // SAFETY: a valid address, queue and block; the HAL retains the block until
        // the listener is removed in `drop`.
        check(unsafe {
            AudioObjectAddPropertyListenerBlock(
                object,
                NonNull::from(&address),
                Some(queue),
                RcBlock::as_ptr(block),
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
        // the last cycle), so no HAL thread uses the capture box when it is freed.
        unsafe {
            if let Some((device, proc_id, capture, _owned)) = self.io.take() {
                if self.started {
                    let _ = AudioDeviceStop(device, proc_id);
                }
                let _ = AudioDeviceDestroyIOProcID(device, proc_id);
                drop(Box::from_raw(capture));
            }
            if let Some((queue, block)) = self.listener.take() {
                for (object, address) in self.listeners.drain(..) {
                    let _ = AudioObjectRemovePropertyListenerBlock(
                        object,
                        NonNull::from(&address),
                        Some(&queue),
                        RcBlock::as_ptr(&block),
                    );
                }
                // A listener already running finishes before this returns (the queue
                // is serial), so nothing uses the block's state after it is dropped.
                queue.exec_sync(|| {});
                drop(block);
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
/// one process (stereo). `exclude` is a process object left out of the device tap.
/// Returns the tap and its UID.
fn create_tap(
    output: Option<&str>,
    process: Option<AudioObjectID>,
    exclude: Option<AudioObjectID>,
) -> Result<(AudioObjectID, String), AudioIoError> {
    let api = tap_api().ok_or(AudioIoError::Unsupported("process taps need macOS 14.4"))?;
    let excluded = match exclude {
        Some(object) => NSArray::from_retained_slice(&[NSNumber::new_u32(object)]),
        None => NSArray::new(),
    };
    // SAFETY: CATapDescription initialisers and setters on a fresh object; the class
    // exists (macOS 14.2+, checked by `tap_api`).
    let description = unsafe {
        let description = match (output, process) {
            (Some(uid), _) => {
                let d = CATapDescription::initExcludingProcesses_andDeviceUID_withStream(
                    CATapDescription::alloc(),
                    &excluded,
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
    // Tap auto-start off: with it on, the HAL does not run the aggregate (its IO proc
    // is never called) until the tapped audio plays, so a silent machine delivered
    // nothing (macOS 26, #109). Off, the aggregate runs from `AudioDeviceStart` and
    // delivers silence too, without starting the output device itself.
    let no = NSNumber::new_bool(false);
    let values: [&AnyObject; 7] = [&name, &uid, &output, &yes, &no, &sub_devices, &taps];
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
                if !input_is_float(device) {
                    return Err(AudioIoError::FormatChanged);
                }
                // The permission may have been revoked since `open_input`.
                match microphone() {
                    Permission::Granted => {}
                    Permission::NotDetermined => {
                        return Err(AudioIoError::PermissionNotDetermined);
                    }
                    _ => return Err(AudioIoError::PermissionDenied),
                }
                (device, device, None, Buffers::All)
            }
            Target::Output { uid, exclude_self } => {
                let device = device_by_uid(uid).ok_or(AudioIoError::EndpointLost)?;
                if nominal_rate(device) != Some(rate) {
                    return Err(AudioIoError::FormatChanged);
                }
                // A process that has not used audio yet has no audio object and so
                // nothing to exclude; its sound would then be captured once it starts.
                let own = exclude_self
                    .then(|| process_object(std::process::id() as i32))
                    .flatten();
                let (tap, tap_uid) = create_tap(Some(uid), None, own)?;
                session.tap = Some((tap, Owned::new()));
                let aggregate = create_aggregate(uid, &tap_uid)?;
                session.aggregate = Some((aggregate, Owned::new()));
                let index = buffer_channels(device, kAudioObjectPropertyScopeInput).len();
                (aggregate, device, None, Buffers::Tap(index))
            }
            Target::Application(pid) => {
                let process = process_object(*pid).ok_or(AudioIoError::EndpointLost)?;
                let output = default_device(kAudioHardwarePropertyDefaultOutputDevice)
                    .ok_or(AudioIoError::EndpointLost)?;
                let output_uid = uid(output).ok_or(AudioIoError::EndpointLost)?;
                if nominal_rate(output) != Some(rate) {
                    return Err(AudioIoError::FormatChanged);
                }
                let (tap, tap_uid) = create_tap(None, Some(process), None)?;
                session.tap = Some((tap, Owned::new()));
                let aggregate = create_aggregate(&output_uid, &tap_uid)?;
                session.aggregate = Some((aggregate, Owned::new()));
                let index = buffer_channels(output, kAudioObjectPropertyScopeInput).len();
                (aggregate, output, Some(process), Buffers::Tap(index))
            }
        };
        if let Buffers::Tap(index) = buffers {
            // The aggregate's inputs are the output device's own, then the tap's one
            // stereo buffer.
            let inputs = buffer_channels(io_device, kAudioObjectPropertyScopeInput);
            if nominal_rate(io_device) != Some(rate)
                || inputs.len() != index + 1
                || inputs.get(index) != Some(&2)
            {
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
        session.watch(Watch {
            status: Arc::clone(&status),
            device: watched,
            rate,
            process,
        });
        session.listen(watched, kAudioDevicePropertyDeviceIsAlive)?;
        session.listen(watched, kAudioDevicePropertyNominalSampleRate)?;
        if process.is_some() {
            session.listen(SYSTEM, kAudioHardwarePropertyProcessObjectList)?;
        }

        // Created last, so no early return above can leak it.
        let capture = Box::into_raw(Box::new(Capture {
            packets: Packets::new(producer, rate, Arc::clone(&self.last_end)),
            clock: Arc::clone(&self.clock),
            buffers,
            channels: self.format.channels.channels(),
        }));
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
// ---------------------------------------------------------------------------------
// Output (monitoring, ADR-0097)

/// The render proc's state. Lives in a `Box` handed to the HAL as client data; touched
/// only by the render proc between `AudioDeviceStart` and `AudioDeviceStop`.
struct Render {
    consumer: OutputConsumer,
    /// Stereo frames pulled from the ring for one cycle, allocated at start.
    scratch: Vec<f32>,
}

impl Render {
    /// One render cycle: stereo from the ring into the device's first two channels,
    /// silence everywhere else. **Real-time**: no allocation, locks, I/O or logging.
    fn cycle(&mut self, list: &mut AudioBufferList) {
        let count = list.mNumberBuffers as usize;
        // SAFETY: the HAL passes `mNumberBuffers` buffers in the list.
        let buffers = unsafe { std::slice::from_raw_parts_mut(list.mBuffers.as_mut_ptr(), count) };
        let Some(first) = buffers.first() else {
            return;
        };
        let per_frame = first.mNumberChannels.max(1) as usize;
        let frames = (first.mDataByteSize as usize / 4 / per_frame).min(self.scratch.len() / 2);
        let pulled = &mut self.scratch[..frames * 2];
        self.consumer.pull(pulled);
        // `channel` counts the device channels across buffers, so left and right land
        // on the first two whatever the buffer layout (one interleaved buffer or one
        // buffer per channel).
        let mut channel = 0_usize;
        for buffer in buffers.iter_mut() {
            let n = buffer.mNumberChannels as usize;
            let len = buffer.mDataByteSize as usize / 4;
            if buffer.mData.is_null() || n == 0 {
                channel += n;
                continue;
            }
            // SAFETY: the HAL's buffer holds `mDataByteSize` bytes of native f32,
            // writable for this cycle.
            let data = unsafe { std::slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), len) };
            for (f, frame) in data.chunks_exact_mut(n).enumerate() {
                for (c, sample) in frame.iter_mut().enumerate() {
                    *sample = match channel + c {
                        0 | 1 if f < frames => pulled[f * 2 + channel + c],
                        _ => 0.0,
                    };
                }
            }
            channel += n;
        }
    }
}

unsafe extern "C-unwind" fn render_proc(
    _device: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    _input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is the `Render` registered with this IO proc, used by nothing
    // else until the proc is stopped; the HAL's output list is valid and writable for
    // this call.
    let (render, list) = unsafe { (&mut *client.cast::<Render>(), &mut *output.as_ptr()) };
    // A panic must not unwind into the HAL's frames; `cycle` has no panicking path by
    // design, and if one slipped in the cycle stays silent.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render.cycle(list)));
    0
}

/// A started output: the render proc and the listeners that watch the device.
struct Running {
    device: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    render: *mut Render,
    /// Listeners for device loss and rate changes (only its listener fields are used).
    session: Session,
    _owned: Owned,
}

// SAFETY: the raw pointer is a box this struct owns and frees itself in `stop`; the
// HAL ids are process-wide and usable from any thread.
unsafe impl Send for Running {}

struct CoreAudioOutput {
    uid: String,
    format: AudioFormat,
    latency_ns: u64,
    reader: OutputStatsReader,
    producer: Option<OutputProducer>,
    /// Held while stopped; moved into the render proc while started.
    consumer: Option<OutputConsumer>,
    status: Arc<Status>,
    run: Option<Running>,
}

impl CoreAudioOutput {
    fn activate(&mut self) -> Result<(), AudioIoError> {
        let device = device_by_uid(&self.uid).ok_or(AudioIoError::EndpointLost)?;
        if nominal_rate(device) != Some(self.format.sample_rate) {
            return Err(AudioIoError::FormatChanged);
        }
        let consumer = self
            .consumer
            .take()
            .ok_or(AudioIoError::Unsupported("output in use"))?;
        let mut session = Session::default();
        session.watch(Watch {
            status: Arc::clone(&self.status),
            device,
            rate: self.format.sample_rate,
            process: None,
        });
        let render = Box::into_raw(Box::new(Render {
            consumer,
            scratch: vec![0.0; MAX_FRAMES * 2],
        }));
        let give_back = |render: *mut Render| {
            // SAFETY: the box was never handed to a running proc.
            unsafe { Box::from_raw(render) }.consumer
        };
        let listened = session
            .listen(device, kAudioDevicePropertyDeviceIsAlive)
            .and_then(|()| session.listen(device, kAudioDevicePropertyNominalSampleRate));
        if let Err(e) = listened {
            self.consumer = Some(give_back(render));
            return Err(e);
        }
        let mut proc_id: AudioDeviceIOProcID = None;
        // SAFETY: a valid device, IO proc and out pointer; `render` stays valid until
        // `stop` destroys the proc.
        let created = check(unsafe {
            AudioDeviceCreateIOProcID(
                device,
                Some(render_proc),
                render.cast(),
                NonNull::from(&mut proc_id),
            )
        });
        if let Err(e) = created {
            self.consumer = Some(give_back(render));
            return Err(e);
        }
        // SAFETY: the proc was created on this device above.
        if let Err(e) = check(unsafe { AudioDeviceStart(device, proc_id) }) {
            // SAFETY: created above and never started.
            let _ = unsafe { AudioDeviceDestroyIOProcID(device, proc_id) };
            self.consumer = Some(give_back(render));
            return Err(e);
        }
        self.run = Some(Running {
            device,
            proc_id,
            render,
            session,
            _owned: Owned::new(),
        });
        Ok(())
    }
}

impl AudioOutput for CoreAudioOutput {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn producer(&mut self) -> Option<OutputProducer> {
        self.producer.take()
    }

    fn start(&mut self) -> Result<(), AudioIoError> {
        if self.run.is_some() {
            return Ok(());
        }
        self.activate()
    }

    fn stop(&mut self) {
        let Some(run) = self.run.take() else {
            return;
        };
        // SAFETY: stopping from a non-IO thread returns after the last cycle, so no
        // HAL thread uses the render box when it is freed; each id is released once.
        let render = unsafe {
            let _ = AudioDeviceStop(run.device, run.proc_id);
            let _ = AudioDeviceDestroyIOProcID(run.device, run.proc_id);
            Box::from_raw(run.render)
        };
        self.consumer = Some(render.consumer);
        drop(run.session);
    }

    fn latency_ns(&self) -> u64 {
        self.latency_ns
    }

    fn stats(&self) -> OutputStats {
        self.reader.stats()
    }

    fn status(&self) -> Result<(), AudioIoError> {
        match self.status.error() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for CoreAudioOutput {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_listener_contains_a_panic_without_a_device() {
        let mut session = Session::default();
        session.watch(Watch {
            status: Arc::new(Status::default()),
            device: 0,
            rate: SampleRate::HZ_48000,
            process: None,
        });
        let Some(listener) = session.listener.as_ref() else {
            panic!("listener block was not installed");
        };
        let block = &listener.1;
        LISTENER_PANIC.store(true, Ordering::Relaxed);
        // Count 0: the slice is empty, so the HAL is not queried.
        block.call((0, NonNull::dangling()));
        assert!(!LISTENER_PANIC.load(Ordering::Relaxed));
        block.call((0, NonNull::dangling()));
    }
}
