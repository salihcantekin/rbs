//! The macOS backend: `VNGeneratePersonSegmentationRequest` on a worker thread.
//!
//! The worker owns every Vision object (they are not `Send`): it builds a new request
//! for every frame, so a mask depends on its own frame alone (Vision's stateful request
//! smooths over earlier frames and trails movement). The render thread only touches a one-slot mailbox with `try_lock`, so it
//! never waits for the worker (principle P6).
//!
//! Zero cost when unused (P13): no thread exists before `warm`, and `close` joins it.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use objc2::rc::Retained;
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_OneComponent8,
};
use objc2_foundation::{NSArray, NSOperatingSystemVersion, NSProcessInfo};
use objc2_vision::{
    VNGeneratePersonSegmentationRequest, VNGeneratePersonSegmentationRequestQualityLevel,
    VNRequest, VNSequenceRequestHandler,
};

use rbs_core::media::{FramePayload, MemoryKind, PixelFormat, Size, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_segment::{
    BackendInfo, ComputeUnit, CostHint, LevelId, Matte, MattePlane, PersonMaskBackend,
    PersonMaskSession, QualityLevel, SegmentError, SegmentHost, SessionRequest, SubmitOutcome,
};

/// Apple Vision person segmentation. Needs macOS 12 or later.
#[derive(Debug, Default, Clone, Copy)]
pub struct VisionBackend;

impl VisionBackend {
    /// Creates the backend.
    pub const fn new() -> Self {
        Self
    }
}

fn vision_available() -> bool {
    let required = NSOperatingSystemVersion {
        majorVersion: 12,
        minorVersion: 0,
        patchVersion: 0,
    };
    NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(required)
}

fn quality_of(level: LevelId) -> Option<VNGeneratePersonSegmentationRequestQualityLevel> {
    match level {
        LevelId::Low => Some(VNGeneratePersonSegmentationRequestQualityLevel::Fast),
        LevelId::Medium => Some(VNGeneratePersonSegmentationRequestQualityLevel::Balanced),
        LevelId::High => Some(VNGeneratePersonSegmentationRequestQualityLevel::Accurate),
        _ => None,
    }
}

impl PersonMaskBackend for VisionBackend {
    fn type_id(&self) -> &'static str {
        crate::TYPE_ID
    }

    fn probe(&self, _host: &dyn SegmentHost) -> BackendInfo {
        if !vision_available() {
            return BackendInfo::unavailable();
        }
        // Measured on an M4 Pro, 1620x1080 input, default compute device (the Neural
        // Engine): about 3, 9 and 30 ms per frame (research doc, 2026-11-24).
        let mut info = BackendInfo::unavailable();
        info.available = true;
        info.levels = vec![
            QualityLevel::new(
                LevelId::Low,
                CostHint::new(3.0, ComputeUnit::Npu, "fast, mask 256x192"),
            ),
            QualityLevel::new(
                LevelId::Medium,
                CostHint::new(9.0, ComputeUnit::Npu, "balanced, mask 512x384"),
            ),
            QualityLevel::new(
                LevelId::High,
                CostHint::new(30.0, ComputeUnit::Npu, "accurate, mask 2016x1512"),
            ),
        ];
        info.input = MemoryKind::System;
        info.output = MemoryKind::System;
        info.temporal = false;
        info.hardware_assisted = true;
        info
    }

    fn open(&self, request: SessionRequest) -> Result<Box<dyn PersonMaskSession>, SegmentError> {
        if !vision_available() {
            return Err(SegmentError::Unavailable);
        }
        let quality = quality_of(request.level).ok_or(SegmentError::Unsupported {
            level: request.level,
        })?;
        Ok(Box::new(VisionSession::new(quality, request.input_size)))
    }
}

/// A frame copied for the worker: the pixels as BGRA, tightly packed.
struct Pending {
    timestamp: Timestamp,
    size: Size,
    bgra: Vec<u8>,
}

#[derive(Default)]
struct Mailbox {
    /// Pre-sized buffers the render thread fills; the worker gives them back after use,
    /// so `submit` allocates nothing after `warm` (principle P6).
    free: Vec<Vec<u8>>,
    pending: Option<Pending>,
    matte: Option<Matte>,
    failed: Option<SegmentError>,
    reset: bool,
}

struct Shared {
    mailbox: Mutex<Mailbox>,
    wake: Condvar,
    stop: AtomicBool,
}

struct VisionSession {
    quality: VNGeneratePersonSegmentationRequestQualityLevel,
    input_size: Size,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

/// Buffers in circulation: one being filled, one pending, one in the worker.
const BUFFERS: usize = 3;

impl VisionSession {
    fn new(quality: VNGeneratePersonSegmentationRequestQualityLevel, input_size: Size) -> Self {
        let shared = Arc::new(Shared {
            mailbox: Mutex::new(Mailbox::default()),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        Self {
            quality,
            input_size,
            shared,
            worker: None,
        }
    }
}

/// Copies a system-memory frame into `bgra` as tightly packed BGRA, the layout Vision
/// takes, reusing the buffer's capacity. `None` when the frame is not RGBA or BGRA
/// system memory.
fn to_bgra(frame: &VideoFrame, bgra: &mut Vec<u8>) -> Option<(Timestamp, Size)> {
    let FramePayload::System(system) = frame.payload() else {
        return None;
    };
    let format = system.format();
    let swap = match format.pixel_format {
        PixelFormat::Bgra8 | PixelFormat::Bgra8Srgb => false,
        PixelFormat::Rgba8 | PixelFormat::Rgba8Srgb => true,
        _ => return None,
    };
    let (data, stride) = system.plane(0)?;
    let width = format.size.width as usize;
    let height = format.size.height as usize;
    bgra.clear();
    for row in 0..height {
        let line = data.get(row * stride..row * stride + width * 4)?;
        if swap {
            for pixel in line.chunks_exact(4) {
                bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
            }
        } else {
            bgra.extend_from_slice(line);
        }
    }
    Some((frame.timestamp(), format.size))
}

impl PersonMaskSession for VisionSession {
    fn warm(&mut self) -> Result<(), SegmentError> {
        if self.worker.is_some() {
            return Ok(());
        }
        let capacity = self.input_size.width as usize * self.input_size.height as usize * 4;
        {
            let mut mailbox = self
                .shared
                .mailbox
                .lock()
                .map_err(|_| SegmentError::DeviceLost)?;
            mailbox.free = (0..BUFFERS - 1)
                .map(|_| Vec::with_capacity(capacity))
                .collect();
        }
        let shared = Arc::clone(&self.shared);
        let quality = self.quality;
        let handle = std::thread::Builder::new()
            .name("rbs-segment-vision".into())
            .spawn(move || worker_main(&shared, quality))
            .map_err(|_| SegmentError::Unavailable)?;
        self.worker = Some(handle);
        Ok(())
    }

    fn submit(&mut self, frame: &VideoFrame) -> SubmitOutcome {
        // The worker holds the lock only to move a pointer, so `try_lock` fails rarely;
        // when it does, the frame is dropped and the previous matte stays in use. The
        // copy is small (the filter downscales first) and reuses a pre-sized buffer.
        let Ok(mut mailbox) = self.shared.mailbox.try_lock() else {
            return SubmitOutcome::DroppedBusy;
        };
        let buffer = match mailbox.pending.take() {
            Some(old) => old.bgra,
            None => match mailbox.free.pop() {
                Some(buffer) => buffer,
                None => return SubmitOutcome::DroppedBusy,
            },
        };
        let mut buffer = buffer;
        match to_bgra(frame, &mut buffer) {
            Some((timestamp, size)) => {
                mailbox.pending = Some(Pending {
                    timestamp,
                    size,
                    bgra: buffer,
                });
            }
            None => {
                mailbox.free.push(buffer);
                return SubmitOutcome::NotShareable;
            }
        }
        drop(mailbox);
        self.shared.wake.notify_one();
        SubmitOutcome::Accepted
    }

    fn latest(&mut self) -> Result<Option<Matte>, SegmentError> {
        let Ok(mut mailbox) = self.shared.mailbox.try_lock() else {
            return Ok(None);
        };
        if let Some(error) = mailbox.failed {
            return Err(error);
        }
        Ok(mailbox.matte.take())
    }

    fn reset(&mut self) {
        if let Ok(mut mailbox) = self.shared.mailbox.try_lock() {
            mailbox.reset = true;
            mailbox.matte = None;
        }
    }

    fn close(mut self: Box<Self>) {
        self.shutdown();
    }
}

impl VisionSession {
    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for VisionSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Everything the worker owns. Vision objects are created on the worker thread.
struct Engine {
    quality: VNGeneratePersonSegmentationRequestQualityLevel,
    handler: Retained<VNSequenceRequestHandler>,
    /// Input buffers for Vision, made once per picture size and reused.
    pool: Option<InputPool>,
}

/// IOSurface-backed BGRA buffers of one size. With a plain buffer (no IOSurface) Vision
/// scales and converts the picture for the model with VideoToolbox on the CPU, the
/// largest part of the worker's time in a release profile; with these buffers the
/// Accurate level took 34.9 instead of 40.9 ms of CPU per 512x288 picture (M4 Pro,
/// 2026-11-24; Fast and Balanced unchanged, masks equal). The pool also ends one
/// buffer allocation per frame.
struct InputPool {
    size: Size,
    pool: CFRetained<CVPixelBufferPool>,
}

impl InputPool {
    fn new(size: Size) -> Option<Self> {
        let width = CFNumber::new_i32(i32::try_from(size.width).ok()?);
        let height = CFNumber::new_i32(i32::try_from(size.height).ok()?);
        // The four-character code fits in an i32 bit for bit; CoreVideo reads it back so.
        let format = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
        let surface = CFDictionary::<CFString, CFType>::empty();
        let yes = objc2_core_foundation::CFBoolean::new(true);
        // SAFETY: the keys are CoreVideo constants that live for the whole process.
        let keys: [&CFString; 5] = unsafe {
            [
                kCVPixelBufferWidthKey,
                kCVPixelBufferHeightKey,
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
            ]
        };
        let values: [&CFType; 5] = [&width, &height, &format, &surface, yes];
        let attributes = CFDictionary::from_slices(&keys, &values);
        let mut raw: *mut CVPixelBufferPool = std::ptr::null_mut();
        // SAFETY: `raw` is a valid out pointer and `attributes` a dictionary of pixel
        // buffer keys with values of the documented types. On success CoreVideo returns
        // a +1 retained pool that `CFRetained::from_raw` adopts.
        let pool = unsafe {
            if CVPixelBufferPool::create(
                None,
                None,
                Some(attributes.as_opaque()),
                NonNull::new(&mut raw)?,
            ) != 0
            {
                return None;
            }
            CFRetained::from_raw(NonNull::new(raw)?)
        };
        Some(Self { size, pool })
    }

    fn buffer(&self) -> Option<Retained<CVPixelBuffer>> {
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: `raw` is a valid out pointer and the pool is alive. On success
        // CoreVideo returns a +1 retained buffer that `Retained::from_raw` adopts; the
        // buffer goes back to the pool when its last reference is dropped.
        unsafe {
            if CVPixelBufferPool::create_pixel_buffer(None, &self.pool, NonNull::new(&mut raw)?)
                != 0
            {
                return None;
            }
            Retained::from_raw(raw)
        }
    }
}

impl Engine {
    fn new(quality: VNGeneratePersonSegmentationRequestQualityLevel) -> Self {
        // SAFETY: a plain Objective-C constructor of a Vision object that this thread
        // alone owns.
        let handler = unsafe { VNSequenceRequestHandler::new() };
        Self {
            quality,
            handler,
            pool: None,
        }
    }

    /// A new request for every frame: Vision's request is stateful and smooths the mask
    /// over the frames before it, so the mask of a moving hand trails the picture (Mac
    /// test, 2026-11-24). A fresh request makes the mask depend on this frame alone,
    /// which is what the filter's exact mode needs (ADR-0183); the filter smooths itself.
    fn request(&self) -> Retained<VNGeneratePersonSegmentationRequest> {
        // SAFETY: plain Objective-C constructor and setters of a Vision object that this
        // thread alone owns; the arguments are valid values of the declared types.
        unsafe {
            let request = VNGeneratePersonSegmentationRequest::new();
            request.setQualityLevel(self.quality);
            request.setOutputPixelFormat(kCVPixelFormatType_OneComponent8);
            request
        }
    }

    /// Runs one frame and copies the mask out as one byte per pixel.
    fn run(&mut self, pending: &Pending) -> Result<Matte, SegmentError> {
        if self.pool.as_ref().is_none_or(|p| p.size != pending.size) {
            self.pool = InputPool::new(pending.size);
        }
        let pool = self.pool.as_ref().ok_or(SegmentError::DeviceLost)?;
        let buffer = fill_pixel_buffer(pool, pending).ok_or(SegmentError::DeviceLost)?;
        let request = self.request();
        let requests = NSArray::from_retained_slice(&[Retained::into_super(Retained::into_super(
            Retained::into_super(request.clone()),
        ))]);
        let requests: Retained<NSArray<VNRequest>> = requests;
        // SAFETY: `buffer` is a valid pixel buffer for the call, `requests` holds the
        // request owned by this thread, and the handler is used only from this thread.
        let performed = unsafe {
            self.handler
                .performRequests_onCVPixelBuffer_error(&requests, &buffer)
        };
        if performed.is_err() {
            return Err(SegmentError::DeviceLost);
        }
        // SAFETY: `results` is read on the thread that performed the request, after it
        // finished.
        let results = unsafe { request.results() }.ok_or(SegmentError::DeviceLost)?;
        let observation = results.firstObject().ok_or(SegmentError::DeviceLost)?;
        // SAFETY: the observation is a valid object returned by Vision.
        let mask = unsafe { observation.pixelBuffer() };
        copy_mask(&mask, pending.timestamp).ok_or(SegmentError::DeviceLost)
    }
}

/// A pool buffer holding `pending`'s pixels.
fn fill_pixel_buffer(pool: &InputPool, pending: &Pending) -> Option<Retained<CVPixelBuffer>> {
    let width = pending.size.width as usize;
    let height = pending.size.height as usize;
    let buffer = pool.buffer()?;
    // SAFETY: the buffer is locked for writing while its base address is used, and
    // unlocked before it leaves this function. Each row copy stays inside both the
    // source (`width * 4` bytes per row, checked in `to_bgra`) and the buffer row.
    unsafe {
        if CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) != 0 {
            return None;
        }
        let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRow(&buffer);
        let ok = !base.is_null() && stride >= width * 4;
        if ok {
            for row in 0..height {
                let source = &pending.bgra[row * width * 4..(row + 1) * width * 4];
                std::ptr::copy_nonoverlapping(source.as_ptr(), base.add(row * stride), width * 4);
            }
        }
        CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
        ok.then_some(buffer)
    }
}

fn copy_mask(mask: &CVPixelBuffer, timestamp: Timestamp) -> Option<Matte> {
    let width = CVPixelBufferGetWidth(mask);
    let height = CVPixelBufferGetHeight(mask);
    // SAFETY: the buffer is locked read-only while its base address is read, and
    // unlocked before returning. Each row read stays inside `stride` bytes of the row.
    let bytes = unsafe {
        if CVPixelBufferLockBaseAddress(mask, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return None;
        }
        let base = CVPixelBufferGetBaseAddress(mask).cast::<u8>().cast_const();
        let stride = CVPixelBufferGetBytesPerRow(mask);
        let copied = if base.is_null() || stride < width {
            None
        } else {
            let mut out = Vec::with_capacity(width * height);
            for row in 0..height {
                out.extend_from_slice(std::slice::from_raw_parts(base.add(row * stride), width));
            }
            Some(out)
        };
        CVPixelBufferUnlockBaseAddress(mask, CVPixelBufferLockFlags::ReadOnly);
        copied?
    };
    let size = Size::new(u32::try_from(width).ok()?, u32::try_from(height).ok()?);
    Some(Matte::new(
        timestamp,
        size,
        MattePlane::Cpu(Arc::from(bytes)),
    ))
}

fn worker_main(shared: &Shared, quality: VNGeneratePersonSegmentationRequestQualityLevel) {
    let mut engine = Engine::new(quality);
    loop {
        let pending = {
            let Ok(mut mailbox) = shared.mailbox.lock() else {
                return;
            };
            loop {
                if shared.stop.load(Ordering::Acquire) {
                    return;
                }
                if mailbox.reset {
                    mailbox.reset = false;
                    engine = Engine::new(quality);
                }
                if let Some(pending) = mailbox.pending.take() {
                    break pending;
                }
                match shared.wake.wait(mailbox) {
                    Ok(guard) => mailbox = guard,
                    Err(_) => return,
                }
            }
        };
        // A panic in the Vision calls must not leave the session silent: it becomes an
        // error the filter can show (rule 8).
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.run(&pending)))
                .unwrap_or(Err(SegmentError::DeviceLost));
        if let Ok(mut mailbox) = shared.mailbox.lock() {
            if mailbox.free.len() < BUFFERS {
                mailbox.free.push(pending.bgra);
            }
            match outcome {
                Ok(matte) => mailbox.matte = Some(matte),
                Err(error) => mailbox.failed = Some(error),
            }
        }
    }
}
