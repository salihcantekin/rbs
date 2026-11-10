//! The macOS backend: device enumeration, camera permission and the AVFoundation
//! capture session.
//!
//! Threads: `enumerate`, `open`, `start` and `stop` run on a control thread;
//! `latest_frame` on the render thread; the sample buffer delegate on the session's
//! serial dispatch queue; notification blocks on whatever thread AVFoundation posts
//! them. The render thread only touches the lock-free ring and an atomic flag. The
//! producer state is behind a mutex that only the delegate and `stop` take.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceFormat,
    AVCaptureDeviceInput, AVCaptureDeviceWasDisconnectedNotification, AVCaptureOutput,
    AVCaptureSession, AVCaptureSessionRuntimeErrorNotification, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVError, AVMediaTypeAudio, AVMediaTypeVideo,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMClock, CMSampleBuffer, CMSyncConvertTime, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionGetDimensions,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetIOSurface,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, kCVPixelBufferPixelFormatTypeKey,
};
use objc2_foundation::{
    NSDictionary, NSError, NSNotification, NSNotificationCenter, NSNumber, NSString,
};
use objc2_io_surface::IOSurfaceRef;

use rbs_capture::camera::{self, ModeChoice, NativeMode};
use rbs_capture::handoff::{FrameMeta, Grave, Graveyard, Ring};
use rbs_capture::{
    CaptureChange, CaptureError, CaptureFactory, CaptureRequest, CaptureSession, CaptureStats,
    CaptureTarget, ErrorDomain, GpuDeviceId, Permission, RelatedDevice, SyncKind, TargetId,
    TargetKind, caps,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{ColorSpace, LeaseOwner, MemoryKind, PixelFormat, Size, VideoFormat};
use rbs_core::registry::{AdapterFactory, AdapterId, HARDWARE, ProbeError};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::{DriftEstimator, FrameRate};

use crate::{ADAPTER_ID, Consumer, FRAMES_IN_FLIGHT, RING_SLOTS};

/// Highest frame rate declared by the probe, which cannot ask devices (50 ms budget).
/// Real limits are in each target's modes.
const MAX_RATE: i64 = 240;

/// How long a handed-out surface stays retained after its last use (capture-port.md,
/// native surface rules: at least 10 s).
const HANDLE_GRACE: Duration = Duration::from_secs(10);

/// How often the graveyard's janitor checks stopped sessions.
const JANITOR_PERIOD: Duration = Duration::from_secs(1);

/// Replaced surfaces kept by a running producer before older ones go to the graveyard.
/// AVFoundation's buffer pool holds a handful of surfaces per format, so this only
/// fills up when the pool is rebuilt many times.
const SEEN_MAX: usize = 64;

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` ('420v'): NV12, limited range.
const PIXEL_FORMAT_420V: u32 = 0x3432_3076;
/// `kCVPixelFormatType_32BGRA` ('BGRA').
const PIXEL_FORMAT_BGRA: u32 = 0x4247_5241;

const CAMERA_PREFIX: &str = "camera:";

/// Largest distance between a sample's mapped time and its arrival before the sample
/// time is treated as being on another clock.
const MAX_PTS_SKEW: Duration = Duration::from_secs(1);

static GRAVEYARD: Graveyard = Graveyard::new("rbs-avf-janitor", HANDLE_GRACE, JANITOR_PERIOD);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Moves an Objective-C or CoreFoundation object to another thread.
///
/// The wrapped objects are AVFoundation capture objects, which Apple documents as
/// usable from any thread once configured (the adapter configures and runs them on one
/// control thread at a time), or `CVPixelBuffer` and `IOSurface` objects, whose retain
/// and release are atomic and which the adapter never mutates.
struct Sendable<T>(T);

// SAFETY: see the type's doc: every wrapped type is thread-safe for the way the adapter
// uses it.
unsafe impl<T> Send for Sendable<T> {}

/// The pixel formats this adapter delivers, with their CoreVideo codes and colour.
const DELIVERED: [(PixelFormat, u32, ColorSpace); 2] = [
    (
        PixelFormat::Nv12,
        PIXEL_FORMAT_420V,
        ColorSpace::BT709_LIMITED,
    ),
    (PixelFormat::Bgra8, PIXEL_FORMAT_BGRA, ColorSpace::SRGB),
];

fn delivered(pf: PixelFormat) -> Option<(u32, ColorSpace)> {
    DELIVERED
        .iter()
        .find(|(p, _, _)| *p == pf)
        .map(|(_, code, color)| (*code, *color))
}

fn ns_error(error: &NSError) -> CaptureError {
    let code = error.code();
    match AVError(code) {
        AVError::DeviceInUseByAnotherApplication | AVError::DeviceAlreadyUsedByAnotherSession => {
            CaptureError::DeviceBusy
        }
        AVError::ApplicationIsNotAuthorized => CaptureError::PermissionDenied,
        AVError::DeviceNotConnected => CaptureError::TargetNotFound,
        _ => CaptureError::Backend {
            domain: ErrorDomain::OsStatus,
            code: code as i64,
            recoverable: false,
        },
    }
}

fn not_running() -> CaptureError {
    CaptureError::Backend {
        domain: ErrorDomain::Library,
        code: 1,
        recoverable: true,
    }
}

fn authorization() -> AVAuthorizationStatus {
    // SAFETY: `AVMediaTypeVideo` is a framework constant, and the call only reads the
    // authorization state for the video media type.
    unsafe {
        match AVMediaTypeVideo {
            Some(video) => AVCaptureDevice::authorizationStatusForMediaType(video),
            None => AVAuthorizationStatus::Denied,
        }
    }
}

fn permission_now() -> Permission {
    match authorization() {
        AVAuthorizationStatus::Authorized => Permission::Granted,
        AVAuthorizationStatus::NotDetermined => Permission::NotDetermined,
        _ => Permission::Denied,
    }
}

fn require_permission() -> Result<(), CaptureError> {
    match permission_now() {
        Permission::Granted => Ok(()),
        Permission::NotDetermined => Err(CaptureError::PermissionNotDetermined),
        _ => Err(CaptureError::PermissionDenied),
    }
}

/// Every video capture device connected now.
fn devices() -> Vec<Retained<AVCaptureDevice>> {
    // SAFETY: `AVMediaTypeVideo` is a framework constant.
    let Some(video) = (unsafe { AVMediaTypeVideo }) else {
        return Vec::new();
    };
    // `devicesWithMediaType:` is deprecated in favour of discovery sessions, whose
    // device type for external cameras changed name in macOS 14 (a constant missing on
    // older systems would stop the binary from loading). It still lists every camera.
    // SAFETY: a class method returning an array of devices.
    #[allow(deprecated)]
    let list = unsafe { AVCaptureDevice::devicesWithMediaType(video) };
    list.iter().collect()
}

/// One audio input as far as pairing needs it: unique id, model id, manufacturer.
type AudioCandidate = (String, String, String);

/// The audio input that belongs with a camera: the only one reporting the same
/// non-empty model id and manufacturer (a webcam's microphone shares them with its
/// camera). Returns `None` when nothing matches or the match is ambiguous, so a wrong
/// microphone is never suggested. Compares what the OS reports and never looks at
/// vendor or model names (P3).
fn paired_audio(model: &str, maker: &str, audio: &[AudioCandidate]) -> Option<String> {
    if model.is_empty() {
        return None;
    }
    let mut found = audio
        .iter()
        .filter(|(_, m, k)| m == model && k == maker)
        .map(|(id, _, _)| id);
    let first = found.next()?;
    found.next().is_none().then(|| first.clone())
}

/// Every audio capture device connected now, for camera pairing. Runs only while
/// targets are listed (the Add source dialog), never at startup (P13).
fn audio_candidates() -> Vec<AudioCandidate> {
    // SAFETY: `AVMediaTypeAudio` is a framework constant.
    let Some(audio) = (unsafe { AVMediaTypeAudio }) else {
        return Vec::new();
    };
    // SAFETY: a class method returning an array of devices; plain property reads.
    #[allow(deprecated)]
    let list = unsafe { AVCaptureDevice::devicesWithMediaType(audio) };
    list.iter()
        .map(|d| unsafe {
            (
                d.uniqueID().to_string(),
                d.modelID().to_string(),
                d.manufacturer().to_string(),
            )
        })
        .collect()
}

fn device(unique_id: &str) -> Option<Retained<AVCaptureDevice>> {
    let id = NSString::from_str(unique_id);
    // SAFETY: looks a device up by its unique id; nothing is opened.
    unsafe { AVCaptureDevice::deviceWithUniqueID(&id) }
}

/// A CoreMedia duration as an exact frame rate (its inverse), when it fits.
fn duration_rate(time: CMTime) -> Option<FrameRate> {
    if !time.flags.contains(CMTimeFlags::Valid) || time.value <= 0 || time.timescale <= 0 {
        return None;
    }
    let value = u32::try_from(time.value).ok()?;
    let scale = u32::try_from(time.timescale).ok()?;
    FrameRate::new(scale, value)
}

/// The frame duration of `rate` for AVFoundation.
fn rate_duration(rate: FrameRate) -> Option<CMTime> {
    let r = rate.per_second();
    let (mut a, mut b) = (r.num(), r.den());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    let gcd = a.max(1);
    Some(CMTime {
        value: i64::from(r.den() / gcd),
        timescale: i32::try_from(r.num() / gcd).ok()?,
        flags: CMTimeFlags::Valid,
        epoch: 0,
    })
}

/// The device's formats as native modes, one per frame rate range and delivered pixel
/// format. `index` is the position in `device.formats()`.
fn native_modes(device: &AVCaptureDevice) -> Vec<NativeMode> {
    let mut modes = Vec::new();
    // SAFETY: plain property reads of a device and its immutable formats.
    unsafe {
        for (index, format) in device.formats().iter().enumerate() {
            let description = format.formatDescription();
            let dims = CMVideoFormatDescriptionGetDimensions(&description);
            let (Ok(width), Ok(height)) = (u32::try_from(dims.width), u32::try_from(dims.height))
            else {
                continue;
            };
            if width == 0 || height == 0 {
                continue;
            }
            for range in format.videoSupportedFrameRateRanges().iter() {
                // The shortest duration is the highest rate.
                let (Some(max), Some(min)) = (
                    duration_rate(range.minFrameDuration()),
                    duration_rate(range.maxFrameDuration()),
                ) else {
                    continue;
                };
                for (pf, _, _) in DELIVERED {
                    modes.push(NativeMode::new(width, height, pf, min, max, index));
                }
            }
        }
    }
    modes
}

fn target_of(device: &AVCaptureDevice, audio: &[AudioCandidate]) -> CaptureTarget {
    let modes = native_modes(device);
    // SAFETY: plain property reads.
    let (id, name, model, maker) = unsafe {
        (
            device.uniqueID().to_string(),
            device.localizedName().to_string(),
            device.modelID().to_string(),
            device.manufacturer().to_string(),
        )
    };
    let mut target = CaptureTarget::new(
        TargetId::new(format!("{CAMERA_PREFIX}{id}")),
        TargetKind::Camera,
        name,
    );
    if let Some(largest) = modes
        .iter()
        .max_by_key(|m| u64::from(m.width) * u64::from(m.height))
    {
        target = target.with_size(largest.width, largest.height);
    }
    if let Some(mic) = paired_audio(&model, &maker, audio) {
        // The audio adapter names its devices by the same unique id.
        target = target.with_related(RelatedDevice::audio(mic));
    }
    target.with_modes(camera::camera_modes(&modes))
}

/// Checks the zero-copy part of a request (capture-port.md, zero-copy negotiation).
/// IOSurfaces are global: any Metal device of the machine can import them.
fn accept_memory(request: &CaptureRequest) -> Result<(), CaptureError> {
    if !request.memory.contains(&MemoryKind::Metal) {
        return Err(CaptureError::FormatUnsupported);
    }
    let Some(gpu) = request.gpu.as_ref() else {
        return Err(CaptureError::FormatUnsupported);
    };
    if !matches!(gpu.device, GpuDeviceId::MetalRegistryId(_))
        || !gpu.sync.contains(&SyncKind::Ready)
    {
        return Err(CaptureError::FormatUnsupported);
    }
    Ok(())
}

/// The device format a session runs with, found again at every `start` (the device
/// object can change when the camera is plugged in again).
#[derive(Clone, Copy)]
struct Chosen {
    width: u32,
    height: u32,
    /// Media subtype of the device format, to tell formats of one size apart.
    subtype: u32,
    pixel_format: PixelFormat,
    rate: FrameRate,
}

fn chosen_of(device: &AVCaptureDevice, choice: &ModeChoice) -> Option<Chosen> {
    // SAFETY: plain property reads.
    let subtype = unsafe {
        let formats = device.formats();
        let format = formats.iter().nth(choice.index)?;
        format.formatDescription().media_sub_type()
    };
    Some(Chosen {
        width: choice.size.width,
        height: choice.size.height,
        subtype,
        pixel_format: choice.pixel_format,
        rate: choice.rate,
    })
}

/// The device format matching `chosen`, and a frame duration it supports.
fn find_format(
    device: &AVCaptureDevice,
    chosen: &Chosen,
) -> Option<(Retained<AVCaptureDeviceFormat>, CMTime)> {
    let want = rate_duration(chosen.rate)?;
    // SAFETY: plain property reads of a device and its immutable formats.
    unsafe {
        device.formats().iter().find_map(|format| {
            let description = format.formatDescription();
            let dims = CMVideoFormatDescriptionGetDimensions(&description);
            if (dims.width as i64, dims.height as i64)
                != (i64::from(chosen.width), i64::from(chosen.height))
                || description.media_sub_type() != chosen.subtype
            {
                return None;
            }
            let fits = format.videoSupportedFrameRateRanges().iter().any(|range| {
                let (min, max) = (range.minFrameDuration(), range.maxFrameDuration());
                time_le(min, want) && time_le(want, max)
            });
            fits.then_some((format, want))
        })
    }
}

/// Exact `a <= b` for valid CoreMedia times.
fn time_le(a: CMTime, b: CMTime) -> bool {
    if a.timescale <= 0 || b.timescale <= 0 {
        return false;
    }
    i128::from(a.value) * i128::from(b.timescale) <= i128::from(b.value) * i128::from(a.timescale)
}

/// Factory of the AVFoundation camera adapter.
#[derive(Debug, Default)]
pub struct AvfCaptureFactory;

impl AvfCaptureFactory {
    /// Creates the factory. Does nothing until used.
    pub fn new() -> Self {
        Self
    }
}

impl AdapterFactory for AvfCaptureFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "AVFoundation cameras"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        // Cheap (capture-port.md: within 50 ms): no device is asked.
        let set =
            |items: &[&str]| CapabilityValue::Set(items.iter().map(|s| (*s).to_owned()).collect());
        let formats: Vec<&str> = DELIVERED.iter().map(|(pf, _, _)| pf.as_str()).collect();
        Ok(CapabilitySet::new()
            .with(caps::TARGET_KINDS, set(&["camera"]))
            .with(caps::TARGET_ENUMERATE, set(&["camera"]))
            .with(caps::TARGET_PICKER, set(&[]))
            .with(caps::PIXEL_FORMATS, set(&formats))
            .with(caps::MEMORY, set(&[MemoryKind::Metal.as_str()]))
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

impl CaptureFactory for AvfCaptureFactory {
    fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
        if kind != TargetKind::Camera {
            return Err(CaptureError::Unsupported(
                "AVFoundation captures cameras only",
            ));
        }
        // Listing devices and their formats needs no permission and opens nothing.
        autoreleasepool(|_| {
            let audio = audio_candidates();
            Ok(devices().iter().map(|d| target_of(d, &audio)).collect())
        })
    }

    fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
        let unique_id = request
            .target
            .as_str()
            .strip_prefix(CAMERA_PREFIX)
            .ok_or(CaptureError::TargetNotFound)?
            .to_owned();
        require_permission()?;
        // Refuse what cannot be delivered before asking the OS.
        accept_memory(request)?;
        if !request
            .pixel_formats
            .iter()
            .any(|pf| delivered(*pf).is_some())
        {
            return Err(CaptureError::FormatUnsupported);
        }
        let chosen = autoreleasepool(|_| {
            let device = device(&unique_id).ok_or(CaptureError::TargetNotFound)?;
            let modes = native_modes(&device);
            let choice = camera::choose_mode(
                &modes,
                &request.pixel_formats,
                request.size,
                request.frame_rate,
            )?;
            // A rate AVFoundation cannot be given exactly is refused now, not at start.
            rate_duration(choice.rate).ok_or(CaptureError::FormatUnsupported)?;
            chosen_of(&device, &choice).ok_or(CaptureError::FormatUnsupported)
        })?;
        let (_, color) = delivered(chosen.pixel_format).ok_or(CaptureError::FormatUnsupported)?;
        let format = VideoFormat::new(
            Size::new(chosen.width, chosen.height),
            chosen.pixel_format,
            color,
        );
        Ok(Box::new(AvfSession {
            unique_id,
            chosen,
            clock: Arc::clone(&request.clock),
            format,
            active: None,
            next_sequence: 0,
            last_timestamp: None,
            base: CaptureStats::default(),
        }))
    }

    fn permission(&self, kind: TargetKind) -> Permission {
        match kind {
            TargetKind::Camera => permission_now(),
            _ => Permission::Granted,
        }
    }

    fn request_permission(&self, kind: TargetKind) -> Result<Permission, CaptureError> {
        if kind != TargetKind::Camera {
            return Ok(Permission::Granted);
        }
        match permission_now() {
            Permission::NotDetermined => {}
            other => return Ok(other),
        }
        // SAFETY: `AVMediaTypeVideo` is a framework constant.
        let Some(video) = (unsafe { AVMediaTypeVideo }) else {
            return Ok(Permission::Denied);
        };
        let (tx, rx) = mpsc::sync_channel::<bool>(1);
        let block = RcBlock::new(move |granted: Bool| {
            let _ = tx.try_send(granted.as_bool());
        });
        // SAFETY: the block matches the documented handler signature; AVFoundation
        // retains it until it is called. The process needs `NSCameraUsageDescription`
        // in its (or its responsible app's) Info.plist, otherwise macOS ends it here.
        unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(video, &block) };
        // Waits for the user's answer (control thread only, capture-port.md).
        match rx.recv() {
            Ok(true) => Ok(Permission::Granted),
            Ok(false) => Ok(Permission::Denied),
            Err(_) => Ok(permission_now()),
        }
    }
}

/// A pixel buffer held by a ring slot, with the surface it names.
struct Held {
    _buffer: CFRetained<CVPixelBuffer>,
    surface: CFRetained<IOSurfaceRef>,
}

/// The ring and the pixel buffers its slots name. Frames hold an `Arc` of it.
struct Shared {
    ring: Ring,
    /// One entry per slot. Only the producer (the delegate, or `stop`) locks it.
    slots: Mutex<Vec<Option<Sendable<Held>>>>,
}

impl AsRef<Ring> for Shared {
    fn as_ref(&self) -> &Ring {
        &self.ring
    }
}

// SAFETY: delegates to `Ring`, whose counting is exact when its calling protocol is
// followed (`rbs_capture::handoff::Ring`): the producer publishes, abandons and
// replaces only slots it claimed, and every lease releases exactly the reference it
// adopted or retained. A slot's pixel buffer (and so its IOSurface) is retained in
// `slots` while the slot is referenced: the producer only drops it for a free slot
// (which only the producer can make referenced again) or a claimed one, so it stays
// valid and unwritten by AVFoundation while any lease (which holds an `Arc<Shared>`)
// refers to it. Neither method locks.
unsafe impl LeaseOwner for Shared {
    fn retain(&self, token: u64) {
        self.ring.retain(token);
    }

    fn release(&self, token: u64) {
        self.ring.release(token);
    }
}

/// Surfaces a frame was handed out with and that the producer no longer holds, kept
/// for the grace so their pointer values are not reused while an importer may cache
/// them.
struct Seen {
    entries: Vec<(Sendable<CFRetained<IOSurfaceRef>>, Instant)>,
}

impl Seen {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
        }
    }

    /// Keeps `surface` from `now`. Never allocates: when full, the oldest entry goes to
    /// the graveyard, which keeps it for the rest of its grace.
    fn keep(&mut self, surface: CFRetained<IOSurfaceRef>, now: Instant) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|(s, _)| std::ptr::eq(&*s.0, &*surface))
        {
            entry.1 = now;
            return;
        }
        if self.entries.len() == SEEN_MAX {
            let (oldest, since) = self.entries.remove(0);
            GRAVEYARD.bury(Box::new(SurfaceGrave {
                _surface: oldest,
                until: since + HANDLE_GRACE,
            }));
        }
        self.entries.push((Sendable(surface), now));
    }

    /// Releases surfaces whose grace has passed.
    fn expire(&mut self, now: Instant) {
        self.entries
            .retain(|(_, since)| now.saturating_duration_since(*since) < HANDLE_GRACE);
    }
}

/// One surface moved out of a full [`Seen`] list. Kept until its own grace ends.
struct SurfaceGrave {
    _surface: Sendable<CFRetained<IOSurfaceRef>>,
    until: Instant,
}

impl Grave for SurfaceGrave {
    fn is_idle(&self) -> bool {
        // Always true (`until - HANDLE_GRACE` lies in the past): the graveyard keeps the
        // surface a full grace from burial, which ends after `until`.
        Instant::now() + HANDLE_GRACE >= self.until
    }
    fn bytes(&self) -> u64 {
        0
    }
}

/// The surfaces of a stopped capture. Kept while frames reference them (so the last
/// lease never frees anything on the render thread) and for the grace after that.
struct RingGrave {
    shared: Arc<Shared>,
    _seen: Seen,
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

/// Producer state, touched by the delegate and by `stop`.
struct Producer {
    shared: Arc<Shared>,
    /// CoreVideo code of the pixel buffers the output was asked for.
    expected: u32,
    seen: Seen,
    sequence: u64,
    clock: Arc<dyn Clock>,
    drift: DriftEstimator,
    last_timestamp: Option<Timestamp>,
    stopped: bool,
    /// The session's synchronization clock, set once the session is configured.
    sync_clock: Option<Sendable<Retained<CMClock>>>,
    host_clock: Sendable<CFRetained<CMClock>>,
}

/// Nanoseconds on the host clock (`mach_absolute_time`), the clock of the capture
/// session's sample buffers.
fn host_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out pointer.
    let _ = unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts) };
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs.saturating_mul(1_000_000_000).saturating_add(nanos)
}

fn cm_time_nanos(time: CMTime) -> Option<u64> {
    if !time.flags.contains(CMTimeFlags::Valid) || time.timescale <= 0 || time.value < 0 {
        return None;
    }
    let nanos = i128::from(time.value) * 1_000_000_000 / i128::from(time.timescale);
    u64::try_from(nanos).ok()
}

fn clamp_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

impl Producer {
    /// Gives the pixel buffers of free slots back to AVFoundation's pool, so held
    /// frames plus the newest one are all the adapter keeps. A free slot can only be
    /// made referenced again by the producer (`claim`), so this cannot race with the
    /// render thread.
    fn return_free(&mut self, now: Instant) {
        let ring = &self.shared.ring;
        let mut slots = lock(&self.shared.slots);
        for (slot, entry) in slots.iter_mut().enumerate() {
            if !ring.is_free(slot) {
                continue;
            }
            if let Some(held) = entry.take() {
                if ring.handed_out(slot) {
                    self.seen.keep(held.0.surface, now);
                }
            }
        }
    }

    fn on_drop(&mut self) {
        if self.stopped {
            return;
        }
        // Frames AVFoundation discarded before the delegate saw them: received from
        // the OS but not delivered (invariant 3).
        self.shared.ring.count_drop();
        self.sequence += 1;
    }

    fn on_sample(&mut self, sample: &CMSampleBuffer) {
        if self.stopped {
            return;
        }
        let arrived = self.clock.now();
        let host = host_nanos();
        let now = Instant::now();
        self.seen.expire(now);
        self.return_free(now);

        // SAFETY: the image buffer of a video sample is a CVPixelBuffer.
        let picture = unsafe { sample.image_buffer() }
            .filter(|buffer| CVPixelBufferGetPixelFormatType(buffer) == self.expected)
            .and_then(|buffer| CVPixelBufferGetIOSurface(Some(&buffer)).map(|s| (buffer, s)));
        let Some((buffer, surface)) = picture else {
            // Received but not deliverable (no IOSurface, unexpected format): counted,
            // so a source that only sends such samples shows drops, not silence.
            self.shared.ring.count_drop();
            self.sequence += 1;
            return;
        };
        let width = clamp_u32(CVPixelBufferGetWidth(&buffer));
        let height = clamp_u32(CVPixelBufferGetHeight(&buffer));

        let shared = Arc::clone(&self.shared);
        let ring = &shared.ring;
        let Some(slot) = ring.claim() else {
            // The consumer holds every slot: drop rather than wait (invariant 9).
            ring.count_drop();
            self.sequence += 1;
            return;
        };
        {
            let mut slots = lock(&shared.slots);
            let Some(entry) = slots.get_mut(slot) else {
                ring.abandon(slot);
                return;
            };
            let value = CFRetained::as_ptr(&surface).as_ptr() as u64;
            if entry.is_none() || ring.handle(slot) != Some(value) {
                if let Some(old) = entry.take() {
                    // A surface a frame was handed out with keeps its pointer value
                    // reserved for the grace; others go back to AVFoundation now.
                    if ring.handed_out(slot) {
                        self.seen.keep(old.0.surface, now);
                    }
                }
                ring.set_handle(slot, value);
            }
            *entry = Some(Sendable(Held {
                _buffer: buffer,
                surface,
            }));
        }

        // Clock edge (ADR-0004): pair the host clock with the master clock now, then
        // convert the sample's host-clock presentation time.
        // SAFETY: reading the timing of a valid sample buffer.
        let pts = unsafe { sample.presentation_time_stamp() };
        // Sample times are on the session's synchronization clock, usually but not
        // always the host clock; convert them to host time first.
        let pts = match self.sync_clock.as_ref() {
            // SAFETY: both arguments are clocks, the kinds the call accepts.
            Some(clock) => unsafe { CMSyncConvertTime(pts, &clock.0, &self.host_clock.0) },
            None => pts,
        };
        let source = cm_time_nanos(pts);
        let mapped = match source {
            Some(source) => {
                let _ = self.drift.observe(host, arrived);
                let mapped = self
                    .drift
                    .mapping()
                    .map_or(arrived, |m| m.to_master(source));
                // A time on an unexpected clock maps far from arrival: use arrival.
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
                source.unwrap_or(0),
                width,
                height,
            ),
        );
        self.sequence += 1;
    }
}

struct DelegateIvars {
    producer: Arc<Mutex<Producer>>,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - `RbsAvfDelegate` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "RbsAvfDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for Delegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(
            &self,
            _output: &AVCaptureOutput,
            sample: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            // GCD queues do not always drain an autorelease pool per work item.
            autoreleasepool(|_| lock(&self.ivars().producer).on_sample(sample));
        }

        #[unsafe(method(captureOutput:didDropSampleBuffer:fromConnection:))]
        fn did_drop(
            &self,
            _output: &AVCaptureOutput,
            _sample: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            lock(&self.ivars().producer).on_drop();
        }
    }
);

impl Delegate {
    fn new(ivars: DelegateIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        // SAFETY: `init` of NSObject on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

struct Active {
    session: Sendable<Retained<AVCaptureSession>>,
    output: Sendable<Retained<AVCaptureVideoDataOutput>>,
    _delegate: Sendable<Retained<Delegate>>,
    _queue: Sendable<DispatchRetained<DispatchQueue>>,
    observers: Vec<Sendable<Retained<ProtocolObject<dyn NSObjectProtocol>>>>,
    producer: Arc<Mutex<Producer>>,
    consumer: Consumer<Shared>,
    shared: Arc<Shared>,
    lost: Arc<AtomicBool>,
}

impl Active {
    /// Stops the session and detaches everything that could still call back.
    fn shut_down(&self) {
        let center = NSNotificationCenter::defaultCenter();
        for observer in &self.observers {
            // SAFETY: removes an observer token returned by `addObserverForName`.
            unsafe { center.removeObserver(observer.0.as_ref()) };
        }
        // SAFETY: stopping a configured session (blocks until it stopped) and
        // detaching its delegate; a nil delegate with a nil queue is allowed.
        unsafe {
            self.session.0.stopRunning();
            self.output.0.setSampleBufferDelegate_queue(None, None);
        }
        // A delegate call queued before the detach may still run: wait for the serial
        // queue to drain, so none runs after `stop` returns and nothing is freed there.
        self._queue.0.exec_sync(|| {});
    }
}

struct AvfSession {
    unique_id: String,
    chosen: Chosen,
    clock: Arc<dyn Clock>,
    format: VideoFormat,
    active: Option<Active>,
    next_sequence: u64,
    last_timestamp: Option<Timestamp>,
    /// Counters of earlier Active periods.
    base: CaptureStats,
}

/// Observes `name` (from `object`) and sets `lost` when it is posted.
fn observe(
    name: &NSString,
    object: &AnyObject,
    lost: &Arc<AtomicBool>,
) -> Sendable<Retained<ProtocolObject<dyn NSObjectProtocol>>> {
    let lost = Arc::clone(lost);
    let block = RcBlock::new(move |_: NonNull<NSNotification>| {
        lost.store(true, Ordering::Release);
    });
    let center = NSNotificationCenter::defaultCenter();
    // SAFETY: the block only stores to an atomic it owns, so it may run on any thread
    // (no queue: it runs on the posting thread); the center copies it.
    Sendable(unsafe {
        center.addObserverForName_object_queue_usingBlock(Some(name), Some(object), None, &block)
    })
}

impl AvfSession {
    fn activate(&mut self) -> Result<Active, CaptureError> {
        // Never let AVFoundation show the prompt from `start` (capture-port.md).
        require_permission()?;
        let device = device(&self.unique_id).ok_or(CaptureError::TargetNotFound)?;
        // SAFETY: a property read.
        if !unsafe { device.isConnected() } {
            return Err(CaptureError::TargetNotFound);
        }
        let (format, duration) =
            // The device is there but no longer offers the format (another model behind
            // the same id): not recoverable by retrying.
            find_format(&device, &self.chosen).ok_or(CaptureError::FormatUnsupported)?;
        let (code, _) =
            delivered(self.chosen.pixel_format).ok_or(CaptureError::FormatUnsupported)?;

        // SAFETY: opens the device for capture; errors come back as NSError.
        let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }
            .map_err(|e| ns_error(&e))?;
        // SAFETY: fresh objects, configured before the session runs.
        let (session, output) =
            unsafe { (AVCaptureSession::new(), AVCaptureVideoDataOutput::new()) };
        // SAFETY: the key is a CF string constant, toll-free bridged to NSString; the
        // value is a number, and only keys AVFoundation documents for macOS are set.
        let settings = unsafe {
            let key = &*(kCVPixelBufferPixelFormatTypeKey as *const _ as *const NSString);
            let value: Retained<AnyObject> = Retained::into_super(Retained::into_super(
                Retained::into_super(NSNumber::new_u32(code)),
            ));
            NSDictionary::from_retained_objects(&[key], &[value])
        };
        // SAFETY: valid settings; late frames are discarded instead of queued.
        unsafe {
            output.setVideoSettings(Some(&settings));
            output.setAlwaysDiscardsLateVideoFrames(true);
        }

        let shared = Arc::new(Shared {
            ring: Ring::new(&[0; RING_SLOTS]),
            slots: Mutex::new((0..RING_SLOTS).map(|_| None).collect()),
        });
        let lost = Arc::new(AtomicBool::new(false));
        let producer = Arc::new(Mutex::new(Producer {
            shared: Arc::clone(&shared),
            expected: code,
            seen: Seen::with_capacity(SEEN_MAX),
            sequence: self.next_sequence,
            clock: Arc::clone(&self.clock),
            drift: DriftEstimator::new(Duration::from_millis(100)),
            last_timestamp: self.last_timestamp,
            stopped: false,
            sync_clock: None,
            // SAFETY: returns the process-wide host time clock.
            host_clock: Sendable(unsafe { CMClock::host_time_clock() }),
        }));
        let delegate = Delegate::new(DelegateIvars {
            producer: Arc::clone(&producer),
        });
        let queue = DispatchQueue::new("org.rbs.capture.avf", None);
        let sink = ProtocolObject::<dyn AVCaptureVideoDataOutputSampleBufferDelegate>::from_ref(
            &*delegate,
        );
        // SAFETY: a serial queue and a delegate both kept in `Active` until `stop`
        // detaches the delegate.
        unsafe { output.setSampleBufferDelegate_queue(Some(sink), Some(&queue)) };

        // SAFETY: the session is configured inside begin/commit; inputs and outputs are
        // only added after `canAdd*` accepted them (adding otherwise throws); the device
        // format and durations are set while the device is locked, the format is one of
        // the device's and the duration lies in one of its ranges (`find_format`).
        let configured = unsafe {
            session.beginConfiguration();
            let ok = session.canAddInput(&input)
                && {
                    session.addInput(&input);
                    session.canAddOutput(&output)
                }
                && {
                    session.addOutput(&output);
                    true
                };
            let locked = ok && device.lockForConfiguration().is_ok();
            if locked {
                device.setActiveFormat(&format);
                device.setActiveVideoMinFrameDuration(duration);
                device.setActiveVideoMaxFrameDuration(duration);
            }
            session.commitConfiguration();
            (ok, locked)
        };
        let fail = |error: CaptureError| {
            // SAFETY: detaching the delegate of an output that never ran.
            unsafe { output.setSampleBufferDelegate_queue(None, None) };
            error
        };
        match configured {
            (false, _) => return Err(fail(CaptureError::DeviceBusy)),
            (true, false) => return Err(fail(not_running())),
            (true, true) => {}
        }

        // SAFETY: framework constants.
        let (disconnected, runtime_error) = unsafe {
            (
                AVCaptureDeviceWasDisconnectedNotification,
                AVCaptureSessionRuntimeErrorNotification,
            )
        };
        // SAFETY: a property read of a configured session.
        lock(&producer).sync_clock = unsafe { session.synchronizationClock() }.map(Sendable);
        let observers = vec![
            observe(disconnected, &device, &lost),
            observe(runtime_error, &session, &lost),
        ];
        // SAFETY: starts the configured session (blocks until running or failed), then
        // releases the configuration lock taken above; the format stays while running.
        let running = unsafe {
            session.startRunning();
            device.unlockForConfiguration();
            session.isRunning()
        };
        let consumer = unsafe {
            // SAFETY: `shared`'s ring only ever names IOSurfaces of pixel buffers of
            // `code` (checked per sample) at the published size, retained in
            // `shared.slots` while their slot is referenced (see the `LeaseOwner`
            // impl); AVFoundation delivers complete frames only.
            Consumer::new(Arc::clone(&shared), self.format)
        };
        let active = Active {
            session: Sendable(session),
            output: Sendable(output),
            _delegate: Sendable(delegate),
            _queue: Sendable(queue),
            observers,
            producer,
            consumer,
            shared,
            lost,
        };
        if !running {
            active.shut_down();
            lock(&active.producer).stopped = true;
            return Err(not_running());
        }
        Ok(active)
    }
}

impl CaptureSession for AvfSession {
    fn format(&self) -> VideoFormat {
        self.active
            .as_ref()
            .map_or(self.format, |a| a.consumer.format())
    }

    fn start(&mut self) -> Result<(), CaptureError> {
        if self.active.is_some() {
            return Ok(());
        }
        let active = autoreleasepool(|_| self.activate())?;
        self.active = Some(active);
        Ok(())
    }

    fn stop(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        autoreleasepool(|_| active.shut_down());
        let seen = {
            // Waits for a delegate call that is still running, then makes later ones
            // no-ops.
            let mut producer = lock(&active.producer);
            producer.stopped = true;
            self.next_sequence = producer.sequence;
            self.last_timestamp = producer.last_timestamp;
            std::mem::replace(&mut producer.seen, Seen::with_capacity(0))
        };
        let ring = &active.shared.ring;
        let waiting = ring.clear();
        self.base.delivered += active.consumer.delivered();
        self.base.dropped += ring.dropped() + u64::from(waiting);
        self.format = active.consumer.format();
        // Pixel buffers nobody took a frame of never left the adapter: give them back
        // now. The rest stay in the graveyard while frames reference them and for the
        // grace after that.
        let bytes = {
            let mut slots = lock(&active.shared.slots);
            let mut bytes = 0u64;
            for (slot, entry) in slots.iter_mut().enumerate() {
                if ring.is_free(slot) && !ring.handed_out(slot) {
                    *entry = None;
                } else if let Some(held) = entry.as_ref() {
                    let w = CVPixelBufferGetWidth(&held.0._buffer) as u64;
                    let h = CVPixelBufferGetHeight(&held.0._buffer) as u64;
                    bytes += match self.format.pixel_format {
                        PixelFormat::Nv12 => w * h * 3 / 2,
                        _ => w * h * 4,
                    };
                }
            }
            bytes
        };
        GRAVEYARD.bury(Box::new(RingGrave {
            shared: Arc::clone(&active.shared),
            _seen: seen,
            bytes,
        }));
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

impl Drop for AvfSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mic(id: &str, model: &str, maker: &str) -> AudioCandidate {
        (id.into(), model.into(), maker.into())
    }

    #[test]
    fn a_camera_pairs_with_the_one_microphone_reporting_its_model() {
        let audio = [
            mic("a", "M1", "X"),
            mic("b", "M2", "X"),
            mic("c", "M2", "Y"),
        ];
        assert_eq!(paired_audio("M2", "X", &audio), Some("b".to_string()));
        assert_eq!(paired_audio("M2", "Z", &audio), None);
        assert_eq!(paired_audio("", "X", &[mic("d", "", "X")]), None);
        // Two matches are ambiguous: suggest nothing rather than a wrong microphone.
        let twins = [mic("e", "M3", "X"), mic("f", "M3", "X")];
        assert_eq!(paired_audio("M3", "X", &twins), None);
    }

    #[test]
    fn durations_and_rates_round_trip_exactly() {
        let t = CMTime {
            value: 1_001,
            timescale: 30_000,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let rate = duration_rate(t);
        assert_eq!(rate, Some(FrameRate::FPS_29_97));
        let back = rate.and_then(rate_duration);
        assert!(back.is_some_and(|b| time_le(b, t) && time_le(t, b)));
        let invalid = CMTime {
            flags: CMTimeFlags(0),
            ..t
        };
        assert_eq!(duration_rate(invalid), None);
    }
}
