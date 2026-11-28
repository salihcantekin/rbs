//! Thin wrapper over one `VTDecompressionSession`: the format description made from the
//! stream's parameter sets, the output callback, and one length-prefixed sample per
//! access unit.
//!
//! - Decoding is synchronous (no `kVTDecodeFrame_EnableAsynchronousDecompression`) and
//!   without temporal processing, so VideoToolbox outputs in decode order (which is
//!   presentation order: the live case has no B-frames) and every picture of a sample
//!   has come out when `decode` returns. `kVTDecompressionPropertyKey_RealTime` is set.
//! - The output callback may still run on a VideoToolbox thread: it only retains the
//!   buffer and pushes it onto a bounded queue in [`Shared`]; it never touches the
//!   caller's `Vec`. The session is invalidated before `Shared` is dropped.
//! - Output buffers are IOSurface-backed and Metal-compatible bi-planar 4:2:0
//!   (`'420v'`, `'420f'` for a full-range stream).
//!
//! Low-level module (principle P11): every `unsafe` block states its invariant.

use std::collections::VecDeque;
use std::ffi::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, MutexGuard};

use objc2_core_foundation::{CFBoolean, CFNumber, CFRetained, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, kCMBlockBufferAssureMemoryNowFlag,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession, VTSessionCopyProperty, kVTAllocationFailedErr,
    kVTCouldNotCreateInstanceErr, kVTCouldNotFindVideoDecoderErr,
    kVTDecompressionPropertyKey_RealTime,
    kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder, kVTInvalidSessionErr,
    kVTSessionMalfunctionErr, kVTVideoDecoderMalfunctionErr, kVTVideoDecoderNotAvailableNowErr,
    kVTVideoDecoderRemovedErr, kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder,
    kVTVideoDecoderUnsupportedDataFormatErr,
};
use rbs_codec::VideoDecodeError;

use super::input::{LENGTH_SIZE, MAX_PPS, write_sample};
use crate::mac::cf;

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`.
pub(super) const FOURCC_420V: u32 = u32::from_be_bytes(*b"420v");
/// `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange`.
pub(super) const FOURCC_420F: u32 = u32::from_be_bytes(*b"420f");

/// Pictures the callback queue holds; more than one sample ever produces (with
/// synchronous decode a sample produces at most one), so it never fills; a picture that
/// would overflow it is released and never reaches the worker.
const OUTPUT_QUEUE: usize = 16;

/// One picture (or a failure) from the output callback.
pub(super) struct Output {
    /// Submission index of the sample (its `sourceFrameRefCon`).
    pub(super) index: u64,
    pub(super) status: i32,
    pub(super) dropped: bool,
    pub(super) buffer: Option<CFRetained<CVPixelBuffer>>,
}

// SAFETY: the pixel buffer is a reference-counted CoreVideo object whose retain and
// release are thread-safe; its contents are read only by the worker that owns the
// `Output`.
unsafe impl Send for Output {}

/// State the callback shares with the worker.
struct Shared {
    queue: Mutex<VecDeque<Output>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Shared {
    fn push(&self, output: Output) {
        let mut queue = lock(&self.queue);
        if queue.len() >= OUTPUT_QUEUE {
            return;
        }
        queue.push_back(output);
    }
}

/// The VideoToolbox output callback. `refcon` is the session's `Shared`.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: i32,
    flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: `refcon` is `Arc::as_ptr` of the session's `Shared`, which outlives the
    // decompression session: it is invalidated (no more callbacks) before the last
    // `Arc` is dropped.
    let shared = unsafe { &*refcon.cast::<Shared>() };
    // SAFETY: VideoToolbox passes a valid image buffer or null, valid for this call;
    // retaining it keeps it past the call.
    let buffer = NonNull::new(image).map(|p| unsafe { CFRetained::retain(p) });
    let output = Output {
        index: frame_refcon as usize as u64,
        status,
        dropped: flags.contains(VTDecodeInfoFlags::FrameDropped),
        buffer,
    };
    // Never unwind into VideoToolbox.
    let _ = catch_unwind(AssertUnwindSafe(|| shared.push(output)));
}

/// A failed decode, and whether the VideoToolbox session must be dropped (the next IDR
/// creates a new one).
#[derive(Debug)]
pub(super) struct Failure {
    pub(super) error: VideoDecodeError,
    pub(super) drop_session: bool,
}

impl Failure {
    /// The failure a VideoToolbox status stands for.
    pub(super) fn status(what: &str, status: i32) -> Self {
        Self {
            error: status_error(what, status),
            drop_session: DEVICE_LOST.contains(&status) || NEW_SESSION.contains(&status),
        }
    }
}

impl From<VideoDecodeError> for Failure {
    fn from(error: VideoDecodeError) -> Self {
        let drop_session = matches!(error, VideoDecodeError::DeviceLost);
        Self {
            error,
            drop_session,
        }
    }
}

/// A VideoToolbox status as a port error.
pub(super) fn status_error(what: &str, status: i32) -> VideoDecodeError {
    if DEVICE_LOST.contains(&status) {
        return VideoDecodeError::DeviceLost;
    }
    if REFUSALS.contains(&status) {
        return VideoDecodeError::Unsupported(format!(
            "VideoToolbox refused the stream ({what}: {status})"
        ));
    }
    VideoDecodeError::Failed {
        reason: format!("{what}: OSStatus {status}"),
        recoverable: true,
    }
}

/// Statuses after which the session must be recreated (sleep, GPU change, a crashed
/// decoder service).
const DEVICE_LOST: [i32; 3] = [
    kVTInvalidSessionErr,
    kVTVideoDecoderRemovedErr,
    kVTSessionMalfunctionErr,
];

/// Statuses after which the session is dropped and made again at the next IDR, while
/// the error stays recoverable: a decoder malfunction, an allocation failure, or a
/// hardware decoder busy for now.
const NEW_SESSION: [i32; 3] = [
    kVTVideoDecoderMalfunctionErr,
    kVTAllocationFailedErr,
    kVTVideoDecoderNotAvailableNowErr,
];

/// Statuses with which VideoToolbox refuses the stream itself.
const REFUSALS: [i32; 3] = [
    kVTCouldNotFindVideoDecoderErr,
    kVTVideoDecoderUnsupportedDataFormatErr,
    kVTCouldNotCreateInstanceErr,
];

/// One decompression session and the format description it decodes.
pub(super) struct Session {
    // Field order: the session is invalidated (in `Drop`) before `shared` goes.
    session: CFRetained<VTDecompressionSession>,
    desc: CFRetained<CMFormatDescription>,
    shared: Arc<Shared>,
    hardware: bool,
}

// SAFETY: the session and format description are reference-counted CoreFoundation
// objects without thread affinity; a `Session` is used by one thread at a time (the
// port's worker), and the callback it registers only touches `Shared` (a mutex).
unsafe impl Send for Session {}

/// A format description from one SPS and its PPSs (NAL units without start codes).
fn format_description(
    sps: &[u8],
    pps: &[Option<&[u8]>; MAX_PPS],
) -> Result<CFRetained<CMFormatDescription>, VideoDecodeError> {
    let mut pointers = [NonNull::<u8>::dangling(); 1 + MAX_PPS];
    let mut sizes = [0usize; 1 + MAX_PPS];
    let mut count = 0;
    for set in std::iter::once(sps).chain(pps.iter().flatten().copied()) {
        if set.is_empty() {
            return Err(VideoDecodeError::InvalidInput("empty parameter set".into()));
        }
        // The pointer keeps the provenance of the whole NAL unit.
        pointers[count] = NonNull::from(set).cast::<u8>();
        sizes[count] = set.len();
        count += 1;
    }
    let mut raw: *const CMFormatDescription = std::ptr::null();
    // SAFETY: `count` pointers and sizes describe live NAL units of the packet (each
    // non-empty), valid for the call, which copies them; `raw` is a valid out pointer
    // that receives a +1 reference on success.
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            count,
            NonNull::from(&mut pointers).cast::<NonNull<u8>>(),
            NonNull::from(&mut sizes).cast::<usize>(),
            LENGTH_SIZE as c_int,
            NonNull::from(&mut raw),
        )
    };
    match (status, NonNull::new(raw.cast_mut())) {
        // SAFETY: a successful create hands over a +1 reference.
        (0, Some(raw)) => Ok(unsafe { CFRetained::from_raw(raw) }),
        _ => Err(VideoDecodeError::InvalidInput(format!(
            "VideoToolbox refused the parameter sets ({status})"
        ))),
    }
}

impl Session {
    /// A session for the stream of `sps` and `pps`, writing `fourcc` buffers.
    pub(super) fn new(
        sps: &[u8],
        pps: &[Option<&[u8]>; MAX_PPS],
        fourcc: u32,
    ) -> Result<Self, VideoDecodeError> {
        let desc = format_description(sps, pps)?;
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::with_capacity(OUTPUT_QUEUE)),
        });
        // SAFETY: the keys are CoreFoundation constants that live for the process.
        let spec = unsafe {
            cf::dictionary(
                &[kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder],
                &[CFBoolean::new(true)],
            )
        };
        let surface_props = cf::dictionary(&[], &[]);
        let pixel_format = CFNumber::new_i64(i64::from(fourcc));
        // SAFETY: as above.
        let attributes = unsafe {
            cf::dictionary(
                &[
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                    kCVPixelBufferMetalCompatibilityKey,
                ],
                &[&pixel_format, &surface_props, CFBoolean::new(true)],
            )
        };
        let callback = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(output_callback),
            decompressionOutputRefCon: Arc::as_ptr(&shared).cast_mut().cast(),
        };
        let mut raw: *mut VTDecompressionSession = std::ptr::null_mut();
        // SAFETY: the description and dictionaries are valid for the call; the callback
        // record is copied, and its refcon stays valid for the session's life (see
        // `output_callback`); `raw` is a valid out pointer that receives a +1 reference
        // on success.
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &desc,
                Some(spec.as_opaque()),
                Some(attributes.as_opaque()),
                &callback,
                NonNull::from(&mut raw),
            )
        };
        let (0, Some(raw)) = (status, NonNull::new(raw)) else {
            return Err(status_error("VTDecompressionSessionCreate", status));
        };
        // SAFETY: a successful create hands over a +1 reference.
        let session: CFRetained<VTDecompressionSession> = unsafe { CFRetained::from_raw(raw) };
        // SAFETY: the key is a CoreFoundation constant. A decoder without real-time
        // mode still decodes; the property is a hint.
        let _ = unsafe { cf::set_bool(&*session, kVTDecompressionPropertyKey_RealTime, true) };
        let hardware = using_hardware(&session);
        Ok(Self {
            session,
            desc,
            shared,
            hardware,
        })
    }

    /// True when VideoToolbox picked a hardware decoder.
    pub(super) fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// Decodes the slices and SEI of `data` (`sample_len` bytes once length-prefixed)
    /// as the sample of submission `index`. Its pictures are in the output queue when
    /// this returns.
    pub(super) fn decode(&self, data: &[u8], sample_len: usize, index: u64) -> Result<(), Failure> {
        let sample = self.sample(data, sample_len)?;
        let mut info = VTDecodeInfoFlags::empty();
        // SAFETY: a live session and sample buffer; the refcon is a plain integer the
        // callback reads back; `info` is a valid out pointer.
        let status = unsafe {
            self.session.decode_frame(
                &sample,
                VTDecodeFrameFlags::empty(),
                index as usize as *mut c_void,
                &mut info,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(Failure::status("decode", status))
        }
    }

    /// A sample buffer holding the length-prefixed NAL units of `data`, in memory
    /// CoreMedia owns (VideoToolbox may keep the sample after the call).
    fn sample(
        &self,
        data: &[u8],
        len: usize,
    ) -> Result<CFRetained<CMSampleBuffer>, VideoDecodeError> {
        let failed = |what: &str, status: i32| VideoDecodeError::Failed {
            reason: format!("{what}: OSStatus {status}"),
            recoverable: true,
        };
        let mut raw: *mut CMBlockBuffer = std::ptr::null_mut();
        // SAFETY: a null memory block with `kCMBlockBufferAssureMemoryNowFlag` makes
        // CoreMedia allocate `len` bytes now; `raw` is a valid out pointer that receives
        // a +1 reference on success.
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                std::ptr::null_mut(),
                len,
                None,
                std::ptr::null(),
                0,
                len,
                kCMBlockBufferAssureMemoryNowFlag,
                NonNull::from(&mut raw),
            )
        };
        let (0, Some(raw)) = (status, NonNull::new(raw)) else {
            return Err(failed("block buffer", status));
        };
        // SAFETY: a successful create hands over a +1 reference.
        let block: CFRetained<CMBlockBuffer> = unsafe { CFRetained::from_raw(raw) };
        let mut contiguous = 0usize;
        let mut ptr: *mut c_char = std::ptr::null_mut();
        // SAFETY: valid out pointers on a live block buffer.
        let status =
            unsafe { block.data_pointer(0, &mut contiguous, std::ptr::null_mut(), &mut ptr) };
        if status != 0 || ptr.is_null() || contiguous != len {
            return Err(failed("block buffer memory", status));
        }
        // SAFETY: the block buffer owns `len` contiguous writable bytes at `ptr` (one
        // memory block it allocated), nobody else references it yet, and the slice is
        // dropped before the buffer is shared.
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), len) };
        if !write_sample(data, dst) {
            return Err(VideoDecodeError::InvalidInput("sample length".into()));
        }
        let mut raw: *mut CMSampleBuffer = std::ptr::null_mut();
        // SAFETY: a live block buffer and format description; one sample of `len`
        // bytes without timing; `raw` is a valid out pointer that receives a +1
        // reference on success.
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(&self.desc),
                1,
                0,
                std::ptr::null(),
                1,
                &len,
                NonNull::from(&mut raw),
            )
        };
        let (0, Some(raw)) = (status, NonNull::new(raw)) else {
            return Err(failed("sample buffer", status));
        };
        // SAFETY: a successful create hands over a +1 reference.
        Ok(unsafe { CFRetained::from_raw(raw) })
    }

    /// Moves the callback's outputs to `out`.
    pub(super) fn take(&self, out: &mut VecDeque<Output>) {
        out.append(&mut lock(&self.shared.queue));
    }

    /// Emits any delayed picture and waits for asynchronous ones (none in the
    /// synchronous mode used, so this returns at once).
    pub(super) fn finish(&self) {
        // SAFETY: plain calls on a live session.
        unsafe {
            let _ = self.session.finish_delayed_frames();
            let _ = self.session.wait_for_asynchronous_frames();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session is valid. Waiting delivers every pending picture;
        // invalidating stops all callbacks before `shared` can be dropped.
        unsafe {
            let _ = self.session.wait_for_asynchronous_frames();
            self.session.invalidate();
        }
    }
}

/// `kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder`; false when
/// unknown.
fn using_hardware(session: &VTDecompressionSession) -> bool {
    let mut value: *const CFType = std::ptr::null();
    // SAFETY: the key is a CoreFoundation constant; `value` is a valid out pointer
    // that receives a +1 reference (the "Copy" rule) on success.
    let status = unsafe {
        VTSessionCopyProperty(
            session,
            kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
            None,
            (&mut value as *mut *const CFType).cast(),
        )
    };
    let Some(raw) = NonNull::new(value.cast_mut()) else {
        return false;
    };
    // SAFETY: a non-null result of a Copy function is an owned reference.
    let value: CFRetained<CFType> = unsafe { CFRetained::from_raw(raw) };
    status == 0
        && value
            .downcast_ref::<CFBoolean>()
            .is_some_and(CFBoolean::as_bool)
}
