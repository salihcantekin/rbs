//! One VideoToolbox compression session behind the encoder port.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime, CMTimeFlags,
    kCMSampleAttachmentKey_NotSync, kCMTimeInvalid, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    CVAttachmentMode, CVPixelBuffer, CVPixelBufferCreateWithIOSurface,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferPool, CVPixelBufferUnlockBaseAddress, kCVImageBufferColorPrimaries_EBU_3213,
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferColorPrimaries_ITU_R_2020,
    kCVImageBufferColorPrimaries_P3_D65, kCVImageBufferColorPrimaries_SMPTE_C,
    kCVImageBufferColorPrimariesKey, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferTransferFunction_ITU_R_2020, kCVImageBufferTransferFunction_ITU_R_2100_HLG,
    kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ, kCVImageBufferTransferFunction_sRGB,
    kCVImageBufferTransferFunctionKey, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_2020,
    kCVImageBufferYCbCrMatrixKey, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVReturnWouldExceedAllocationThreshold,
};
use objc2_io_surface::IOSurfaceRef;
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_ColorPrimaries,
    kVTCompressionPropertyKey_ConstantBitRate, kVTCompressionPropertyKey_DataRateLimits,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_Quality,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTCouldNotFindVideoEncoderErr,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTInvalidSessionErr,
    kVTProfileLevel_H264_Baseline_AutoLevel, kVTProfileLevel_H264_High_AutoLevel,
    kVTProfileLevel_H264_Main_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTProfileLevel_HEVC_Main10_AutoLevel, kVTPropertyNotSupportedErr,
    kVTVideoEncoderMalfunctionErr,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};
use rbs_codec::{
    CreateError, EncodeError, EncoderChange, EncoderPreset, RateControl, VideoEncoder,
    VideoEncoderConfig,
};
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, ColorPrimaries, ColorRange, EncodedPacket, FramePayload,
    Lease, MatrixCoefficients, MemoryKind, NativeSurface, PixelFormat, StreamDescriptor,
    StreamParams, SurfaceSync, TransferFunction, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;

use super::cf::{self, Status};
use crate::{FRAMES_IN_FLIGHT, SURFACE_CACHE, SessionStats};

/// Timescale of every `CMTime` the session uses: nanoseconds, like master time.
const TIMESCALE: i32 = 1_000_000_000;

/// Failure kinds stored in [`Shared::failure`] (upper 32 bits; the lower 32 bits hold
/// the OSStatus for [`FAIL_STATUS`]).
const FAIL_NONE: u32 = 0;
/// VideoToolbox reported an error status.
const FAIL_STATUS: u32 = 1;
/// The parameter sets of a keyframe differ from the extradata in `stream()`.
const FAIL_PARAMETER_SETS: u32 = 2;
/// A packet could not be built (no data, bad timestamps).
const FAIL_PACKET: u32 = 3;
/// The output callback panicked.
const FAIL_PANIC: u32 = 4;

/// `'420v'`, `'420f'`, `'x420'`, `'xf20'`: the bi-planar CoreVideo formats.
const FOURCC_NV12_VIDEO: u32 = u32::from_be_bytes(*b"420v");
const FOURCC_NV12_FULL: u32 = u32::from_be_bytes(*b"420f");
const FOURCC_P010_VIDEO: u32 = u32::from_be_bytes(*b"x420");
const FOURCC_P010_FULL: u32 = u32::from_be_bytes(*b"xf20");

/// Slot states. `FREE`: owned by the encoder thread. `IN_FLIGHT`: handed to
/// VideoToolbox. `COMPLETING`: claimed by whoever won the exchange from `IN_FLIGHT`
/// (the output callback, or `submit` after a refused frame). `DONE`: the outcome is
/// written and owned by the encoder thread again.
const FREE: u8 = 0;
const IN_FLIGHT: u8 = 1;
const COMPLETING: u8 = 2;
const DONE: u8 = 3;

/// What became of a frame.
enum Outcome {
    /// Nothing yet, or already taken by `drain`.
    Empty,
    /// The encoded packet.
    Packet(EncodedPacket),
    /// VideoToolbox dropped the frame, or failed it (the failure is recorded).
    Lost,
}

/// One frame VideoToolbox may still be reading: its lease while in flight, its
/// outcome once done. Packets stay in their frame's slot until `drain` takes them in
/// submission order, so the output order never depends on which thread or in which
/// order VideoToolbox runs its callbacks, and no lock is needed.
struct Slot {
    state: AtomicU8,
    /// Bumped by `submit` each time the slot is reused; part of the frame refcon, so a
    /// late or repeated callback for an earlier use cannot complete the current one.
    generation: AtomicU32,
    lease: UnsafeCell<Option<Lease>>,
    outcome: UnsafeCell<Outcome>,
}

/// A retained CoreFoundation object that is only read after creation.
struct Frozen<T: ?Sized>(CFRetained<T>);

// SAFETY: format descriptions and strings are immutable CoreFoundation objects;
// retain and release are atomic, and these are only read.
unsafe impl<T: ?Sized> Send for Frozen<T> {}
// SAFETY: see `Send`.
unsafe impl<T: ?Sized> Sync for Frozen<T> {}

/// Set once by `create` (the priming frame), read by the callback afterwards.
struct Primed {
    desc: Frozen<CMFormatDescription>,
    record: Vec<u8>,
}

/// State shared with the output callback, which VideoToolbox calls on its own threads.
struct Shared {
    codec: CodecId,
    bitstream: BitstreamFormat,
    frame_duration: Duration,
    slots: Box<[Slot]>,
    /// First failure: kind in the upper 32 bits, OSStatus in the lower.
    failure: AtomicU64,
    /// Master time of VideoToolbox time 0, set before the first real frame.
    base_ns: AtomicI64,
    /// The decoder configuration atom name (`avcC` or `hvcC`), created once.
    atom: Frozen<CFString>,
    /// The priming frame's description; its record is the extradata.
    primed: OnceLock<Primed>,
}

// Leases cross to the callback thread.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Lease>();
    assert_send::<EncodedPacket>();
};

// SAFETY: a slot's `lease` and `outcome` are accessed only by the slot's owner:
// the encoder thread while the state is `FREE` or `DONE`, and the single winner of the
// `IN_FLIGHT -> COMPLETING` exchange while it is `COMPLETING`. Each hand-over is a
// `Release` store or exchange paired with the next owner's `Acquire` load or exchange,
// so writes before the hand-over are visible after it. `Lease` and `EncodedPacket`
// are `Send` (checked above). Everything else is atomics, a `OnceLock` of a `Sync`
// type, or immutable data.
unsafe impl Sync for Shared {}

impl Shared {
    fn fail(&self, kind: u32, status: Status) {
        let value = (u64::from(kind) << 32) | u64::from(status as u32);
        let _ = self
            .failure
            .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire);
    }

    fn failure(&self) -> Option<(u32, Status)> {
        let v = self.failure.load(Ordering::Acquire);
        let kind = (v >> 32) as u32;
        (kind != FAIL_NONE).then_some((kind, v as u32 as i32))
    }

    /// Claims slot `index` from `IN_FLIGHT` if it is still in use `generation`.
    /// Exactly one caller wins per use; the winner owns the slot until it stores
    /// `DONE` or `FREE`.
    fn claim(&self, index: usize, generation: u32) -> Option<&Slot> {
        let slot = self.slots.get(index)?;
        if slot.generation.load(Ordering::Acquire) != generation {
            return None;
        }
        slot.state
            .compare_exchange(IN_FLIGHT, COMPLETING, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| slot)
    }

    /// The output callback's work. Never panics on purpose; the caller contains it.
    fn on_output(
        &self,
        frame_refcon: usize,
        status: Status,
        flags: VTEncodeInfoFlags,
        sample: Option<&CMSampleBuffer>,
    ) {
        if frame_refcon == 0 {
            self.on_priming(status, sample);
            return;
        }
        let index = (frame_refcon & 0xff) - 1;
        let generation = (frame_refcon >> 8) as u32;
        let Some(slot) = self.claim(index, generation) else {
            return;
        };
        // VideoToolbox no longer reads the source frame: hand the surface back.
        // SAFETY: this caller won the claim and owns the slot (see `Sync`).
        drop(unsafe { (*slot.lease.get()).take() });
        let outcome = if status != 0 {
            self.fail(FAIL_STATUS, status);
            Outcome::Lost
        } else {
            match sample.filter(|_| !flags.contains(VTEncodeInfoFlags::FrameDropped)) {
                Some(sample) => match self.packet(sample) {
                    Some(packet) => Outcome::Packet(packet),
                    None => {
                        self.fail(FAIL_PACKET, 0);
                        Outcome::Lost
                    }
                },
                None => Outcome::Lost,
            }
        };
        // SAFETY: as above.
        unsafe { *slot.outcome.get() = outcome };
        slot.state.store(DONE, Ordering::Release);
    }

    fn on_priming(&self, status: Status, sample: Option<&CMSampleBuffer>) {
        if status != 0 {
            self.fail(FAIL_STATUS, status);
            return;
        }
        // SAFETY: `s` is a valid sample buffer for the callback's duration.
        let Some(desc) = sample.and_then(|s| unsafe { s.format_description() }) else {
            return;
        };
        if let Some(record) = cf::decoder_config_record(&desc, &self.atom.0) {
            let _ = self.primed.set(Primed {
                desc: Frozen(desc),
                record,
            });
        }
    }

    fn packet(&self, sample: &CMSampleBuffer) -> Option<EncodedPacket> {
        // SAFETY: `sample` is valid for the callback; these getters only read it.
        let (pts, dts, block, desc) = unsafe {
            (
                sample.presentation_time_stamp(),
                sample.decode_time_stamp(),
                sample.data_buffer(),
                sample.format_description(),
            )
        };
        let keyframe = is_sync_sample(sample);
        if keyframe && !self.same_parameter_sets(desc.as_deref()) {
            self.fail(FAIL_PARAMETER_SETS, 0);
            return None;
        }
        let base = self.base_ns.load(Ordering::Acquire);
        let pts = master_time(pts, base)?;
        let dts = master_time(dts, base).unwrap_or(pts).min(pts);
        let data = copy_block(&*block?)?;
        EncodedPacket::new(self.codec, self.bitstream, pts, dts, keyframe, data)
            .ok()
            .map(|p| p.with_duration(self.frame_duration))
    }

    /// True when `desc` carries the same decoder configuration record as the primed
    /// description. Only the record is compared: other extensions (colour, chroma
    /// location, aspect) may legitimately differ from the priming frame's.
    fn same_parameter_sets(&self, desc: Option<&CMFormatDescription>) -> bool {
        let (Some(desc), Some(primed)) = (desc, self.primed.get()) else {
            return false;
        };
        std::ptr::eq(desc, &*primed.desc.0)
            || cf::decoder_config_matches(desc, &self.atom.0, &primed.record)
    }
}

/// True unless the sample is marked `NotSync`.
fn is_sync_sample(sample: &CMSampleBuffer) -> bool {
    // SAFETY: `sample` is valid; the attachments array is owned by it and not mutated
    // while we read it on this thread.
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
        return true;
    };
    // SAFETY: sample attachments are an array of dictionaries with CFString keys.
    let attachments: &objc2_core_foundation::CFArray<CFDictionary<CFString, CFType>> =
        unsafe { attachments.cast_unchecked() };
    let Some(first) = attachments.get(0) else {
        return true;
    };
    // SAFETY: the key is a CoreFoundation constant that lives for the process.
    let key = unsafe { kCMSampleAttachmentKey_NotSync };
    // SAFETY: the dictionary is not mutated while this reference lives.
    match unsafe { first.get_unchecked(key) } {
        Some(value) => !value
            .downcast_ref::<CFBoolean>()
            .is_some_and(CFBoolean::as_bool),
        None => true,
    }
}

/// Copies a (possibly non-contiguous) block buffer into one exact-size payload: the one
/// allocation per packet the port allows.
fn copy_block(block: &CMBlockBuffer) -> Option<Bytes> {
    // SAFETY: `block` is a valid block buffer.
    let len = unsafe { block.data_length() };
    if len == 0 {
        return None;
    }
    let mut data = Vec::<u8>::with_capacity(len);
    let dest = NonNull::new(data.as_mut_ptr().cast::<c_void>())?;
    // SAFETY: `dest` has room for `len` bytes and `0..len` is within the buffer.
    let status = unsafe { block.copy_data_bytes(0, len, dest) };
    if status != 0 {
        return None;
    }
    // SAFETY: `copy_data_bytes` initialised all `len` bytes.
    unsafe { data.set_len(len) };
    Some(Bytes::from(data))
}

/// Converts a VideoToolbox time back to master time (`base` is the master time of
/// VideoToolbox time 0). `None` for invalid times.
fn master_time(t: CMTime, base: i64) -> Option<Timestamp> {
    if !t.flags.contains(CMTimeFlags::Valid) || t.timescale <= 0 {
        return None;
    }
    let ns = if t.timescale == TIMESCALE {
        i128::from(t.value)
    } else {
        i128::from(t.value) * 1_000_000_000 / i128::from(t.timescale)
    };
    // A decode time before master time 0 clamps to zero; `dts <= pts` still holds.
    let ns = (ns + i128::from(base)).max(0);
    u64::try_from(ns).ok().map(Timestamp::from_nanos)
}

fn vt_time(ns: u64) -> CMTime {
    CMTime {
        value: i64::try_from(ns).unwrap_or(i64::MAX),
        timescale: TIMESCALE,
        flags: CMTimeFlags::Valid,
        epoch: 0,
    }
}

/// The VideoToolbox output callback. `refcon` is the session's `Shared`.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: Status,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: `refcon` is `Arc::as_ptr` of the session's `Shared`, which outlives the
    // compression session: the session is invalidated (no more callbacks) before the
    // last `Arc` is dropped.
    let shared = unsafe { &*refcon.cast::<Shared>() };
    // SAFETY: VideoToolbox passes a valid sample buffer or null, valid for this call.
    let sample = unsafe { sample.as_ref() };
    // Never unwind into VideoToolbox.
    if catch_unwind(AssertUnwindSafe(|| {
        shared.on_output(frame_refcon as usize, status, flags, sample)
    }))
    .is_err()
    {
        shared.fail(FAIL_PANIC, 0);
    }
}

/// The compression session, invalidated on drop (no callbacks after that).
struct Handle(CFRetained<VTCompressionSession>);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the session is valid. Completing first delivers every pending frame
        // (and releases its lease); invalidating stops all callbacks before `Shared`
        // can be dropped.
        unsafe {
            let _ = self.0.complete_frames(kCMTimeInvalid);
            self.0.invalidate();
        }
    }
}

/// An IOSurface wrapped once as a pixel buffer.
struct CacheEntry {
    surface: usize,
    buffer: CFRetained<CVPixelBuffer>,
}

/// One VideoToolbox encoder session. Created by [`super::VideoToolboxFactory`].
pub struct VideoToolboxSession {
    // Field order matters: the handle is dropped (session invalidated) before `shared`.
    handle: Handle,
    shared: Arc<Shared>,
    /// Slots in submission order: `(index, forced keyframe)`. A fixed ring, so
    /// `submit` and `drain` never allocate.
    order: [(u8, bool); FRAMES_IN_FLIGHT],
    order_head: usize,
    order_len: usize,
    /// Master time of the first real frame, once known.
    first_ns: Option<u64>,
    stream: StreamDescriptor,
    format: VideoFormat,
    memory: MemoryKind,
    fourcc: u32,
    rate_control: RateControl,
    /// Colour attachments for source buffers (`ColorSpace` as CoreVideo strings).
    color: [(&'static CFString, &'static CFString); 3],
    /// Whether CBR uses `ConstantBitRate` (true) or an average plus a 1 s cap.
    native_cbr: bool,
    frame_duration: CMTime,
    force_keyframe: CFRetained<CFDictionary<CFString, CFType>>,
    keyframe_next: bool,
    last_pts: Option<u64>,
    cache: Vec<CacheEntry>,
    cache_clock: usize,
    submitted: u64,
    packets: u64,
    dropped: u64,
    copied: u64,
    wrapped: u64,
}

// SAFETY: the session is driven by one thread at a time (port rule). VideoToolbox
// sessions, pixel buffers and dictionaries may be used from any thread; the retained
// CoreFoundation objects are only touched through `&mut self`.
unsafe impl Send for VideoToolboxSession {}

fn fourcc_of(format: &VideoFormat) -> Option<u32> {
    let full = format.color.range == ColorRange::Full;
    match (format.pixel_format, full) {
        (PixelFormat::Nv12, false) => Some(FOURCC_NV12_VIDEO),
        (PixelFormat::Nv12, true) => Some(FOURCC_NV12_FULL),
        (PixelFormat::P010, false) => Some(FOURCC_P010_VIDEO),
        (PixelFormat::P010, true) => Some(FOURCC_P010_FULL),
        _ => None,
    }
}

fn unsupported(what: impl Into<String>) -> CreateError {
    CreateError::Unsupported { what: what.into() }
}

fn device(what: &str, status: Status) -> CreateError {
    CreateError::Device(format!("{what} failed with OSStatus {status}"))
}

/// The error for a recorded failure (`kind`, `status`). Every recorded failure ends
/// the session.
fn failure_error(kind: u32, status: Status) -> EncodeError {
    let reason = match kind {
        FAIL_PARAMETER_SETS => "VideoToolbox changed the parameter sets mid-session",
        FAIL_PACKET => "VideoToolbox returned a packet without data or timestamps",
        FAIL_PANIC => "the VideoToolbox output callback panicked",
        _ => return status_error(status, false),
    };
    EncodeError::Failed {
        reason: reason.into(),
        recoverable: false,
    }
}

/// The error for an OSStatus. A lost or broken session is `DeviceLost`.
fn status_error(status: Status, recoverable: bool) -> EncodeError {
    if status == kVTInvalidSessionErr || status == kVTVideoEncoderMalfunctionErr {
        return EncodeError::DeviceLost;
    }
    EncodeError::Failed {
        reason: format!("VideoToolbox failed with OSStatus {status}"),
        recoverable,
    }
}

fn check_rate_control(rc: &RateControl) -> Result<(), String> {
    match *rc {
        RateControl::Cbr { bitrate_bps: 0 } => Err("zero bitrate".into()),
        RateControl::Vbr {
            target_bps,
            max_bps,
        } if target_bps == 0 || max_bps < target_bps => Err("VBR needs 0 < target <= max".into()),
        RateControl::ConstantQuality { level } if level > 51 => {
            Err("quality level above 51".into())
        }
        _ => Ok(()),
    }
}

impl VideoToolboxSession {
    pub(super) fn new(
        config: &VideoEncoderConfig,
        hardware_only: bool,
    ) -> Result<Self, CreateError> {
        let format = config.format;
        let codec_type = match config.codec {
            CodecId::H264 => kCMVideoCodecType_H264,
            CodecId::Hevc => kCMVideoCodecType_HEVC,
            other => return Err(unsupported(format!("codec {other}"))),
        };
        let fourcc = fourcc_of(&format)
            .ok_or_else(|| unsupported(format!("pixel format {}", format.pixel_format)))?;
        if format.pixel_format == PixelFormat::P010 && config.codec != CodecId::Hevc {
            return Err(unsupported("10-bit input needs HEVC"));
        }
        if !matches!(config.memory, MemoryKind::Metal | MemoryKind::System) {
            return Err(unsupported(format!("input memory {}", config.memory)));
        }
        let (w, h) = (format.size.width, format.size.height);
        if !(super::MIN_SIDE..=super::MAX_WIDTH).contains(&i64::from(w))
            || !(super::MIN_SIDE..=super::MAX_HEIGHT).contains(&i64::from(h))
            || w % 2 != 0
            || h % 2 != 0
        {
            return Err(unsupported(format!("picture size {w}x{h}")));
        }
        check_rate_control(&config.rate_control).map_err(CreateError::InvalidConfig)?;
        let profile = profile_level(config)?;
        let frame_duration = config.frame_rate.frame_duration();
        if frame_duration.is_zero() {
            return Err(CreateError::InvalidConfig("frame rate".into()));
        }

        if matches!(config.rate_control, RateControl::ConstantQuality { .. })
            && !(hardware_only && super::CONSTANT_QUALITY)
        {
            return Err(unsupported(
                "constant quality needs an Apple silicon hardware encoder",
            ));
        }
        let atom = if config.codec == CodecId::H264 {
            "avcC"
        } else {
            "hvcC"
        };
        let shared = Arc::new(Shared {
            codec: config.codec,
            bitstream: BitstreamFormat::LengthPrefixed { length_size: 4 },
            frame_duration,
            slots: (0..FRAMES_IN_FLIGHT)
                .map(|_| Slot {
                    state: AtomicU8::new(FREE),
                    generation: AtomicU32::new(0),
                    lease: UnsafeCell::new(None),
                    outcome: UnsafeCell::new(Outcome::Empty),
                })
                .collect(),
            failure: AtomicU64::new(0),
            base_ns: AtomicI64::new(0),
            atom: Frozen(CFString::from_str(atom)),
            primed: OnceLock::new(),
        });

        // SAFETY: the keys are CoreFoundation constants that live for the process.
        let spec = unsafe {
            cf::dictionary(
                &[
                    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
                    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                ],
                &[CFBoolean::new(true), CFBoolean::new(hardware_only)],
            )
        };
        let surface_props = cf::dictionary(&[], &[]);
        let pixel_format = objc2_core_foundation::CFNumber::new_i64(i64::from(fourcc));
        let width = objc2_core_foundation::CFNumber::new_i64(i64::from(w));
        let height = objc2_core_foundation::CFNumber::new_i64(i64::from(h));
        // SAFETY: as above.
        let attributes = unsafe {
            cf::dictionary(
                &[
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferWidthKey,
                    kCVPixelBufferHeightKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                ],
                &[&pixel_format, &width, &height, &surface_props],
            )
        };
        let mut raw: *mut VTCompressionSession = std::ptr::null_mut();
        // SAFETY: the dictionaries are valid for the call; the callback and its refcon
        // stay valid for the session's life (see `output_callback`); `raw` is a valid out
        // pointer that receives a +1 reference on success.
        let status = unsafe {
            VTCompressionSession::create(
                None,
                w as i32,
                h as i32,
                codec_type,
                Some(spec.as_opaque()),
                Some(attributes.as_opaque()),
                None,
                Some(output_callback),
                Arc::as_ptr(&shared).cast_mut().cast(),
                NonNull::from(&mut raw),
            )
        };
        let raw = NonNull::new(raw);
        if status == kVTCouldNotFindVideoEncoderErr {
            return Err(unsupported(format!(
                "no VideoToolbox {} encoder for this configuration",
                config.codec
            )));
        }
        let (0, Some(raw)) = (status, raw) else {
            return Err(device("VTCompressionSessionCreate", status));
        };
        // SAFETY: a successful create hands over a +1 reference.
        let handle = Handle(unsafe { CFRetained::from_raw(raw) });
        let session = &*handle.0;

        // SAFETY: every key below is a CoreFoundation constant that lives for the process.
        unsafe {
            cf::set_bool(session, kVTCompressionPropertyKey_RealTime, true)
                .map_err(|s| device("RealTime", s))?;
            cf::set_property(session, kVTCompressionPropertyKey_ProfileLevel, profile)
                .map_err(|s| device("ProfileLevel", s))?;
            cf::set_bool(
                session,
                kVTCompressionPropertyKey_AllowFrameReordering,
                false,
            )
            .map_err(|s| device("AllowFrameReordering", s))?;
            let interval = config.keyframe_interval.frames_at(config.frame_rate);
            cf::set_i64(
                session,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                i64::from(interval),
            )
            .map_err(|s| device("MaxKeyFrameInterval", s))?;
            cf::set_f64(
                session,
                kVTCompressionPropertyKey_ExpectedFrameRate,
                config.frame_rate.per_second().as_f64(),
            )
            .map_err(|s| device("ExpectedFrameRate", s))?;
            if config.tuning.preset == EncoderPreset::Speed {
                // Optional: older systems and some encoders do not know it.
                let _ = cf::set_bool(
                    session,
                    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                    true,
                );
            }
        }
        // SAFETY: the session is valid.
        let color = unsafe { set_color(session, &format)? };
        let native_cbr =
            apply_rate_control(session, &config.rate_control, None).map_err(|s| match s {
                s if s == kVTPropertyNotSupportedErr => unsupported(format!(
                    "rate control {} on this encoder",
                    config.rate_control.name()
                )),
                s => device("rate control", s),
            })?;
        // SAFETY: the session is valid.
        let status = unsafe { session.prepare_to_encode_frames() };
        if status != 0 {
            return Err(device("PrepareToEncodeFrames", status));
        }

        // SAFETY: the keys are CoreFoundation constants that live for the process.
        let force_keyframe = unsafe {
            cf::dictionary(
                &[kVTEncodeFrameOptionKey_ForceKeyFrame],
                &[CFBoolean::new(true)],
            )
        };
        let extradata = prime(&handle, &shared, &format, frame_duration, config.codec)?;
        let stream = StreamDescriptor::new(
            config.codec,
            shared.bitstream,
            Bytes::from(extradata),
            StreamParams::video(format, config.frame_rate),
        );
        Ok(Self {
            handle,
            shared,
            order: [(0, false); FRAMES_IN_FLIGHT],
            order_head: 0,
            order_len: 0,
            first_ns: None,
            stream,
            format,
            memory: config.memory,
            fourcc,
            rate_control: config.rate_control,
            color,
            native_cbr,
            frame_duration: vt_time(u64::try_from(frame_duration.as_nanos()).unwrap_or(u64::MAX)),
            force_keyframe,
            keyframe_next: true,
            last_pts: None,
            cache: Vec::with_capacity(SURFACE_CACHE),
            cache_clock: 0,
            submitted: 0,
            packets: 0,
            dropped: 0,
            copied: 0,
            wrapped: 0,
        })
    }

    /// The session's counters.
    pub fn stats(&self) -> SessionStats {
        SessionStats {
            submitted: self.submitted,
            packets: self.packets,
            dropped: self.dropped,
            copied: self.copied,
            wrapped: self.wrapped,
        }
    }

    fn failure(&self) -> Result<(), EncodeError> {
        match self.shared.failure() {
            None => Ok(()),
            Some((kind, status)) => Err(failure_error(kind, status)),
        }
    }

    /// State of the oldest frame in flight, if any.
    fn head_state(&self) -> Option<u8> {
        (self.order_len > 0).then(|| {
            let (index, _) = self.order[self.order_head];
            self.shared.slots[usize::from(index)]
                .state
                .load(Ordering::Acquire)
        })
    }

    /// `Busy` when every slot is in flight or waiting for `drain`. Room only ever comes
    /// from `drain`, so there is nothing to wait for here.
    fn check_room(&self) -> Result<(), EncodeError> {
        if self.order_len < FRAMES_IN_FLIGHT {
            Ok(())
        } else {
            Err(EncodeError::Busy)
        }
    }

    /// Attaches the session's colour description to a source buffer, so VideoToolbox
    /// reads the pixels in the colour space the bitstream signals.
    fn attach_color(&self, buffer: &CVPixelBuffer) {
        for (key, value) in self.color {
            // SAFETY: the buffer is valid; key and value are CoreVideo string constants
            // of the documented types.
            unsafe { buffer.set_attachment(key, value, CVAttachmentMode::ShouldPropagate) };
        }
    }

    /// The pixel buffer wrapping `surface`, from the cache or newly wrapped.
    fn wrap_surface(&mut self, surface: usize) -> Result<CFRetained<CVPixelBuffer>, EncodeError> {
        self.cache_clock = self.cache_clock.wrapping_add(1);
        if let Some(entry) = self.cache.iter().find(|e| e.surface == surface) {
            return Ok(entry.buffer.clone());
        }
        let ptr = NonNull::new(surface as *mut IOSurfaceRef)
            .ok_or_else(|| EncodeError::InvalidInput("null IOSurface".into()))?;
        // SAFETY: `VideoFrame::native`'s contract: the value is a live IOSurfaceRef for as
        // long as the frame's lease, which the caller holds during this call.
        let io = unsafe { ptr.as_ref() };
        if io.pixel_format() != self.fourcc
            || io.width() != self.format.size.width as usize
            || io.height() != self.format.size.height as usize
        {
            return Err(EncodeError::InvalidInput(
                "IOSurface format or size does not match the session".into(),
            ));
        }
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: `io` is valid; the pixel buffer retains the surface; `raw` receives a
        // +1 reference on success.
        let status =
            unsafe { CVPixelBufferCreateWithIOSurface(None, io, None, NonNull::from(&mut raw)) };
        let (0, Some(raw)) = (status, NonNull::new(raw)) else {
            return Err(EncodeError::Failed {
                reason: format!("CVPixelBufferCreateWithIOSurface failed with {status}"),
                recoverable: true,
            });
        };
        // SAFETY: a successful create hands over a +1 reference.
        let buffer = unsafe { CFRetained::from_raw(raw) };
        self.attach_color(&buffer);
        self.wrapped += 1;
        let entry = CacheEntry {
            surface,
            buffer: buffer.clone(),
        };
        if self.cache.len() < SURFACE_CACHE {
            self.cache.push(entry);
        } else {
            // A ring larger than the cache: replace round-robin. Never allocates.
            let index = self.cache_clock % SURFACE_CACHE;
            self.cache[index] = entry;
        }
        Ok(buffer)
    }

    /// A pool pixel buffer holding a copy of a system-memory frame.
    fn copy_system(
        &mut self,
        frame: &VideoFrame,
    ) -> Result<CFRetained<CVPixelBuffer>, EncodeError> {
        let FramePayload::System(system) = frame.payload() else {
            return Err(EncodeError::InvalidInput(
                "frame is not in system memory".into(),
            ));
        };
        let buffer = pool_buffer(&self.handle.0)?;
        let planes = self.format.pixel_format.plane_count();
        let width = self.format.size.width;
        let mut row_bytes = [0usize; 2];
        for (plane, row) in row_bytes.iter_mut().enumerate().take(planes) {
            *row = self
                .format
                .pixel_format
                .min_row_bytes(plane, width)
                .and_then(|r| usize::try_from(r).ok())
                .ok_or_else(|| EncodeError::InvalidInput("plane layout".into()))?;
        }
        with_locked(&buffer, |base| {
            for (plane, row) in row_bytes.iter().enumerate().take(planes) {
                let Some((src, src_stride)) = system.plane(plane) else {
                    return false;
                };
                let (dst, dst_stride, rows) = base(plane);
                if dst.is_null() || dst_stride < *row || src_stride < *row {
                    return false;
                }
                for y in 0..rows {
                    let Some(src_row) = src.get(y * src_stride..y * src_stride + row) else {
                        return false;
                    };
                    // SAFETY: the destination plane has `rows` rows of `dst_stride >= row`
                    // bytes while the buffer is locked; source and destination never
                    // overlap (different allocations).
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            src_row.as_ptr(),
                            dst.add(y * dst_stride),
                            *row,
                        )
                    };
                }
            }
            true
        })?;
        self.attach_color(&buffer);
        self.copied += 1;
        Ok(buffer)
    }

    /// A slot that is not in flight. One exists whenever the order ring has room,
    /// because only slots in the ring are not `FREE`.
    fn free_slot(&self) -> Option<usize> {
        self.shared
            .slots
            .iter()
            .position(|s| s.state.load(Ordering::Acquire) == FREE)
    }

    fn push_order(&mut self, index: usize, forced: bool) {
        let tail = (self.order_head + self.order_len) % FRAMES_IN_FLIGHT;
        // `index < FRAMES_IN_FLIGHT <= u8::MAX` (checked at compile time below).
        self.order[tail] = (index as u8, forced);
        self.order_len += 1;
    }

    fn pop_order(&mut self) {
        self.order_head = (self.order_head + 1) % FRAMES_IN_FLIGHT;
        self.order_len -= 1;
    }
}

const _: () = assert!(FRAMES_IN_FLIGHT <= u8::MAX as usize);

/// A fresh buffer from the session's pixel buffer pool.
fn pool_buffer(session: &VTCompressionSession) -> Result<CFRetained<CVPixelBuffer>, EncodeError> {
    // SAFETY: the session is valid; the getter returns a retained pool.
    let pool: CFRetained<CVPixelBufferPool> =
        unsafe { session.pixel_buffer_pool() }.ok_or(EncodeError::DeviceLost)?;
    let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: `pool` is valid; `raw` receives a +1 reference on success.
    let status =
        unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut raw)) };
    match (status, NonNull::new(raw)) {
        // SAFETY: a successful create hands over a +1 reference.
        (0, Some(raw)) => Ok(unsafe { CFRetained::from_raw(raw) }),
        // The pool is at its limit: try again after a drain.
        (s, _) if s == kCVReturnWouldExceedAllocationThreshold => Err(EncodeError::Busy),
        (s, _) => Err(EncodeError::Failed {
            reason: format!("CVPixelBufferPoolCreatePixelBuffer failed with {s}"),
            recoverable: true,
        }),
    }
}

/// Locks `buffer` for writing and calls `write` with a function returning
/// `(base address, bytes per row, rows)` per plane.
fn with_locked(
    buffer: &CVPixelBuffer,
    write: impl FnOnce(&dyn Fn(usize) -> (*mut u8, usize, usize)) -> bool,
) -> Result<(), EncodeError> {
    // SAFETY: `buffer` is valid; it is unlocked below on every path.
    if unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::empty()) } != 0 {
        return Err(EncodeError::Failed {
            reason: "CVPixelBufferLockBaseAddress failed".into(),
            recoverable: true,
        });
    }
    let base = |plane: usize| {
        (
            CVPixelBufferGetBaseAddressOfPlane(buffer, plane).cast::<u8>(),
            CVPixelBufferGetBytesPerRowOfPlane(buffer, plane),
            CVPixelBufferGetHeightOfPlane(buffer, plane),
        )
    };
    let ok = write(&base);
    // SAFETY: locked above with the same flags.
    unsafe { CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::empty()) };
    if ok {
        Ok(())
    } else {
        Err(EncodeError::InvalidInput(
            "frame planes do not match the session's format".into(),
        ))
    }
}

/// Encodes one black frame to learn the parameter sets and returns the `avcC` /
/// `hvcC` record.
fn prime(
    handle: &Handle,
    shared: &Shared,
    format: &VideoFormat,
    frame_duration: Duration,
    codec: CodecId,
) -> Result<Vec<u8>, CreateError> {
    let session = &*handle.0;
    let buffer =
        pool_buffer(session).map_err(|e| CreateError::Device(format!("pixel buffer pool: {e}")))?;
    let ten_bit = format.pixel_format == PixelFormat::P010;
    let (black, grey): (u16, u16) = match (ten_bit, format.color.range) {
        (false, ColorRange::Full) => (0, 128),
        (false, _) => (16, 128),
        (true, ColorRange::Full) => (0, 512 << 6),
        (true, _) => (64 << 6, 512 << 6),
    };
    with_locked(&buffer, |base| {
        for plane in 0..2 {
            let (dst, stride, rows) = base(plane);
            if dst.is_null() {
                return false;
            }
            let value = if plane == 0 { black } else { grey };
            for y in 0..rows {
                // SAFETY: each row has `stride` bytes while the buffer is locked.
                let row = unsafe { std::slice::from_raw_parts_mut(dst.add(y * stride), stride) };
                if ten_bit {
                    for pair in row.chunks_exact_mut(2) {
                        pair.copy_from_slice(&value.to_le_bytes());
                    }
                } else {
                    row.fill(value as u8);
                }
            }
        }
        true
    })
    .map_err(|e| CreateError::Device(format!("priming frame: {e}")))?;
    let duration = vt_time(u64::try_from(frame_duration.as_nanos()).unwrap_or(u64::MAX));
    // SAFETY: the session and buffer are valid; refcon 0 marks the priming frame.
    let status = unsafe {
        session.encode_frame(
            &buffer,
            vt_time(0),
            duration,
            None,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(device("priming EncodeFrame", status));
    }
    // SAFETY: the session is valid; this waits for the priming frame's callback.
    let status = unsafe { session.complete_frames(kCMTimeInvalid) };
    if status != 0 {
        return Err(device("priming CompleteFrames", status));
    }
    if let Some((_, status)) = shared.failure() {
        return Err(device("priming frame", status));
    }
    let primed = shared
        .primed
        .get()
        .ok_or_else(|| CreateError::Device("priming frame produced no output".into()))?;
    let atom = if codec == CodecId::H264 {
        "avcC"
    } else {
        "hvcC"
    };
    let record = primed.record.clone();
    // NAL length size: avcC byte 4, hvcC byte 21, low two bits plus one. The adapter
    // declares 4-byte lengths; anything else would be mis-framed.
    let length_byte = if codec == CodecId::H264 { 4 } else { 21 };
    match record.get(length_byte) {
        Some(b) if b & 3 == 3 => Ok(record),
        _ => Err(CreateError::Device(format!(
            "{atom} record does not use 4-byte NAL lengths"
        ))),
    }
}

fn profile_level(config: &VideoEncoderConfig) -> Result<&'static CFString, CreateError> {
    let ten_bit = config.format.pixel_format == PixelFormat::P010;
    let name = config.profile.as_deref();
    // SAFETY: the profile constants live for the process.
    unsafe {
        Ok(match (config.codec, name) {
            (CodecId::H264, None | Some("high")) => kVTProfileLevel_H264_High_AutoLevel,
            (CodecId::H264, Some("main")) => kVTProfileLevel_H264_Main_AutoLevel,
            (CodecId::H264, Some("baseline")) => kVTProfileLevel_H264_Baseline_AutoLevel,
            (CodecId::Hevc, None) if ten_bit => kVTProfileLevel_HEVC_Main10_AutoLevel,
            (CodecId::Hevc, None | Some("main")) if !ten_bit => kVTProfileLevel_HEVC_Main_AutoLevel,
            (CodecId::Hevc, Some("main10")) => kVTProfileLevel_HEVC_Main10_AutoLevel,
            (codec, name) => {
                return Err(unsupported(format!(
                    "profile {} for {codec} with {}",
                    name.unwrap_or("(default)"),
                    config.format.pixel_format
                )));
            }
        })
    }
}

/// Signals the colour description in the bitstream. Combinations VideoToolbox has no
/// constant for are refused instead of mis-signalled.
///
/// # Safety
///
/// `session` must be a valid compression session.
unsafe fn set_color(
    session: &VTCompressionSession,
    format: &VideoFormat,
) -> Result<[(&'static CFString, &'static CFString); 3], CreateError> {
    let c = format.color;
    // SAFETY: the constants live for the process.
    let (primaries, transfer, matrix) = unsafe {
        let primaries = match c.primaries {
            ColorPrimaries::Bt709 => kCVImageBufferColorPrimaries_ITU_R_709_2,
            ColorPrimaries::Bt601Pal => kCVImageBufferColorPrimaries_EBU_3213,
            ColorPrimaries::Bt601Ntsc => kCVImageBufferColorPrimaries_SMPTE_C,
            ColorPrimaries::Bt2020 => kCVImageBufferColorPrimaries_ITU_R_2020,
            ColorPrimaries::DisplayP3 => kCVImageBufferColorPrimaries_P3_D65,
            _ => return Err(unsupported("colour primaries")),
        };
        let transfer = match c.transfer {
            TransferFunction::Bt709 => kCVImageBufferTransferFunction_ITU_R_709_2,
            TransferFunction::Srgb => kCVImageBufferTransferFunction_sRGB,
            TransferFunction::Pq => kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
            TransferFunction::Hlg => kCVImageBufferTransferFunction_ITU_R_2100_HLG,
            _ if c.primaries == ColorPrimaries::Bt2020 => kCVImageBufferTransferFunction_ITU_R_2020,
            _ => return Err(unsupported("transfer function")),
        };
        let matrix = match c.matrix {
            MatrixCoefficients::Bt709 => kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            MatrixCoefficients::Bt601 => kCVImageBufferYCbCrMatrix_ITU_R_601_4,
            MatrixCoefficients::Bt2020Ncl => kCVImageBufferYCbCrMatrix_ITU_R_2020,
            _ => return Err(unsupported("YCbCr matrix")),
        };
        (primaries, transfer, matrix)
    };
    // SAFETY: the keys live for the process.
    unsafe {
        for (key, value, what) in [
            (
                kVTCompressionPropertyKey_ColorPrimaries,
                primaries,
                "ColorPrimaries",
            ),
            (
                kVTCompressionPropertyKey_TransferFunction,
                transfer,
                "TransferFunction",
            ),
            (kVTCompressionPropertyKey_YCbCrMatrix, matrix, "YCbCrMatrix"),
        ] {
            cf::set_property(session, key, value).map_err(|s| device(what, s))?;
        }
        Ok([
            (kCVImageBufferColorPrimariesKey, primaries),
            (kCVImageBufferTransferFunctionKey, transfer),
            (kCVImageBufferYCbCrMatrixKey, matrix),
        ])
    }
}

/// Applies a rate-control mode. `native_cbr` is the previous choice for CBR (`None`
/// at creation: try `ConstantBitRate`, fall back to an average plus a 1 s cap).
/// Returns the CBR choice.
fn apply_rate_control(
    session: &VTCompressionSession,
    rc: &RateControl,
    native_cbr: Option<bool>,
) -> Result<bool, Status> {
    // SAFETY: every key is a CoreFoundation constant that lives for the process.
    unsafe {
        let capped = |average: u64, max: u64| -> Result<(), Status> {
            cf::set_i64(
                session,
                kVTCompressionPropertyKey_AverageBitRate,
                clamp_i64(average),
            )?;
            let limits = cf::data_rate_limits(clamp_i64(max / 8), 1.0);
            cf::set_property(session, kVTCompressionPropertyKey_DataRateLimits, &limits)
        };
        match *rc {
            RateControl::Cbr { bitrate_bps } => {
                if native_cbr != Some(false) {
                    match cf::set_i64(
                        session,
                        kVTCompressionPropertyKey_ConstantBitRate,
                        clamp_i64(bitrate_bps),
                    ) {
                        Ok(()) => return Ok(true),
                        // Older encoders do not know `ConstantBitRate`: fall back.
                        Err(s) if native_cbr.is_none() && s == kVTPropertyNotSupportedErr => {}
                        Err(s) => return Err(s),
                    }
                }
                capped(bitrate_bps, bitrate_bps).map(|()| false)
            }
            RateControl::Vbr {
                target_bps,
                max_bps,
            } => capped(target_bps, max_bps).map(|()| false),
            RateControl::ConstantQuality { level } => {
                let quality = 1.0 - f64::from(level.min(51)) / 51.0;
                cf::set_f64(session, kVTCompressionPropertyKey_Quality, quality).map(|()| false)
            }
            _ => Err(kVTPropertyNotSupportedErr),
        }
    }
}

fn clamp_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

impl VideoEncoder for VideoToolboxSession {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }

    fn submit(&mut self, frame: &VideoFrame) -> Result<(), EncodeError> {
        self.failure()?;
        if *frame.format() != self.format || frame.memory_kind() != self.memory {
            return Err(EncodeError::InvalidInput(
                "frame format, size or memory differs from the session".into(),
            ));
        }
        let ns = frame.timestamp().as_nanos();
        if self.last_pts.is_some_and(|last| ns <= last) {
            return Err(EncodeError::InvalidInput(
                "frame timestamps must increase".into(),
            ));
        }
        let surface = match frame.payload() {
            FramePayload::Native {
                surface: NativeSurface::IoSurface { surface, .. },
                sync: SurfaceSync::Ready,
                ..
            } => Some(*surface),
            FramePayload::Native {
                surface: NativeSurface::IoSurface { .. },
                ..
            } => {
                return Err(EncodeError::InvalidInput(
                    "IOSurface frames must be ready (SurfaceSync::Ready)".into(),
                ));
            }
            FramePayload::System(_) => None,
            _ => {
                return Err(EncodeError::InvalidInput(
                    "payload is neither an IOSurface nor system memory".into(),
                ));
            }
        };
        self.check_room()?;
        let (buffer, lease) = match surface {
            Some(surface) => (self.wrap_surface(surface)?, frame.lease().cloned()),
            None => (self.copy_system(frame)?, None),
        };
        let index = self.free_slot().ok_or(EncodeError::Busy)?;

        // The VideoToolbox timeline starts one frame before the first real frame, so the
        // priming frame (time 0) always precedes it and times stay small and positive.
        let first = *self.first_ns.get_or_insert(ns);
        let frame_ns = self.frame_duration.value;
        let base = i64::try_from(first)
            .unwrap_or(i64::MAX)
            .saturating_sub(frame_ns);
        // Stored before the encode call: its callback may run inside it.
        self.shared.base_ns.store(base, Ordering::Release);
        let vt_ns = (i128::from(ns) - i128::from(base)).max(0);

        let forced = self.keyframe_next;
        let slot = &self.shared.slots[index];
        let generation = slot.generation.load(Ordering::Relaxed).wrapping_add(1) & 0x00ff_ffff;
        slot.generation.store(generation, Ordering::Relaxed);
        // SAFETY: the slot is `FREE`, so this thread owns it (see `Shared`'s `Sync`).
        unsafe {
            *slot.lease.get() = lease;
            *slot.outcome.get() = Outcome::Empty;
        }
        // Publishes the generation, lease and outcome to the callback.
        slot.state.store(IN_FLIGHT, Ordering::Release);
        self.push_order(index, forced);

        let properties = forced.then(|| self.force_keyframe.as_opaque());
        // `generation` fits 24 bits and `index + 1` 8 bits; 0 is the priming frame.
        let refcon = ((generation as usize) << 8) | (index + 1);
        // SAFETY: the session and buffer are valid; `refcon` identifies this use of the
        // slot in the callback.
        let status = unsafe {
            self.handle.0.encode_frame(
                &buffer,
                vt_time(u64::try_from(vt_ns).unwrap_or(u64::MAX)),
                self.frame_duration,
                properties,
                refcon as *mut c_void,
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            // No callback follows a refused frame; take the slot back unless one did.
            if let Some(slot) = self.shared.claim(index, generation) {
                // SAFETY: this thread won the claim and owns the slot.
                drop(unsafe { (*slot.lease.get()).take() });
                slot.state.store(FREE, Ordering::Release);
                // The slot was pushed last; nothing was pushed after it.
                self.order_len -= 1;
            }
            return Err(status_error(status, true));
        }
        self.keyframe_next = false;
        self.last_pts = Some(ns);
        self.submitted += 1;
        Ok(())
    }

    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        while self.head_state() == Some(DONE) {
            let (index, forced) = self.order[self.order_head];
            let slot = &self.shared.slots[usize::from(index)];
            // SAFETY: the slot is `DONE`, so this thread owns it again.
            let outcome = unsafe { std::mem::replace(&mut *slot.outcome.get(), Outcome::Empty) };
            match outcome {
                Outcome::Packet(packet) => {
                    out.push(packet);
                    self.packets += 1;
                }
                Outcome::Lost | Outcome::Empty => {
                    self.dropped += 1;
                    // A drop never takes a pending keyframe with it (invariant 3).
                    if forced {
                        self.keyframe_next = true;
                    }
                }
            }
            slot.state.store(FREE, Ordering::Release);
            self.pop_order();
        }
        self.failure()
    }

    fn reconfigure(&mut self, change: &EncoderChange) -> Result<(), EncodeError> {
        match change {
            EncoderChange::RequestKeyframe => {
                self.keyframe_next = true;
                Ok(())
            }
            EncoderChange::Bitrate(rc) => {
                if rc.name() != self.rate_control.name() {
                    // Switching modes mid-session is not reliable across encoders.
                    return Err(EncodeError::Unsupported);
                }
                check_rate_control(rc).map_err(EncodeError::InvalidInput)?;
                match apply_rate_control(&self.handle.0, rc, Some(self.native_cbr)) {
                    Ok(_) => {
                        self.rate_control = *rc;
                        Ok(())
                    }
                    Err(s) if s == kVTPropertyNotSupportedErr => Err(EncodeError::Unsupported),
                    Err(s) => Err(EncodeError::Failed {
                        reason: format!("bitrate change failed with OSStatus {s}"),
                        recoverable: true,
                    }),
                }
            }
            _ => Err(EncodeError::Unsupported),
        }
    }

    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        // SAFETY: the session is valid; this waits until every pending frame was emitted.
        let status = unsafe { self.handle.0.complete_frames(kCMTimeInvalid) };
        self.drain(out)?;
        if status != 0 {
            return Err(status_error(status, false));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(value: i64, timescale: i32) -> CMTime {
        CMTime {
            value,
            timescale,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        }
    }

    #[test]
    fn master_time_adds_the_base_and_clamps_at_zero() {
        let base = 2_000_000_000;
        assert_eq!(
            master_time(time(33_000_000, TIMESCALE), base),
            Some(Timestamp::from_nanos(2_033_000_000))
        );
        // Another timescale is converted to nanoseconds.
        assert_eq!(
            master_time(time(3, 90_000), 0),
            Some(Timestamp::from_nanos(33_333))
        );
        assert_eq!(master_time(time(5, TIMESCALE), -10), Some(Timestamp::ZERO));
        let mut invalid = time(1, TIMESCALE);
        invalid.flags = CMTimeFlags::empty();
        assert_eq!(master_time(invalid, 0), None);
        assert_eq!(master_time(time(1, 0), 0), None);
    }

    #[test]
    fn vt_time_round_trips_through_master_time() {
        for ns in [0, 1, 33_366_666, 86_400_000_000_000] {
            assert_eq!(master_time(vt_time(ns), 0), Some(Timestamp::from_nanos(ns)));
        }
    }

    #[test]
    fn rate_control_is_checked() {
        assert!(check_rate_control(&RateControl::Cbr { bitrate_bps: 0 }).is_err());
        assert!(check_rate_control(&RateControl::Cbr { bitrate_bps: 1 }).is_ok());
        assert!(
            check_rate_control(&RateControl::Vbr {
                target_bps: 2,
                max_bps: 1
            })
            .is_err()
        );
        assert!(check_rate_control(&RateControl::ConstantQuality { level: 52 }).is_err());
        assert!(check_rate_control(&RateControl::ConstantQuality { level: 23 }).is_ok());
    }

    #[test]
    fn failures_keep_the_first_kind_and_status() {
        let shared = Shared {
            codec: CodecId::H264,
            bitstream: BitstreamFormat::LengthPrefixed { length_size: 4 },
            frame_duration: Duration::from_millis(33),
            slots: Box::new([]),
            failure: AtomicU64::new(0),
            base_ns: AtomicI64::new(0),
            atom: Frozen(CFString::from_str("avcC")),
            primed: OnceLock::new(),
        };
        assert_eq!(shared.failure(), None);
        shared.fail(FAIL_STATUS, -12_345);
        shared.fail(FAIL_PANIC, 0);
        assert_eq!(shared.failure(), Some((FAIL_STATUS, -12_345)));
        assert!(shared.claim(0, 0).is_none(), "no such slot");
        assert!(matches!(
            failure_error(FAIL_STATUS, kVTInvalidSessionErr),
            EncodeError::DeviceLost
        ));
        assert!(matches!(
            failure_error(FAIL_PARAMETER_SETS, 0),
            EncodeError::Failed {
                recoverable: false,
                ..
            }
        ));
        assert!(matches!(
            status_error(-1, true),
            EncodeError::Failed {
                recoverable: true,
                ..
            }
        ));
    }

    #[test]
    fn a_slot_is_claimed_once_per_use() {
        let slot = Slot {
            state: AtomicU8::new(IN_FLIGHT),
            generation: AtomicU32::new(7),
            lease: UnsafeCell::new(None),
            outcome: UnsafeCell::new(Outcome::Empty),
        };
        let shared = Shared {
            codec: CodecId::H264,
            bitstream: BitstreamFormat::LengthPrefixed { length_size: 4 },
            frame_duration: Duration::from_millis(33),
            slots: Box::new([slot]),
            failure: AtomicU64::new(0),
            base_ns: AtomicI64::new(0),
            atom: Frozen(CFString::from_str("avcC")),
            primed: OnceLock::new(),
        };
        assert!(shared.claim(0, 6).is_none(), "stale generation");
        assert!(shared.claim(0, 7).is_some());
        assert!(shared.claim(0, 7).is_none(), "already claimed");
    }

    #[test]
    fn only_bi_planar_formats_have_a_fourcc() {
        let f = |pf, cs| VideoFormat::new(rbs_core::media::Size::new(64, 64), pf, cs);
        use rbs_core::media::ColorSpace;
        assert_eq!(
            fourcc_of(&f(PixelFormat::Nv12, ColorSpace::BT709_LIMITED)),
            Some(FOURCC_NV12_VIDEO)
        );
        assert_eq!(
            fourcc_of(&f(PixelFormat::P010, ColorSpace::BT709_LIMITED)),
            Some(FOURCC_P010_VIDEO)
        );
        assert_eq!(
            fourcc_of(&f(PixelFormat::I420, ColorSpace::BT709_LIMITED)),
            None
        );
    }
}
