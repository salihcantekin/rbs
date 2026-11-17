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
    AVAsset, AVAssetReader, AVAssetReaderStatus, AVAssetReaderTrackOutput, AVAssetTrack,
    AVMediaType, AVMediaTypeAudio, AVMediaTypeVideo, AVURLAsset,
};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer, CMTime, CMTimeFlags,
    CMTimeRange, kCMTimePositiveInfinity,
};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_foundation::{NSDictionary, NSError, NSNumber, NSString, NSURL};

use rbs_media_decode::{
    DecodeError, InterleavedPcm, MediaDecoder, Picture, PictureLayout, PictureSample, SampleColor,
    SoundSample, frame_duration_rule, refuse_picture,
};

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

fn video_settings() -> Retained<NSDictionary<NSString, AnyObject>> {
    // `kCVPixelBufferPixelFormatTypeKey` is the string "PixelFormatType".
    settings(&[(
        "PixelFormatType",
        NSNumber::new_u32(kCVPixelFormatType_32BGRA),
    )])
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

/// A reader of `track` from `cursor` on.
fn start_reader(
    asset: &AVAsset,
    track: &AVAssetTrack,
    settings: &NSDictionary<NSString, AnyObject>,
    cursor: Duration,
) -> Result<(Retained<AVAssetReader>, Retained<AVAssetReaderTrackOutput>), DecodeError> {
    // SAFETY: live objects; the reader is configured before `startReading`.
    unsafe {
        let reader = AVAssetReader::assetReaderWithAsset_error(asset)
            .map_err(|e| failed("asset reader", Some(e)))?;
        let output = AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
            track,
            Some(settings),
        );
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
    output: Retained<AVAssetReaderTrackOutput>,
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
    video: Stream,
    sound: Option<(Stream, u32, u16)>,
    duration: Option<Duration>,
    nominal: Option<Duration>,
    cursor: Duration,
    last_duration: Option<Duration>,
}

// SAFETY: AVAsset, AVAssetReader and their outputs are not bound to a thread; the
// decoder is used by one thread at a time (the worker or the probe).
unsafe impl Send for AvfDecoder {}

impl AvfDecoder {
    pub(crate) fn open(path: &Path) -> Result<Self, DecodeError> {
        autoreleasepool(|_| Self::open_in_pool(path))
    }

    fn open_in_pool(path: &Path) -> Result<Self, DecodeError> {
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
        let (reader, output) =
            start_reader(&asset, &video_track, &video_settings(), Duration::ZERO)?;
        Ok(Self {
            asset,
            video_track,
            audio_track,
            video: Stream { reader, output },
            sound: None,
            duration: time(length).filter(|d| !d.is_zero()),
            nominal: nominal_duration(rate),
            cursor: Duration::ZERO,
            last_duration: None,
        })
    }

    fn picture(&mut self, sample: &CMSampleBuffer) -> Result<PictureSample, DecodeError> {
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
        let frame_duration = frame_duration_rule(stated, false, self.nominal)
            .or(self.last_duration)
            .or(self.nominal);
        self.last_duration = frame_duration;
        if pts.is_none() && frame_duration.is_none() {
            return Err(DecodeError::NoTimestamp);
        }
        Ok(PictureSample {
            pts,
            frame_duration,
            picture: Picture::system(w, h, PictureLayout::Rgba8, SampleColor::Srgb, bytes),
        })
    }

    fn open_sound(&mut self) -> Result<(), DecodeError> {
        if self.sound.is_some() {
            return Ok(());
        }
        let Some(track) = &self.audio_track else {
            return Ok(());
        };
        let (reader, output) = start_reader(&self.asset, track, &audio_settings(), self.cursor)?;
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
            let (reader, output) =
                start_reader(&self.asset, &self.video_track, &video_settings(), cursor)?;
            self.video = Stream { reader, output };
            self.sound = None;
            self.cursor = cursor;
            self.last_duration = None;
            Ok(())
        })
    }

    fn next_picture(&mut self) -> Result<Option<PictureSample>, DecodeError> {
        autoreleasepool(|_| {
            let Some(sample) = self.video.next()? else {
                return Ok(None);
            };
            self.picture(&sample).map(Some)
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
