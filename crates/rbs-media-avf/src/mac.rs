//! The macOS backend: one `AVAssetReader` per stream.
//!
//! Threads: every call runs on the media source's worker or probe thread, never on a
//! real-time thread. Each call runs inside an autorelease pool, because the worker
//! thread has none and AVFoundation returns autoreleased objects.

use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::Duration;

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAsset, AVAssetReader, AVAssetReaderOutput, AVAssetReaderStatus, AVAssetReaderTrackOutput,
    AVAssetReaderVideoCompositionOutput, AVAssetTrack, AVMediaType, AVMediaTypeAudio,
    AVMediaTypeVideo, AVURLAsset, AVVideoComposition,
};
use objc2_core_foundation::CFRetained;
use objc2_core_foundation::{CFString, CFType};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMFormatDescription, CMSampleBuffer, CMTime,
    CMTimeFlags, CMTimeRange, CMVideoFormatDescriptionGetDimensions,
    kCMFormatDescriptionColorPrimaries_EBU_3213, kCMFormatDescriptionColorPrimaries_ITU_R_709_2,
    kCMFormatDescriptionColorPrimaries_ITU_R_2020, kCMFormatDescriptionColorPrimaries_P3_D65,
    kCMFormatDescriptionColorPrimaries_SMPTE_C, kCMFormatDescriptionExtension_ColorPrimaries,
    kCMFormatDescriptionExtension_TransferFunction, kCMFormatDescriptionExtension_YCbCrMatrix,
    kCMFormatDescriptionTransferFunction_ITU_R_709_2,
    kCMFormatDescriptionTransferFunction_ITU_R_2020,
    kCMFormatDescriptionTransferFunction_SMPTE_240M_1995,
    kCMFormatDescriptionTransferFunction_sRGB, kCMFormatDescriptionYCbCrMatrix_ITU_R_601_4,
    kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2, kCMFormatDescriptionYCbCrMatrix_ITU_R_2020,
    kCMFormatDescriptionYCbCrMatrix_SMPTE_240M_1995, kCMTimePositiveInfinity,
};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
    kCVReturnSuccess,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSString, NSURL};
use rbs_core::media::{
    ColorPrimaries, ColorRange, ColorSpace, MatrixCoefficients, NativeSurface, PixelFormat, Size,
    SurfaceSync, TransferFunction, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;

use rbs_media_decode::{
    DecodeError, InterleavedPcm, MediaDecoder, Picture, PictureLayout, PictureSample, Rotation,
    SampleColor, SoundSample, frame_duration_rule, refuse_picture,
};

use crate::surfaces::Surfaces;
use crate::{bgra_to_rgba, cm_duration, nominal_duration};

/// `kAudioFormatLinearPCM` (`'lpcm'`).
const LINEAR_PCM: u32 = 0x6C70_636D;
/// `AVErrorDecoderNotFound` and `AVErrorFileFormatNotRecognized`.
const MISSING_DECODER: [isize; 2] = [-11833, -11828];

fn failed(what: &str, error: Option<Retained<NSError>>) -> DecodeError {
    match error {
        Some(e) if MISSING_DECODER.contains(&e.code()) => DecodeError::Unsupported(format!(
            "AVFoundation cannot decode this file: {}",
            e.localizedDescription()
        )),
        Some(e) => DecodeError::Failed(format!("{what}: {}", e.localizedDescription())),
        None => DecodeError::Failed(what.to_owned()),
    }
}

fn time(t: CMTime) -> Option<Duration> {
    cm_duration(t.flags.contains(CMTimeFlags::Valid), t.value, t.timescale)
}

fn settings(pairs: &[(&str, Retained<NSNumber>)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<Retained<NSString>> = pairs.iter().map(|(k, _)| NSString::from_str(k)).collect();
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let values: Vec<Retained<AnyObject>> = pairs
        .iter()
        .map(|(_, v)| Retained::into_super(Retained::into_super(Retained::into_super(v.clone()))))
        .collect();
    NSDictionary::from_retained_objects(&key_refs, &values)
}

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` ('420v'): NV12, limited range.
const PIXEL_FORMAT_420V: u32 = 0x3432_3076;

/// The picture output: `32BGRA` for the copy, or IOSurface-backed `420v` (NV12, what
/// VideoToolbox decodes to, so it converts nothing) for native pictures.
fn video_settings(native: bool) -> Retained<NSDictionary<NSString, AnyObject>> {
    if !native {
        // `kCVPixelBufferPixelFormatTypeKey` is the string "PixelFormatType".
        return settings(&[(
            "PixelFormatType",
            NSNumber::new_u32(kCVPixelFormatType_32BGRA),
        )]);
    }
    let format = NSString::from_str("PixelFormatType");
    // `kCVPixelBufferIOSurfacePropertiesKey` is the string "IOSurfaceProperties"; an
    // empty dictionary asks for IOSurface-backed buffers with default properties.
    let surface = NSString::from_str("IOSurfaceProperties");
    let empty: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
    let values: [Retained<AnyObject>; 2] = [
        Retained::into_super(Retained::into_super(Retained::into_super(
            NSNumber::new_u32(PIXEL_FORMAT_420V),
        ))),
        Retained::into_super(Retained::into_super(empty)),
    ];
    NSDictionary::from_retained_objects(&[&*format, &*surface], &values)
}

/// The colour of the track's Y'CbCr from its format description, as the importer
/// decodes native `420v` pictures, or `None` when it cannot (an HDR or unknown transfer,
/// an unknown matrix or primaries): such a file opens for copied pictures, which
/// VideoToolbox converts to sRGB. A missing tag follows the common player rule: BT.601
/// up to 576 lines, BT.709 above, and the BT.709 transfer.
fn track_color(track: &AVAssetTrack) -> Option<ColorSpace> {
    // SAFETY: a plain property read of a live track.
    let descriptions = unsafe { track.formatDescriptions() };
    let first = descriptions.firstObject()?;
    // SAFETY: `formatDescriptions` holds `CMFormatDescription`s, which are CF objects;
    // the reference lives as long as `first`.
    let description: &CMFormatDescription =
        unsafe { &*(Retained::as_ptr(&first).cast::<CMFormatDescription>()) };
    // SAFETY: a video track's description is a video format description.
    let lines = unsafe { CMVideoFormatDescriptionGetDimensions(description) }.height;
    // SAFETY: framework string constants, valid for the life of the process.
    let (keys, matrices, all_primaries, transfers) = unsafe {
        (
            [
                kCMFormatDescriptionExtension_YCbCrMatrix,
                kCMFormatDescriptionExtension_ColorPrimaries,
                kCMFormatDescriptionExtension_TransferFunction,
            ],
            [
                (
                    kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2,
                    MatrixCoefficients::Bt709,
                ),
                (
                    kCMFormatDescriptionYCbCrMatrix_ITU_R_601_4,
                    MatrixCoefficients::Bt601,
                ),
                (
                    kCMFormatDescriptionYCbCrMatrix_SMPTE_240M_1995,
                    MatrixCoefficients::Bt601,
                ),
                (
                    kCMFormatDescriptionYCbCrMatrix_ITU_R_2020,
                    MatrixCoefficients::Bt2020Ncl,
                ),
            ],
            [
                (
                    kCMFormatDescriptionColorPrimaries_ITU_R_709_2,
                    ColorPrimaries::Bt709,
                ),
                (
                    kCMFormatDescriptionColorPrimaries_EBU_3213,
                    ColorPrimaries::Bt601Pal,
                ),
                (
                    kCMFormatDescriptionColorPrimaries_SMPTE_C,
                    ColorPrimaries::Bt601Ntsc,
                ),
                (
                    kCMFormatDescriptionColorPrimaries_ITU_R_2020,
                    ColorPrimaries::Bt2020,
                ),
                (
                    kCMFormatDescriptionColorPrimaries_P3_D65,
                    ColorPrimaries::DisplayP3,
                ),
            ],
            [
                (
                    kCMFormatDescriptionTransferFunction_ITU_R_709_2,
                    TransferFunction::Bt709,
                ),
                (
                    kCMFormatDescriptionTransferFunction_ITU_R_2020,
                    TransferFunction::Bt709,
                ),
                (
                    kCMFormatDescriptionTransferFunction_SMPTE_240M_1995,
                    TransferFunction::Bt709,
                ),
                (
                    kCMFormatDescriptionTransferFunction_sRGB,
                    TransferFunction::Srgb,
                ),
            ],
        )
    };
    // The value of a tag: `Ok(None)` when missing, `Err` when the tag names a value the
    // table does not have.
    fn lookup<T: Copy>(
        description: &CMFormatDescription,
        key: &CFString,
        table: &[(&CFString, T)],
    ) -> Result<Option<T>, ()> {
        // SAFETY: reads a tag of a live description.
        let Some(value) = (unsafe { description.extension(key) }) else {
            return Ok(None);
        };
        let value: &CFType = &value;
        table
            .iter()
            .find(|(name, _)| *value == ***name)
            .map(|&(_, v)| Some(v))
            .ok_or(())
    }
    let [matrix_key, primaries_key, transfer_key] = keys;
    let matrix = lookup(description, matrix_key, &matrices)
        .ok()?
        .unwrap_or(if lines <= 576 {
            MatrixCoefficients::Bt601
        } else {
            MatrixCoefficients::Bt709
        });
    let primaries = lookup(description, primaries_key, &all_primaries)
        .ok()?
        .unwrap_or(match matrix {
            MatrixCoefficients::Bt601 if lines == 576 => ColorPrimaries::Bt601Pal,
            MatrixCoefficients::Bt601 => ColorPrimaries::Bt601Ntsc,
            MatrixCoefficients::Bt2020Ncl => ColorPrimaries::Bt2020,
            _ => ColorPrimaries::Bt709,
        });
    let transfer = lookup(description, transfer_key, &transfers)
        .ok()?
        .unwrap_or(TransferFunction::Bt709);
    // `420v` is video range whatever the file's range: VideoToolbox converts.
    Some(ColorSpace::new(
        primaries,
        transfer,
        matrix,
        ColorRange::Limited,
    ))
}

fn audio_settings() -> Retained<NSDictionary<NSString, AnyObject>> {
    // The AVAudioSettings keys are strings equal to their names.
    settings(&[
        ("AVFormatIDKey", NSNumber::new_u32(LINEAR_PCM)),
        ("AVLinearPCMBitDepthKey", NSNumber::new_u32(32)),
        ("AVLinearPCMIsFloatKey", NSNumber::new_bool(true)),
        ("AVLinearPCMIsBigEndianKey", NSNumber::new_bool(false)),
        ("AVLinearPCMIsNonInterleaved", NSNumber::new_bool(false)),
    ])
}

/// The first track of `kind`, if any.
fn first_track(
    asset: &AVAsset,
    kind: Option<&'static AVMediaType>,
) -> Option<Retained<AVAssetTrack>> {
    let kind = kind?;
    // SAFETY: a live asset and a framework media type constant. The synchronous track
    // list is deprecated in favour of the async loader, which needs a run loop the
    // worker does not have; it still loads synchronously on macOS 15.
    #[allow(deprecated)]
    let tracks = unsafe { asset.tracksWithMediaType(kind) };
    tracks.firstObject()
}

/// The track's rotation from its display matrix (`preferredTransform`): AVFoundation
/// decodes the stored picture, and a phone's upright clip is stored on its side.
fn track_rotation(track: &AVAssetTrack) -> Rotation {
    // SAFETY: a plain property read of a live track.
    let t = unsafe { track.preferredTransform() };
    // The matrix [a b; c d] turns by atan2(b, a), clockwise on screen (y down).
    Rotation::from_clockwise_degrees(t.b.atan2(t.a).to_degrees())
}

/// A reader of `track` from `cursor` on. A `rotated` track is read through a video
/// composition made from the asset's properties, which turns each picture upright on
/// the GPU into a new IOSurface-backed buffer (spec invariant 12); other tracks are
/// read as decoded, with no extra pass.
fn start_reader(
    asset: &AVAsset,
    track: &AVAssetTrack,
    settings: &NSDictionary<NSString, AnyObject>,
    cursor: Duration,
    rotated: bool,
) -> Result<(Retained<AVAssetReader>, Retained<AVAssetReaderOutput>), DecodeError> {
    // SAFETY: live objects; the reader is configured before `startReading`.
    unsafe {
        let reader = AVAssetReader::assetReaderWithAsset_error(asset)
            .map_err(|e| failed("asset reader", Some(e)))?;
        let output: Retained<AVAssetReaderOutput> = if rotated {
            let tracks = NSArray::from_slice(&[track]);
            let output = AVAssetReaderVideoCompositionOutput::assetReaderVideoCompositionOutputWithVideoTracks_videoSettings(
                &tracks,
                Some(settings),
            );
            // The synchronous constructor is deprecated in favour of an async one that
            // needs a run loop the worker does not have; the asset is loaded already.
            #[allow(deprecated)]
            let composition = AVVideoComposition::videoCompositionWithPropertiesOfAsset(asset);
            output.setVideoComposition(Some(&composition));
            Retained::into_super(output)
        } else {
            Retained::into_super(
                AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
                    track,
                    Some(settings),
                ),
            )
        };
        // The output's buffers are only read, so AVFoundation may hand out its own.
        output.setAlwaysCopiesSampleData(false);
        if !reader.canAddOutput(&output) {
            return Err(DecodeError::Unsupported(
                "AVFoundation cannot decode this track".into(),
            ));
        }
        reader.addOutput(&output);
        if !cursor.is_zero() {
            let nanos = i64::try_from(cursor.as_nanos()).unwrap_or(i64::MAX);
            reader.setTimeRange(CMTimeRange {
                start: CMTime::new(nanos, 1_000_000_000),
                duration: kCMTimePositiveInfinity,
            });
        }
        if !reader.startReading() {
            return Err(failed("start reading", reader.error()));
        }
        Ok((reader, output))
    }
}

/// One stream's reader and output.
struct Stream {
    reader: Retained<AVAssetReader>,
    output: Retained<AVAssetReaderOutput>,
}

impl Stream {
    /// The next sample buffer, or `None` at the end of the stream.
    fn next(&self) -> Result<Option<Retained<CMSampleBuffer>>, DecodeError> {
        // SAFETY: a reader that started reading; `copyNextSampleBuffer` returns +1.
        unsafe {
            match self.output.copyNextSampleBuffer() {
                Some(sample) => Ok(Some(sample)),
                None if self.reader.status() == AVAssetReaderStatus::Failed => {
                    Err(failed("read", self.reader.error()))
                }
                None => Ok(None),
            }
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: stops a live reader; idempotent.
        unsafe { self.reader.cancelReading() };
    }
}

/// The decoder of one file.
pub(crate) struct AvfDecoder {
    asset: Retained<AVURLAsset>,
    video_track: Retained<AVAssetTrack>,
    audio_track: Option<Retained<AVAssetTrack>>,
    /// The picture track is stored on its side or upside down (spec invariant 12).
    rotated: bool,
    video: Stream,
    sound: Option<(Stream, u32, u16)>,
    duration: Option<Duration>,
    nominal: Option<Duration>,
    cursor: Duration,
    last_duration: Option<Duration>,
    /// The slots of native pictures and their colour; `None` for the copy
    /// (`cpu-upload`, or a colour the importer cannot decode).
    surfaces: Option<(Surfaces, ColorSpace)>,
}

// SAFETY: AVAsset, AVAssetReader and their outputs are not bound to a thread; the
// decoder is used by one thread at a time (the worker or the probe).
unsafe impl Send for AvfDecoder {}

impl AvfDecoder {
    pub(crate) fn open(path: &Path, native: bool) -> Result<Self, DecodeError> {
        autoreleasepool(|_| Self::open_in_pool(path, native))
    }

    fn open_in_pool(path: &Path, native: bool) -> Result<Self, DecodeError> {
        let path: PathBuf = path.to_owned();
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        // SAFETY: a file URL and no options.
        let asset = unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) };
        // SAFETY: framework constants.
        let (video_kind, audio_kind) = unsafe { (AVMediaTypeVideo, AVMediaTypeAudio) };
        let video_track = first_track(&asset, video_kind)
            .ok_or_else(|| DecodeError::Unsupported("the file has no picture stream".into()))?;
        let audio_track = first_track(&asset, audio_kind);
        // SAFETY: plain property reads of live objects.
        let (length, rate) = unsafe { (asset.duration(), video_track.nominalFrameRate()) };
        let color = if native {
            track_color(&video_track)
        } else {
            None
        };
        let native = color.is_some();
        let rotated = track_rotation(&video_track) != Rotation::None;
        let (reader, output) = start_reader(
            &asset,
            &video_track,
            &video_settings(native),
            Duration::ZERO,
            rotated,
        )?;
        Ok(Self {
            asset,
            video_track,
            audio_track,
            rotated,
            video: Stream { reader, output },
            sound: None,
            duration: time(length).filter(|d| !d.is_zero()),
            nominal: nominal_duration(rate),
            cursor: Duration::ZERO,
            last_duration: None,
            surfaces: color.map(|c| (Surfaces::new(), c)),
        })
    }

    /// The picture of `sample`, or `None` when it is skipped because every native slot
    /// is still referenced (the clock repeats the previous picture).
    fn picture(&mut self, sample: &CMSampleBuffer) -> Result<Option<PictureSample>, DecodeError> {
        // SAFETY: plain reads of a live sample buffer.
        let (pts, stated, image) = unsafe {
            (
                time(sample.presentation_time_stamp()),
                time(sample.duration()),
                sample.image_buffer(),
            )
        };
        let image =
            image.ok_or_else(|| DecodeError::Failed("a video sample without a picture".into()))?;
        let width = CVPixelBufferGetWidth(&image);
        let height = CVPixelBufferGetHeight(&image);
        let (w, h) = (
            u32::try_from(width).unwrap_or(u32::MAX),
            u32::try_from(height).unwrap_or(u32::MAX),
        );
        if let Some(refused) = refuse_picture(w, h, width.saturating_mul(height).saturating_mul(4))
        {
            return Err(DecodeError::Failed(format!(
                "the picture must be {}",
                refused.expected()
            )));
        }
        let frame_duration = frame_duration_rule(stated, false, self.nominal)
            .or(self.last_duration)
            .or(self.nominal);
        if let Some((surfaces, color)) = self.surfaces.as_mut() {
            self.last_duration = frame_duration;
            if pts.is_none() && frame_duration.is_none() {
                return Err(DecodeError::NoTimestamp);
            }
            if CVPixelBufferGetPixelFormatType(&image) != PIXEL_FORMAT_420V {
                return Err(DecodeError::Failed(
                    "the decoder gave a picture of another pixel format".into(),
                ));
            }
            let Some((surface, lease)) = surfaces.hand_out(image) else {
                return Ok(None);
            };
            let format = VideoFormat::new(Size::new(w, h), PixelFormat::Nv12, *color);
            let surface = NativeSurface::io_surface(CFRetained::as_ptr(&surface).as_ptr() as usize);
            // SAFETY: the IOSurface belongs to the pixel buffer the pool keeps in the
            // lease's slot until every reference to that slot is released, and
            // VideoToolbox does not write a buffer it handed out while it is retained.
            let frame = unsafe {
                VideoFrame::native(format, Timestamp::ZERO, surface, SurfaceSync::Ready, lease)
            };
            let picture = Picture::native(frame)
                .ok_or_else(|| DecodeError::Failed("a native picture".into()))?;
            return Ok(Some(PictureSample {
                pts,
                frame_duration,
                picture,
            }));
        }
        let mut bytes = Vec::new();
        // SAFETY: the base address is read only between lock and unlock, within the
        // rows up to the last pixel of the last row.
        let copied = unsafe {
            if CVPixelBufferLockBaseAddress(&image, CVPixelBufferLockFlags::ReadOnly)
                != kCVReturnSuccess
            {
                return Err(DecodeError::Failed("lock picture".into()));
            }
            let base = CVPixelBufferGetBaseAddress(&image).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRow(&image);
            let done = if base.is_null() {
                None
            } else {
                let src = std::slice::from_raw_parts(
                    base,
                    stride.saturating_mul(height.saturating_sub(1)) + width * 4,
                );
                bgra_to_rgba(src, stride, width, height, &mut bytes)
            };
            CVPixelBufferUnlockBaseAddress(&image, CVPixelBufferLockFlags::ReadOnly);
            done
        };
        copied.ok_or_else(|| DecodeError::Failed("the picture buffer is too small".into()))?;
        self.last_duration = frame_duration;
        if pts.is_none() && frame_duration.is_none() {
            return Err(DecodeError::NoTimestamp);
        }
        Ok(Some(PictureSample {
            pts,
            frame_duration,
            picture: Picture::system(w, h, PictureLayout::Rgba8, SampleColor::Srgb, bytes),
        }))
    }

    fn open_sound(&mut self) -> Result<(), DecodeError> {
        if self.sound.is_some() {
            return Ok(());
        }
        let Some(track) = &self.audio_track else {
            return Ok(());
        };
        let (reader, output) =
            start_reader(&self.asset, track, &audio_settings(), self.cursor, false)?;
        self.sound = Some((Stream { reader, output }, 0, 0));
        Ok(())
    }

    fn sound(&mut self) -> Result<Option<SoundSample>, DecodeError> {
        self.open_sound()?;
        let Some((stream, rate, channels)) = &mut self.sound else {
            return Ok(None);
        };
        let Some(sample) = stream.next()? else {
            return Ok(None);
        };
        // SAFETY: plain reads of a live sample buffer; the format description of an
        // audio sample is an audio description, and its ASBD lives as long as it.
        unsafe {
            if *rate == 0 {
                let format = sample
                    .format_description()
                    .ok_or_else(|| DecodeError::Failed("sound without a format".into()))?;
                let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(&format);
                let asbd = asbd
                    .as_ref()
                    .ok_or_else(|| DecodeError::Failed("sound without a format".into()))?;
                *rate = asbd.mSampleRate.round() as u32;
                *channels = u16::try_from(asbd.mChannelsPerFrame).unwrap_or(0);
                if *rate == 0 || *channels == 0 {
                    return Err(DecodeError::Failed(
                        "the sound has no rate or channels".into(),
                    ));
                }
            }
            let pts = time(sample.presentation_time_stamp());
            let block = sample
                .data_buffer()
                .ok_or_else(|| DecodeError::Failed("sound without data".into()))?;
            let len = block.data_length();
            let mut samples = vec![0f32; len / 4];
            if let Some(dest) = NonNull::new(samples.as_mut_ptr().cast()) {
                if block.copy_data_bytes(0, samples.len() * 4, dest) != 0 {
                    return Err(DecodeError::Failed("copy sound".into()));
                }
            }
            Ok(Some(SoundSample {
                pts,
                pcm: InterleavedPcm {
                    sample_rate: *rate,
                    channels: *channels,
                    samples,
                },
            }))
        }
    }
}

impl MediaDecoder for AvfDecoder {
    fn seek(&mut self, cursor: Duration) -> Result<(), DecodeError> {
        autoreleasepool(|_| {
            let native = self.surfaces.is_some();
            let (reader, output) = start_reader(
                &self.asset,
                &self.video_track,
                &video_settings(native),
                cursor,
                self.rotated,
            )?;
            self.video = Stream { reader, output };
            self.sound = None;
            self.cursor = cursor;
            self.last_duration = None;
            Ok(())
        })
    }

    fn next_picture(&mut self) -> Result<Option<PictureSample>, DecodeError> {
        autoreleasepool(|_| {
            loop {
                let Some(sample) = self.video.next()? else {
                    return Ok(None);
                };
                if let Some(picture) = self.picture(&sample)? {
                    return Ok(Some(picture));
                }
            }
        })
    }

    fn next_sound(&mut self) -> Result<Option<SoundSample>, DecodeError> {
        autoreleasepool(|_| self.sound())
    }

    fn has_audio(&self) -> bool {
        self.audio_track.is_some()
    }

    fn duration(&self) -> Option<Duration> {
        self.duration
    }
}
