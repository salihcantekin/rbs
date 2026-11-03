//! The Windows backend: device enumeration and the Media Foundation capture session.
//!
//! Threads: `enumerate`, `open`, `start` and `stop` run on a control thread;
//! `latest_frame` on the render thread; the source reader callback on Media
//! Foundation's work queue threads. The render thread only touches the lock-free ring
//! and an atomic flag. The producer state is behind a mutex that only the callback and
//! `stop` take.
//!
//! COM: the adapter keeps the process's implicit multithreaded apartment alive
//! (`CoIncrementMTAUsage`, like the WGC adapter), so threads that never initialised COM
//! (the engine's control threads) use Media Foundation objects in the MTA without
//! changing their own apartment. Media Foundation objects are free-threaded.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFActivate, IMFAttributes, IMFMediaBuffer, IMFMediaEvent, IMFMediaSource,
    IMFMediaType, IMFSample, IMFSourceReader, IMFSourceReaderCallback,
    IMFSourceReaderCallback_Impl, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_E_HW_MFT_FAILED_START_STREAMING,
    MF_E_NO_MORE_TYPES, MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED,
    MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_RATE_RANGE_MAX, MF_MT_FRAME_RATE_RANGE_MIN, MF_MT_FRAME_SIZE, MF_MT_SUBTYPE,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX, MF_READWRITE_DISABLE_CONVERTERS,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_ASYNC_CALLBACK,
    MF_SOURCE_READER_DISCONNECT_MEDIASOURCE_ON_SHUTDOWN, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_VERSION, MF2DBuffer_LockFlags_Read,
    MFCreateAttributes, MFCreateDeviceSource, MFCreateSourceReaderFromMediaSource,
    MFEnumDeviceSources, MFSTARTUP_NOSOCKET, MFSampleExtension_DeviceReferenceSystemTime,
    MFShutdown, MFStartup,
};
use windows::Win32::System::Com::{CoIncrementMTAUsage, CoTaskMemFree};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::core::{GUID, HRESULT, HSTRING, Interface, PCWSTR, PWSTR};
use windows_core::{Ref, implement};

use rbs_capture::camera::{self, NativeMode};
use rbs_capture::handoff::{FrameMeta, Grave, Graveyard};
use rbs_capture::{
    CaptureChange, CaptureError, CaptureFactory, CaptureRequest, CaptureSession, CaptureStats,
    CaptureTarget, ErrorDomain, TargetId, TargetKind, caps,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{ColorSpace, MemoryKind, PixelFormat, Size, VideoFormat};
use rbs_core::registry::{AdapterFactory, AdapterId, HARDWARE, ProbeError};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::{DriftEstimator, FrameRate};

use crate::{
    ADAPTER_ID, Consumer, FRAMES_IN_FLIGHT, PIXEL_FORMATS, Shared, SourceLayout, color_of,
    fill_slot, pixel_format_of_subtype,
};

/// Highest frame rate declared by the probe, which cannot ask devices (50 ms budget).
/// Real limits are in each target's modes.
const MAX_RATE: i64 = 240;

/// Longest wait in `start` for the first sample or error. Cameras can take a second or
/// more to wake up; past this `start` succeeds and later errors arrive as target loss.
const FIRST_SAMPLE_WAIT: Duration = Duration::from_secs(2);

/// Longest wait in `stop` for the reader to confirm its flush (a hang guard).
const FLUSH_WAIT: Duration = Duration::from_millis(500);

/// Largest distance between a sample's mapped time and its arrival before the device
/// time is treated as unusable.
const MAX_PTS_SKEW: Duration = Duration::from_secs(1);

const CAMERA_PREFIX: &str = "camera:";

/// System-memory rings have no handle values an importer could cache, so a stopped
/// ring only waits until no frame refers to it.
/// The grace covers the moment in `Lease::drop` between releasing the slot and
/// dropping the lease's `Arc<Shared>`, so the janitor never leaves the last reference
/// to a lease on the render thread.
static GRAVEYARD: Graveyard = Graveyard::new(
    "rbs-mf-janitor",
    Duration::from_millis(100),
    Duration::from_secs(1),
);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Moves a Media Foundation interface to another thread. Media Foundation objects are
/// free-threaded and the adapter's threads share the process's MTA.
struct Sendable<T>(T);

// SAFETY: see the type's doc.
unsafe impl<T> Send for Sendable<T> {}

/// Keeps the process's implicit multithreaded apartment alive (see the module doc).
fn ensure_mta() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| {
        // SAFETY: plain FFI call. The cookie is never released: the MTA stays alive
        // for the process lifetime, which is the intent.
        let _ = unsafe { CoIncrementMTAUsage() };
    });
}

fn backend(e: &windows::core::Error) -> CaptureError {
    hresult_error(e.code())
}

fn hresult_error(code: HRESULT) -> CaptureError {
    match code {
        E_ACCESSDENIED => CaptureError::PermissionDenied,
        MF_E_VIDEO_RECORDING_DEVICE_PREEMPTED | MF_E_HW_MFT_FAILED_START_STREAMING => {
            CaptureError::DeviceBusy
        }
        MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED => CaptureError::TargetLost,
        _ => CaptureError::Backend {
            domain: ErrorDomain::HResult,
            code: i64::from(code.0),
            recoverable: false,
        },
    }
}

/// One `MFStartup`, balanced by `MFShutdown` on drop. Held only while the adapter uses
/// Media Foundation: around enumeration and opening, and while a session is Active.
struct Runtime;

impl Runtime {
    fn start() -> Result<Self, CaptureError> {
        ensure_mta();
        // SAFETY: plain FFI call, balanced by `Drop`.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.map_err(|e| backend(&e))?;
        Ok(Self)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: balances the `MFStartup` of `start`.
        let _ = unsafe { MFShutdown() };
    }
}

fn attributes(capacity: u32) -> Result<IMFAttributes, CaptureError> {
    let mut attributes = None;
    // SAFETY: a valid out pointer.
    unsafe { MFCreateAttributes(&mut attributes, capacity) }.map_err(|e| backend(&e))?;
    attributes.ok_or(CaptureError::Backend {
        domain: ErrorDomain::Library,
        code: 1,
        recoverable: false,
    })
}

fn video_capture_attributes(capacity: u32) -> Result<IMFAttributes, CaptureError> {
    let attributes = attributes(capacity)?;
    // SAFETY: setting a GUID attribute on a fresh attribute store.
    unsafe {
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
    }
    .map_err(|e| backend(&e))?;
    Ok(attributes)
}

fn allocated_string(activate: &IMFActivate, key: &GUID) -> Option<String> {
    let mut value = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: valid out pointers; the string is freed with CoTaskMemFree below.
    unsafe { activate.GetAllocatedString(key, &mut value, &mut len) }.ok()?;
    // SAFETY: a NUL-terminated string allocated by the call above.
    let text = unsafe { value.to_string() }.ok();
    // SAFETY: frees the string allocated by GetAllocatedString.
    unsafe { CoTaskMemFree(Some(value.0 as *const _)) };
    text
}

/// A video capture device: its symbolic link (the stable id) and its name.
struct Device {
    link: String,
    name: String,
}

fn devices() -> Result<Vec<Device>, CaptureError> {
    let attributes = video_capture_attributes(1)?;
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: valid out pointers; the array is freed with CoTaskMemFree below.
    unsafe { MFEnumDeviceSources(&attributes, &mut list, &mut count) }.map_err(|e| backend(&e))?;
    let mut out = Vec::with_capacity(count as usize);
    if !list.is_null() {
        // SAFETY: the call returned an array of `count` activation objects.
        let entries = unsafe { std::slice::from_raw_parts_mut(list, count as usize) };
        for entry in entries.iter_mut() {
            // Taking releases each activation object when it drops.
            if let Some(activate) = entry.take() {
                let link = allocated_string(
                    &activate,
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                );
                let name = allocated_string(&activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME);
                if let Some(link) = link {
                    out.push(Device {
                        name: name.unwrap_or_else(|| link.clone()),
                        link,
                    });
                }
            }
        }
        // SAFETY: frees the array allocated by MFEnumDeviceSources; its entries were
        // released above.
        unsafe { CoTaskMemFree(Some(list as *const _)) };
    }
    Ok(out)
}

/// A media source for the device with this symbolic link, shut down on drop.
struct Source(IMFMediaSource);

impl Source {
    fn open(link: &str) -> Result<Self, CaptureError> {
        let attributes = video_capture_attributes(2)?;
        let link = HSTRING::from(link);
        // SAFETY: a NUL-terminated string that outlives the call.
        unsafe {
            attributes.SetString(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                PCWSTR(link.as_ptr()),
            )
        }
        .map_err(|e| backend(&e))?;
        // SAFETY: creates the device's media source; it does not stream yet.
        let source = unsafe { MFCreateDeviceSource(&attributes) }.map_err(|e| backend(&e))?;
        Ok(Self(source))
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        // SAFETY: releases the device; later calls on the source fail cleanly.
        let _ = unsafe { self.0.Shutdown() };
    }
}

fn first_video_stream() -> u32 {
    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32
}

fn packed(t: &IMFMediaType, key: &GUID) -> Option<(u32, u32)> {
    // SAFETY: reading a UINT64 attribute.
    let value = unsafe { t.GetUINT64(key) }.ok()?;
    Some(((value >> 32) as u32, value as u32))
}

fn rate_of(t: &IMFMediaType, key: &GUID) -> Option<FrameRate> {
    let (num, den) = packed(t, key)?;
    FrameRate::new(num, den)
}

/// What the adapter needs from one native media type.
#[derive(Clone, Copy)]
struct TypeInfo {
    subtype: u32,
    pixel_format: PixelFormat,
    width: u32,
    height: u32,
    min_rate: FrameRate,
    max_rate: FrameRate,
    color: ColorSpace,
    /// `MF_MT_DEFAULT_STRIDE`, or the packed row size.
    stride: i32,
}

fn type_info(t: &IMFMediaType) -> Option<TypeInfo> {
    // SAFETY: reading attributes of a media type.
    let subtype = unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.ok()?;
    let pixel_format = pixel_format_of_subtype(subtype.data1)?;
    let (width, height) = packed(t, &MF_MT_FRAME_SIZE)?;
    if width == 0 || height == 0 {
        return None;
    }
    let rate = rate_of(t, &MF_MT_FRAME_RATE);
    let min_rate = rate_of(t, &MF_MT_FRAME_RATE_RANGE_MIN).or(rate)?;
    let max_rate = rate_of(t, &MF_MT_FRAME_RATE_RANGE_MAX).or(rate)?;
    // SAFETY: reading optional UINT32 attributes.
    let (matrix, range, stride) = unsafe {
        (
            t.GetUINT32(&MF_MT_YUV_MATRIX).ok(),
            t.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE).ok(),
            t.GetUINT32(&MF_MT_DEFAULT_STRIDE).ok(),
        )
    };
    let packed_row = pixel_format.min_row_bytes(0, width)?;
    let stride = match stride {
        Some(s) if s != 0 => s as i32,
        _ => i32::try_from(packed_row).ok()?,
    };
    Some(TypeInfo {
        subtype: subtype.data1,
        pixel_format,
        width,
        height,
        min_rate,
        max_rate,
        color: color_of(pixel_format, matrix, range, height),
        stride,
    })
}

/// The native media types of the reader's first video stream, by index; types the
/// adapter cannot deliver are `None`.
fn native_types(reader: &IMFSourceReader) -> Vec<Option<(IMFMediaType, TypeInfo)>> {
    let mut out = Vec::new();
    for index in 0u32.. {
        // SAFETY: enumerates the stream's native types until there are no more.
        match unsafe { reader.GetNativeMediaType(first_video_stream(), index) } {
            Ok(t) => {
                let info = type_info(&t);
                out.push(info.map(|info| (t, info)));
            }
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
            Err(_) => break,
        }
    }
    out
}

fn modes_of(types: &[Option<(IMFMediaType, TypeInfo)>]) -> Vec<NativeMode> {
    types
        .iter()
        .enumerate()
        .filter_map(|(index, t)| {
            let (_, info) = t.as_ref()?;
            Some(NativeMode::new(
                info.width,
                info.height,
                info.pixel_format,
                info.min_rate,
                info.max_rate,
                index,
            ))
        })
        .collect()
}

/// The native types of a device, read through a short-lived synchronous reader.
fn device_types(source: &Source) -> Result<Vec<Option<(IMFMediaType, TypeInfo)>>, CaptureError> {
    // `Source` owns the source's shutdown: releasing the reader must not shut it down,
    // because `start` builds its own reader on the same source afterwards.
    let attributes = attributes(1)?;
    // SAFETY: setting an attribute on a fresh store; then a synchronous reader over
    // the source, from which nothing is read.
    let reader = unsafe {
        attributes
            .SetUINT32(&MF_SOURCE_READER_DISCONNECT_MEDIASOURCE_ON_SHUTDOWN, 1)
            .map_err(|e| backend(&e))?;
        MFCreateSourceReaderFromMediaSource(&source.0, &attributes)
    }
    .map_err(|e| backend(&e))?;
    Ok(native_types(&reader))
}

/// Factory of the Media Foundation camera adapter.
#[derive(Debug, Default)]
pub struct MfCaptureFactory;

impl MfCaptureFactory {
    /// Creates the factory. Does nothing until used.
    pub fn new() -> Self {
        Self
    }
}

impl AdapterFactory for MfCaptureFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "Media Foundation cameras"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        // Cheap (capture-port.md: within 50 ms): no device is asked and Media
        // Foundation is not started.
        let set =
            |items: &[&str]| CapabilityValue::Set(items.iter().map(|s| (*s).to_owned()).collect());
        let formats: Vec<&str> = PIXEL_FORMATS.iter().map(|pf| pf.as_str()).collect();
        Ok(CapabilitySet::new()
            .with(caps::TARGET_KINDS, set(&["camera"]))
            .with(caps::TARGET_ENUMERATE, set(&["camera"]))
            .with(caps::TARGET_PICKER, set(&[]))
            .with(caps::PIXEL_FORMATS, set(&formats))
            .with(caps::MEMORY, set(&[MemoryKind::System.as_str()]))
            .with(caps::CURSOR, CapabilityValue::Flag(false))
            .with(caps::FRAME_RATE_MAX, CapabilityValue::Int(MAX_RATE))
            .with(
                caps::FRAMES_IN_FLIGHT,
                CapabilityValue::Int(FRAMES_IN_FLIGHT as i64),
            )
            .with(caps::HDR, CapabilityValue::Flag(false))
            .with(HARDWARE, CapabilityValue::Flag(true)))
    }
}

impl CaptureFactory for MfCaptureFactory {
    fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
        if kind != TargetKind::Camera {
            return Err(CaptureError::Unsupported(
                "Media Foundation captures cameras only",
            ));
        }
        let _runtime = Runtime::start()?;
        let mut out = Vec::new();
        for device in devices()? {
            // A camera the privacy settings block, or one busy elsewhere, is still
            // listed, without modes.
            let types = Source::open(&device.link)
                .and_then(|source| device_types(&source))
                .unwrap_or_default();
            let modes = modes_of(&types);
            let mut target = CaptureTarget::new(
                TargetId::new(format!("{CAMERA_PREFIX}{}", device.link)),
                TargetKind::Camera,
                device.name,
            );
            if let Some(largest) = modes
                .iter()
                .max_by_key(|m| u64::from(m.width) * u64::from(m.height))
            {
                target = target.with_size(largest.width, largest.height);
            }
            out.push(target.with_modes(camera::camera_modes(&modes)));
        }
        Ok(out)
    }

    fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
        let link = request
            .target
            .as_str()
            .strip_prefix(CAMERA_PREFIX)
            .ok_or(CaptureError::TargetNotFound)?
            .to_owned();
        // Refuse what cannot be delivered before asking the OS.
        if !request.memory.contains(&MemoryKind::System)
            || !request
                .pixel_formats
                .iter()
                .any(|pf| PIXEL_FORMATS.contains(pf))
        {
            return Err(CaptureError::FormatUnsupported);
        }
        let _runtime = Runtime::start()?;
        let device = devices()?
            .into_iter()
            .find(|d| d.link == link)
            .ok_or(CaptureError::TargetNotFound)?;
        let source = Source::open(&device.link)?;
        let types = device_types(&source)?;
        let choice = camera::choose_mode(
            &modes_of(&types),
            &request.pixel_formats,
            request.size,
            request.frame_rate,
        )?;
        let (_, info) = types
            .get(choice.index)
            .and_then(Option::as_ref)
            .ok_or(CaptureError::FormatUnsupported)?;
        let format = VideoFormat::new(choice.size, choice.pixel_format, info.color);
        Ok(Box::new(MfSession {
            link,
            chosen: Chosen {
                info: *info,
                rate: choice.rate,
            },
            clock: Arc::clone(&request.clock),
            format,
            active: None,
            next_sequence: 0,
            last_timestamp: None,
            base: CaptureStats::default(),
        }))
    }
}

/// The native type a session runs with, found again at every `start`.
#[derive(Clone, Copy)]
struct Chosen {
    info: TypeInfo,
    rate: FrameRate,
}

impl Chosen {
    fn matches(&self, other: &TypeInfo) -> bool {
        let rate = self.rate;
        other.subtype == self.info.subtype
            && (other.width, other.height) == (self.info.width, self.info.height)
            && camera::rate_cmp(rate, other.min_rate) != std::cmp::Ordering::Less
            && camera::rate_cmp(rate, other.max_rate) != std::cmp::Ordering::Greater
    }
}

/// Nanoseconds on the performance counter, the clock of
/// `MFSampleExtension_DeviceReferenceSystemTime` (in 100 ns units).
fn qpc_nanos(frequency: i64) -> u64 {
    let mut counter = 0i64;
    // SAFETY: a valid out pointer.
    let _ = unsafe { QueryPerformanceCounter(&mut counter) };
    if frequency <= 0 || counter < 0 {
        return 0;
    }
    let nanos = i128::from(counter) * 1_000_000_000 / i128::from(frequency);
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

/// Producer state, touched by the reader callback and by `stop`.
struct Producer {
    shared: Arc<Shared>,
    /// The reader, to request the next sample. Taken by `stop`.
    reader: Option<Sendable<IMFSourceReader>>,
    pixel_format: PixelFormat,
    size: Size,
    stride: i32,
    lost: Arc<AtomicBool>,
    copied: Arc<AtomicU64>,
    sequence: u64,
    clock: Arc<dyn Clock>,
    drift: DriftEstimator,
    qpc_frequency: i64,
    last_timestamp: Option<Timestamp>,
    stopped: bool,
    /// Reports the first sample or error to `start`, once.
    first: Option<mpsc::SyncSender<Result<(), CaptureError>>>,
}

impl Producer {
    fn report_first(&mut self, result: Result<(), CaptureError>) {
        if let Some(first) = self.first.take() {
            let _ = first.try_send(result);
        }
    }

    fn drop_frame(&mut self) {
        self.shared.ring.count_drop();
        self.sequence += 1;
    }

    fn on_sample(&mut self, sample: &IMFSample) {
        let arrived = self.clock.now();
        let host = qpc_nanos(self.qpc_frequency);
        let shared = Arc::clone(&self.shared);
        let ring = &shared.ring;
        let Some(slot) = ring.claim() else {
            // The consumer holds every slot: drop rather than wait (invariant 9).
            self.drop_frame();
            return;
        };
        // SAFETY: the claim is held until publish or abandon below.
        let filled = unsafe { self.copy(sample, slot) };
        if filled.is_none() {
            ring.abandon(slot);
            self.drop_frame();
            return;
        }
        self.copied.fetch_add(1, Ordering::Relaxed);

        // Clock edge (ADR-0004): pair the performance counter with the master clock
        // now, then convert the device's capture time when it reports one.
        // SAFETY: reading an optional UINT64 attribute of the sample.
        let device_time = unsafe { sample.GetUINT64(&MFSampleExtension_DeviceReferenceSystemTime) }
            .ok()
            .and_then(|t| t.checked_mul(100));
        let mapped = match device_time {
            Some(source) => {
                let _ = self.drift.observe(host, arrived);
                let mapped = self
                    .drift
                    .mapping()
                    .map_or(arrived, |m| m.to_master(source));
                if mapped.as_nanos().abs_diff(arrived.as_nanos()) > MAX_PTS_SKEW.as_nanos() as u64 {
                    arrived
                } else {
                    mapped
                }
            }
            None => arrived,
        };
        let floor = self
            .last_timestamp
            .map_or(Timestamp::ZERO, |t| t + Duration::from_nanos(1));
        let timestamp = mapped.min(arrived).max(floor);
        self.last_timestamp = Some(timestamp);
        ring.publish(
            slot,
            FrameMeta::new(
                timestamp.as_nanos(),
                self.sequence,
                device_time.unwrap_or(0),
                self.size.width,
                self.size.height,
            ),
        );
        self.sequence += 1;
    }

    /// Copies the sample's picture into `slot`.
    ///
    /// # Safety
    ///
    /// The caller holds `slot`'s claim.
    unsafe fn copy(&self, sample: &IMFSample, slot: usize) -> Option<()> {
        // SAFETY: plain calls on a valid sample.
        let buffer: IMFMediaBuffer = unsafe {
            match sample.GetBufferCount().ok()? {
                1 => sample.GetBufferByIndex(0).ok()?,
                _ => sample.ConvertToContiguousBuffer().ok()?,
            }
        };
        if let Ok(two_d) = buffer.cast::<IMF2DBuffer2>() {
            let (mut scan0, mut pitch) = (std::ptr::null_mut(), 0i32);
            let (mut start, mut len) = (std::ptr::null_mut(), 0u32);
            // SAFETY: valid out pointers; unlocked below.
            unsafe {
                two_d.Lock2DSize(
                    MF2DBuffer_LockFlags_Read,
                    &mut scan0,
                    &mut pitch,
                    &mut start,
                    &mut len,
                )
            }
            .ok()?;
            let result = (|| {
                if start.is_null() || scan0 < start {
                    return None;
                }
                // SAFETY: the locked buffer is `len` bytes from `start`, readable until
                // `Unlock2D`.
                let bytes = unsafe { std::slice::from_raw_parts(start, len as usize) };
                let layout = SourceLayout {
                    scan0: scan0 as usize - start as usize,
                    pitch: pitch as isize,
                };
                // SAFETY: the caller holds the claim.
                unsafe {
                    fill_slot(
                        &self.shared,
                        slot,
                        self.pixel_format,
                        self.size,
                        bytes,
                        layout,
                    )
                }
            })();
            // SAFETY: balances the lock above.
            let _ = unsafe { two_d.Unlock2D() };
            return result;
        }
        let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
        // SAFETY: valid out pointers; unlocked below.
        unsafe { buffer.Lock(&mut data, None, Some(&mut len)) }.ok()?;
        let result = (|| {
            if data.is_null() {
                return None;
            }
            // SAFETY: the locked buffer is `len` bytes from `data`, readable until
            // `Unlock`.
            let bytes = unsafe { std::slice::from_raw_parts(data, len as usize) };
            // A contiguous buffer has the default stride; a negative one (bottom-up
            // RGB) stores the bottom row first.
            let pitch = self.stride as isize;
            let scan0 = if pitch < 0 {
                pitch.unsigned_abs() * (self.size.height as usize).saturating_sub(1)
            } else {
                0
            };
            // SAFETY: the caller holds the claim.
            unsafe {
                fill_slot(
                    &self.shared,
                    slot,
                    self.pixel_format,
                    self.size,
                    bytes,
                    SourceLayout { scan0, pitch },
                )
            }
        })();
        // SAFETY: balances the lock above.
        let _ = unsafe { buffer.Unlock() };
        result
    }
}

/// Signalled when the reader confirms a flush.
#[derive(Default)]
struct Flushed {
    done: Mutex<bool>,
    wake: Condvar,
    /// Reader callbacks running now, so `stop` can wait until none can still request
    /// a sample.
    running: Mutex<usize>,
    idle: Condvar,
}

/// Counts one running callback for as long as it lives.
struct Running<'a>(&'a Flushed);

impl<'a> Running<'a> {
    fn enter(state: &'a Flushed) -> Self {
        *lock(&state.running) += 1;
        Self(state)
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let mut running = lock(&self.0.running);
        *running = running.saturating_sub(1);
        if *running == 0 {
            self.0.idle.notify_all();
        }
    }
}

#[implement(IMFSourceReaderCallback)]
struct ReaderCallback {
    producer: Arc<Mutex<Producer>>,
    flushed: Arc<Flushed>,
}

impl IMFSourceReaderCallback_Impl for ReaderCallback_Impl {
    fn OnReadSample(
        &self,
        status: HRESULT,
        _stream: u32,
        flags: u32,
        _timestamp: i64,
        sample: Ref<IMFSample>,
    ) -> windows::core::Result<()> {
        // Held until the next sample is requested, so `stop` can wait for it.
        let _running = Running::enter(&self.flushed);
        let reader = {
            let mut producer = lock(&self.producer);
            if producer.stopped {
                return Ok(());
            }
            let failed =
                (flags as i32) & (MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_ENDOFSTREAM.0) != 0;
            if status.is_err() || failed {
                // The camera was unplugged, taken over or failed (invariant 7).
                producer.lost.store(true, Ordering::Release);
                let error = if status.is_err() {
                    hresult_error(status)
                } else {
                    CaptureError::TargetLost
                };
                producer.report_first(Err(error));
                return Ok(());
            }
            if let Some(sample) = sample.as_ref() {
                producer.on_sample(sample);
                producer.report_first(Ok(()));
            }
            // A clone only adds a reference; the call is made outside the lock.
            producer.reader.as_ref().map(|r| r.0.clone())
        };
        if let Some(reader) = reader {
            // SAFETY: an asynchronous request for the next sample (all out pointers
            // null, as the asynchronous mode requires).
            if unsafe { reader.ReadSample(first_video_stream(), 0, None, None, None, None) }
                .is_err()
            {
                lock(&self.producer).lost.store(true, Ordering::Release);
            }
        }
        Ok(())
    }

    fn OnFlush(&self, _stream: u32) -> windows::core::Result<()> {
        *lock(&self.flushed.done) = true;
        self.flushed.wake.notify_all();
        Ok(())
    }

    fn OnEvent(&self, _stream: u32, _event: Ref<IMFMediaEvent>) -> windows::core::Result<()> {
        Ok(())
    }
}

/// A stopped ring, kept until no frame refers to it so the last lease never frees its
/// buffers on the render thread.
struct RingGrave {
    shared: Arc<Shared>,
    bytes: u64,
}

impl Grave for RingGrave {
    fn is_idle(&self) -> bool {
        self.shared.ring.is_idle()
    }
    fn bytes(&self) -> u64 {
        self.bytes
    }
}

struct Active {
    reader: Sendable<IMFSourceReader>,
    source: Sendable<Source>,
    flushed: Arc<Flushed>,
    producer: Arc<Mutex<Producer>>,
    consumer: Consumer,
    shared: Arc<Shared>,
    lost: Arc<AtomicBool>,
    copied: Arc<AtomicU64>,
    _runtime: Runtime,
}

struct MfSession {
    link: String,
    chosen: Chosen,
    clock: Arc<dyn Clock>,
    format: VideoFormat,
    active: Option<Active>,
    next_sequence: u64,
    last_timestamp: Option<Timestamp>,
    /// Counters of earlier Active periods.
    base: CaptureStats,
}

impl MfSession {
    fn activate(&mut self) -> Result<Active, CaptureError> {
        let runtime = Runtime::start()?;
        if !devices()?.iter().any(|d| d.link == self.link) {
            return Err(CaptureError::TargetNotFound);
        }
        let source = Source::open(&self.link)?;
        // Find the chosen type among the device's types again (by format, size and a
        // range containing the rate), before the reader exists.
        let media_type = device_types(&source)?
            .into_iter()
            .flatten()
            .find(|(_, info)| self.chosen.matches(info))
            .map(|(t, _)| t)
            .ok_or(CaptureError::FormatUnsupported)?;
        let rate = self.chosen.rate.per_second();
        // SAFETY: setting the frame rate on a media type the adapter owns.
        unsafe {
            media_type.SetUINT64(
                &MF_MT_FRAME_RATE,
                (u64::from(rate.num()) << 32) | u64::from(rate.den()),
            )
        }
        .map_err(|e| backend(&e))?;

        let shared = Arc::new(Shared::new());
        let lost = Arc::new(AtomicBool::new(false));
        let copied = Arc::new(AtomicU64::new(0));
        let (first_tx, first_rx) = mpsc::sync_channel(1);
        let mut frequency = 0i64;
        // SAFETY: a valid out pointer.
        let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
        let producer = Arc::new(Mutex::new(Producer {
            shared: Arc::clone(&shared),
            reader: None,
            pixel_format: self.format.pixel_format,
            size: self.format.size,
            stride: self.chosen.info.stride,
            lost: Arc::clone(&lost),
            copied: Arc::clone(&copied),
            sequence: self.next_sequence,
            clock: Arc::clone(&self.clock),
            drift: DriftEstimator::new(Duration::from_millis(100)),
            qpc_frequency: frequency,
            last_timestamp: self.last_timestamp,
            stopped: false,
            first: Some(first_tx),
        }));
        let flushed = Arc::new(Flushed::default());
        let callback: IMFSourceReaderCallback = ReaderCallback {
            producer: Arc::clone(&producer),
            flushed: Arc::clone(&flushed),
        }
        .into();
        let attributes = attributes(3)?;
        // SAFETY: setting attributes on a fresh store; the reader keeps the callback.
        unsafe {
            attributes
                .SetUINT32(&MF_SOURCE_READER_DISCONNECT_MEDIASOURCE_ON_SHUTDOWN, 1)
                .map_err(|e| backend(&e))?;
            attributes
                .SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &callback)
                .map_err(|e| backend(&e))?;
            attributes
                .SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)
                .map_err(|e| backend(&e))?;
        }
        // SAFETY: an asynchronous reader over the source with the attributes above.
        let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source.0, &attributes) }
            .map_err(|e| backend(&e))?;
        // SAFETY: sets a native type of the stream (converters are disabled).
        unsafe { reader.SetCurrentMediaType(first_video_stream(), None, &media_type) }
            .map_err(|e| backend(&e))?;
        lock(&producer).reader = Some(Sendable(reader.clone()));
        let fail = |producer: &Arc<Mutex<Producer>>, error: CaptureError| {
            let mut p = lock(producer);
            p.stopped = true;
            p.reader = None;
            error
        };
        // SAFETY: the first asynchronous sample request (all out pointers null).
        let requested =
            unsafe { reader.ReadSample(first_video_stream(), 0, None, None, None, None) };
        if let Err(e) = requested {
            return Err(fail(&producer, backend(&e)));
        }
        // Report errors of the stream's start (privacy settings, a camera busy
        // elsewhere) from `start` itself. A slow camera passes after the wait; its
        // later errors arrive as target loss.
        if let Ok(Err(error)) = first_rx.recv_timeout(FIRST_SAMPLE_WAIT) {
            // Gone before any frame: not found rather than lost.
            let error = match error {
                CaptureError::TargetLost => CaptureError::TargetNotFound,
                other => other,
            };
            return Err(fail(&producer, error));
        }
        let consumer = Consumer::new(Arc::clone(&shared), self.format);
        Ok(Active {
            reader: Sendable(reader),
            source: Sendable(source),
            flushed,
            producer,
            consumer,
            shared,
            lost,
            copied,
            _runtime: runtime,
        })
    }
}

impl CaptureSession for MfSession {
    fn format(&self) -> VideoFormat {
        self.active
            .as_ref()
            .map_or(self.format, |a| a.consumer.format())
    }

    fn start(&mut self) -> Result<(), CaptureError> {
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
        {
            // Waits for a callback that is still running, then makes later ones
            // no-ops and stops them from requesting more samples.
            let mut producer = lock(&active.producer);
            producer.stopped = true;
            producer.reader = None;
            self.next_sequence = producer.sequence;
            self.last_timestamp = producer.last_timestamp;
        }
        // A callback that read the reader before `stopped` was set may still be about to
        // request a sample: wait until no callback runs (bounded, a hang guard), so the
        // flush below comes after its request.
        {
            let running = lock(&active.flushed.running);
            let _ = active
                .flushed
                .idle
                .wait_timeout_while(running, FLUSH_WAIT, |running| *running > 0);
        }
        // SAFETY: an asynchronous flush; `OnFlush` confirms it.
        if unsafe { active.reader.0.Flush(MF_SOURCE_READER_ALL_STREAMS.0 as u32) }.is_ok() {
            let done = lock(&active.flushed.done);
            let _ = active
                .flushed
                .wake
                .wait_timeout_while(done, FLUSH_WAIT, |done| !*done);
        }
        let ring = &active.shared.ring;
        let waiting = ring.clear();
        self.base.delivered += active.consumer.delivered();
        self.base.dropped += ring.dropped() + u64::from(waiting);
        self.base.cpu_copied += active.copied.load(Ordering::Relaxed);
        self.format = active.consumer.format();
        let Active {
            reader,
            source,
            shared,
            consumer,
            producer,
            _runtime,
            ..
        } = active;
        drop(reader);
        // Shuts the device down (the camera light goes off).
        drop(source);
        drop(consumer);
        drop(producer);
        // A lease may hold the ring's `Arc` for a moment after releasing its slot, so the
        // count of `Arc`s decides too: only this one left means nobody can free it later.
        if !shared.ring.is_idle() || Arc::strong_count(&shared) > 1 {
            // SAFETY: the reader is flushed and the producer stopped, so nothing
            // writes the slots any more.
            let bytes = unsafe { shared.held_bytes() };
            GRAVEYARD.bury(Box::new(RingGrave { shared, bytes }));
        }
        drop(_runtime);
    }

    fn latest_frame(&mut self) -> Result<Option<rbs_core::media::VideoFrame>, CaptureError> {
        let Some(active) = self.active.as_mut() else {
            return Ok(None);
        };
        if active.lost.load(Ordering::Acquire) {
            return Err(CaptureError::TargetLost);
        }
        Ok(active.consumer.next())
    }

    fn stats(&self) -> CaptureStats {
        let mut stats = self.base;
        if let Some(active) = self.active.as_ref() {
            stats.delivered += active.consumer.delivered();
            stats.dropped += active.shared.ring.dropped();
            stats.cpu_copied += active.copied.load(Ordering::Relaxed);
        }
        stats
    }

    fn reconfigure(&mut self, change: CaptureChange) -> Result<(), CaptureError> {
        match change {
            CaptureChange::Cursor(_) => Err(CaptureError::Unsupported("cameras have no cursor")),
            _ => Err(CaptureError::Unsupported("change not supported")),
        }
    }
}

impl Drop for MfSession {
    fn drop(&mut self) {
        self.stop();
    }
}
