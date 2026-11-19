//! The Windows backend: synchronous source readers, one per stream.
//!
//! Threads: every call runs on the media source's worker or probe thread, never on a
//! real-time thread. COM: the adapter keeps the process's implicit multithreaded
//! apartment alive (`CoIncrementMTAUsage`, like the capture adapter), so a worker
//! thread that never initialised COM uses the free-threaded Media Foundation objects
//! in the MTA.

use std::path::Path;
use std::time::Duration;

use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFDXGIDeviceManager, IMFMediaType, IMFSample, IMFSourceReader,
    MF_E_INVALIDMEDIATYPE, MF_E_TOPO_CODEC_NOT_FOUND, MF_E_UNSUPPORTED_BYTESTREAM_TYPE,
    MF_E_UNSUPPORTED_FORMAT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE, MF_MT_VIDEO_ROTATION, MF_PD_DURATION,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_D3D_MANAGER, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_VERSION, MFAudioFormat_Float,
    MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Audio,
    MFMediaType_Video, MFSTARTUP_NOSOCKET, MFShutdown, MFStartup, MFVideoArea,
    MFVideoFormat_ARGB32, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::CoIncrementMTAUsage;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{GUID, HRESULT, HSTRING, Interface};

use rbs_media_decode::{
    DecodeError, InterleavedPcm, MediaDecoder, Picture, PictureLayout, PictureSample, Rotation,
    SampleColor, SoundSample, frame_duration_rule, refuse_picture, rotate_rgba,
};

use crate::edits::{SoundEdit, Trimmed};
use crate::gpu::Gpu;
use crate::{Crop, f32_samples, hns, missing_codec, nominal_duration, to_hns};

const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
const AUDIO: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
const ALL: u32 = MF_SOURCE_READER_ALL_STREAMS.0 as u32;
const SOURCE: u32 = MF_SOURCE_READER_MEDIASOURCE.0 as u32;

fn failed(what: &str, e: &windows::core::Error) -> DecodeError {
    DecodeError::Failed(format!("{what}: {e}"))
}

fn is_unsupported(code: HRESULT) -> bool {
    matches!(
        code,
        MF_E_UNSUPPORTED_BYTESTREAM_TYPE
            | MF_E_INVALIDMEDIATYPE
            | MF_E_TOPO_CODEC_NOT_FOUND
            | MF_E_UNSUPPORTED_FORMAT
    )
}

/// Keeps the process's implicit multithreaded apartment alive (see the module doc).
fn ensure_mta() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| {
        // SAFETY: plain FFI call. The cookie is never released: the MTA stays alive
        // for the process lifetime, which is the intent.
        let _ = unsafe { CoIncrementMTAUsage() };
    });
}

/// One `MFStartup`, balanced by `MFShutdown` on drop. Declared last in the decoder so
/// it drops after the readers.
struct Runtime;

impl Runtime {
    fn start() -> Result<Self, DecodeError> {
        ensure_mta();
        // SAFETY: plain FFI call, balanced by `Drop`.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }
            .map_err(|e| failed("MFStartup", &e))?;
        Ok(Self)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: balances the `MFStartup` of `start`.
        let _ = unsafe { MFShutdown() };
    }
}

/// The URL Media Foundation reads: the canonical path without the verbatim prefix
/// (`accept_local_media_path` refuses UNC, so only `\\?\C:\...` reaches here).
fn url_of(path: &Path) -> HSTRING {
    let text = path.to_string_lossy();
    let plain = text.strip_prefix(r"\\?\").unwrap_or(&text);
    HSTRING::from(plain)
}

/// A source reader on `path` with only `stream` selected. With `manager`, the decoder
/// and the video processor run on its D3D11 device.
fn reader(
    path: &Path,
    stream: u32,
    video_processing: bool,
    manager: Option<&IMFDXGIDeviceManager>,
) -> Result<IMFSourceReader, DecodeError> {
    let mut attributes = None;
    // SAFETY: a valid out pointer.
    unsafe { MFCreateAttributes(&mut attributes, 3) }
        .map_err(|e| failed("MFCreateAttributes", &e))?;
    let attributes =
        attributes.ok_or_else(|| DecodeError::Failed("MFCreateAttributes gave nothing".into()))?;
    if video_processing {
        // SAFETY: a valid attribute store and key.
        unsafe { attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1) }
            .map_err(|e| failed("reader attributes", &e))?;
    }
    if let Some(manager) = manager {
        // SAFETY: a valid attribute store, keys and a live manager, which the store
        // keeps its own reference to.
        unsafe {
            attributes
                .SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, manager)
                .and_then(|()| attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1))
        }
        .map_err(|e| failed("reader attributes", &e))?;
    }
    // SAFETY: the URL and attributes outlive the call.
    let reader =
        unsafe { MFCreateSourceReaderFromURL(&url_of(path), &attributes) }.map_err(|e| {
            if is_unsupported(e.code()) {
                DecodeError::Unsupported("Media Foundation cannot read this file type".into())
            } else {
                failed("open", &e)
            }
        })?;
    // SAFETY: valid stream indices of a live reader.
    unsafe {
        reader
            .SetStreamSelection(ALL, false)
            .and_then(|()| reader.SetStreamSelection(stream, true))
    }
    .map_err(|e| failed("stream selection", &e))?;
    Ok(reader)
}

fn media_type(major: &GUID, subtype: &GUID) -> Result<IMFMediaType, DecodeError> {
    // SAFETY: plain FFI calls on a new media type.
    unsafe {
        let ty = MFCreateMediaType().map_err(|e| failed("MFCreateMediaType", &e))?;
        ty.SetGUID(&MF_MT_MAJOR_TYPE, major)
            .and_then(|()| ty.SetGUID(&MF_MT_SUBTYPE, subtype))
            .map_err(|e| failed("media type", &e))?;
        Ok(ty)
    }
}

/// The decoded picture's geometry, read again when the type changes.
struct VideoShape {
    frame_w: u32,
    frame_h: u32,
    /// Bytes from one row to the next; negative for a bottom-up picture.
    stride: isize,
    crop: Crop,
    nominal: Option<Duration>,
}

impl VideoShape {
    fn read(reader: &IMFSourceReader) -> Result<Self, DecodeError> {
        // SAFETY: a valid stream index of a live reader; attribute reads on the type.
        unsafe {
            let ty = reader
                .GetCurrentMediaType(VIDEO)
                .map_err(|e| failed("video type", &e))?;
            let size = ty
                .GetUINT64(&MF_MT_FRAME_SIZE)
                .map_err(|e| failed("frame size", &e))?;
            let frame_w = (size >> 32) as u32;
            let frame_h = (size & 0xFFFF_FFFF) as u32;
            // RGB without a stated stride is bottom-up (Media Foundation's rule).
            let stride = match ty.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
                Ok(raw) => raw as i32 as isize,
                Err(_) => -(frame_w as isize * 4),
            };
            let mut area = MFVideoArea::default();
            let aperture = {
                // SAFETY: `area` is a plain struct of the blob's size.
                let bytes = std::slice::from_raw_parts_mut(
                    (&mut area as *mut MFVideoArea).cast::<u8>(),
                    std::mem::size_of::<MFVideoArea>(),
                );
                ty.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes, None)
                    .ok()
                    .map(|()| {
                        (
                            i32::from(area.OffsetX.value),
                            i32::from(area.OffsetY.value),
                            area.Area.cx,
                            area.Area.cy,
                        )
                    })
            };
            let nominal = ty
                .GetUINT64(&MF_MT_FRAME_RATE)
                .ok()
                .and_then(nominal_duration);
            Ok(Self {
                frame_w,
                frame_h,
                stride,
                crop: Crop::of(frame_w, frame_h, aperture),
                nominal,
            })
        }
    }
}

/// The sound reader and its format, created on the first `next_sound`.
struct Sound {
    reader: IMFSourceReader,
    sample_rate: u32,
    channels: u16,
}

/// The turn still to apply to the reader's pictures (spec invariant 12).
///
/// The turn comes from the picture track's display matrix (`edits.rs`), or from
/// `MF_MT_VIDEO_ROTATION` (counted counter-clockwise) for a file without one. The
/// source reader's video processor applies a stated `MF_MT_VIDEO_ROTATION` itself
/// (measured on Windows 11 for a quarter turn), so a quarter turn is left only when
/// the output picture (its display aperture) still lies the way the stored one does;
/// a half turn, which no size shows, is left only when the native type states none.
fn left_to_turn(path: &Path, native: &IMFMediaType, shape: &VideoShape) -> Rotation {
    // SAFETY: attribute reads on a native type.
    let (stated, stored) = unsafe {
        (
            native.GetUINT32(&MF_MT_VIDEO_ROTATION).unwrap_or(0),
            native.GetUINT64(&MF_MT_FRAME_SIZE).ok(),
        )
    };
    let rotation = std::fs::File::open(path)
        .ok()
        .and_then(|mut file| crate::edits::video_rotation(&mut file))
        .unwrap_or_else(|| Rotation::from_clockwise_degrees(-f64::from(stated)));
    if rotation.swaps_sides() {
        let lies_as_stored = stored.is_none_or(|size| {
            let (w, h) = ((size >> 32) as u32, size as u32);
            let (cw, ch) = (shape.crop.width, shape.crop.height);
            w == h || cw == ch || (w > h) == (cw > ch)
        });
        if lies_as_stored {
            rotation
        } else {
            Rotation::None
        }
    } else if stated != 0 {
        Rotation::None
    } else {
        rotation
    }
}

/// The decoder of one file.
pub(crate) struct MfDecoder {
    path: std::path::PathBuf,
    video: IMFSourceReader,
    shape: VideoShape,
    /// The turn that shows the track upright (spec invariant 12), applied to every
    /// picture: by the video processor for a native picture, on the copy otherwise.
    rotation: Rotation,
    sound: Option<Sound>,
    has_audio: bool,
    duration: Option<Duration>,
    /// The sound track's edit list, which Media Foundation does not apply to AAC
    /// (`edits.rs`); `None` when it changes nothing or the file has none.
    sound_edit: Option<SoundEdit>,
    /// The last seek target, applied to a sound reader created after it.
    cursor: Duration,
    /// The previous picture's duration, to continue an unstated one.
    last_duration: Option<Duration>,
    /// Swizzle buffer for a picture copied in memory. Empty while every picture is
    /// handed out from the GPU.
    rgba: crate::SwizzleSpare,
    /// The decode device and the shared textures of native pictures; `None` copies.
    /// Dropped after the readers that use its device manager.
    gpu: Option<Gpu>,
    /// Declared last: Media Foundation shuts down after the readers are released.
    _runtime: Runtime,
}

// SAFETY: Media Foundation source readers are free-threaded and the adapter's threads
// share the process's MTA (module doc). The decoder is used by one thread at a time.
unsafe impl Send for MfDecoder {}

impl MfDecoder {
    /// Opens `path`. With the render adapter's `luid`, pictures are decoded on that
    /// adapter and handed out as shared textures; when that device cannot be made or
    /// cannot decode the file, the file opens again for copied pictures.
    pub(crate) fn open(path: &Path, luid: Option<u64>) -> Result<Self, DecodeError> {
        if let Some(luid) = luid
            && let Ok(gpu) = Gpu::new(luid)
            && let Ok(decoder) = Self::open_with(path, Some(gpu))
        {
            return Ok(decoder);
        }
        Self::open_with(path, None)
    }

    fn open_with(path: &Path, gpu: Option<Gpu>) -> Result<Self, DecodeError> {
        let runtime = Runtime::start()?;
        let video = reader(path, VIDEO, true, gpu.as_ref().map(Gpu::manager))?;
        // SAFETY: valid stream indices of a live reader.
        let native = unsafe { video.GetNativeMediaType(VIDEO, 0) }.map_err(|e| {
            if is_unsupported(e.code())
                || e.code() == windows::Win32::Media::MediaFoundation::MF_E_INVALIDSTREAMNUMBER
            {
                DecodeError::Unsupported("the file has no picture stream".into())
            } else {
                failed("video stream", &e)
            }
        })?;
        // The video processor's B8G8R8A8 texture for a native picture (RGB32 would be
        // B8G8R8X8, which the importer does not take); 32-bit BGRX in memory otherwise.
        let subtype = if gpu.is_some() {
            MFVideoFormat_ARGB32
        } else {
            MFVideoFormat_RGB32
        };
        let want = media_type(&MFMediaType_Video, &subtype)?;
        // SAFETY: a valid stream index and media type.
        if let Err(e) = unsafe { video.SetCurrentMediaType(VIDEO, None, &want) } {
            if is_unsupported(e.code()) {
                // SAFETY: attribute read on a native type.
                let subtype = unsafe { native.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
                return Err(DecodeError::Unsupported(missing_codec(subtype.data1)));
            }
            return Err(failed("video output type", &e));
        }
        let shape = VideoShape::read(&video)?;
        let rotation = left_to_turn(path, &native, &shape);
        let (w, h) = (shape.crop.width, shape.crop.height);
        if let Some(refused) = refuse_picture(w, h, w as usize * h as usize * 4) {
            return Err(DecodeError::Failed(format!(
                "the picture must be {}",
                refused.expected()
            )));
        }
        // SAFETY: valid stream indices of a live reader.
        let has_audio = unsafe { video.GetNativeMediaType(AUDIO, 0) }.is_ok();
        // SAFETY: a valid attribute of the media source.
        let duration = unsafe { video.GetPresentationAttribute(SOURCE, &MF_PD_DURATION) }
            .ok()
            .and_then(|value: PROPVARIANT| u64::try_from(&value).ok())
            .filter(|&v| v > 0)
            .and_then(|v| hns(i64::try_from(v).ok()?));
        let sound_edit = if has_audio {
            std::fs::File::open(path)
                .ok()
                .and_then(|mut file| crate::edits::sound_edit(&mut file))
                .filter(|edit| !edit.is_identity())
        } else {
            None
        };
        Ok(Self {
            path: path.to_owned(),
            video,
            shape,
            rotation,
            sound: None,
            has_audio,
            duration,
            sound_edit,
            cursor: Duration::ZERO,
            last_duration: None,
            rgba: crate::SwizzleSpare::new(),
            gpu,
            _runtime: runtime,
        })
    }

    fn open_sound(&mut self) -> Result<&mut Sound, DecodeError> {
        if self.sound.is_none() {
            let reader = reader(&self.path, AUDIO, false, None)?;
            let want = media_type(&MFMediaType_Audio, &MFAudioFormat_Float)?;
            // SAFETY: a valid stream index and media type; attribute reads on the type.
            let (sample_rate, channels) = unsafe {
                reader
                    .SetCurrentMediaType(AUDIO, None, &want)
                    .map_err(|e| {
                        if is_unsupported(e.code()) {
                            DecodeError::Unsupported(
                                "no Media Foundation decoder for this sound".into(),
                            )
                        } else {
                            failed("sound output type", &e)
                        }
                    })?;
                let ty = reader
                    .GetCurrentMediaType(AUDIO)
                    .map_err(|e| failed("sound type", &e))?;
                (
                    ty.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)
                        .map_err(|e| failed("sample rate", &e))?,
                    ty.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)
                        .map_err(|e| failed("channels", &e))?,
                )
            };
            let channels = u16::try_from(channels)
                .ok()
                .filter(|&c| c > 0)
                .ok_or_else(|| DecodeError::Failed("the sound has no channels".into()))?;
            if !self.cursor.is_zero() {
                seek_reader(&reader, self.cursor)?;
            }
            self.sound = Some(Sound {
                reader,
                sample_rate,
                channels,
            });
        }
        self.sound
            .as_mut()
            .ok_or_else(|| DecodeError::Failed("no sound reader".into()))
    }
}

fn seek_reader(reader: &IMFSourceReader, cursor: Duration) -> Result<(), DecodeError> {
    let position = PROPVARIANT::from(to_hns(cursor));
    // SAFETY: GUID_NULL selects 100 ns units; the PROPVARIANT outlives the call.
    unsafe { reader.SetCurrentPosition(&GUID::zeroed(), &position) }.map_err(|e| failed("seek", &e))
}

/// One read of `stream`: `Ok(None)` at its end; skips stream ticks.
fn read(
    reader: &IMFSourceReader,
    stream: u32,
) -> Result<Option<(IMFSample, i64, bool)>, DecodeError> {
    loop {
        let mut flags = 0u32;
        let mut stamp = 0i64;
        let mut sample = None;
        // SAFETY: valid out pointers that outlive the synchronous call.
        unsafe {
            reader.ReadSample(
                stream,
                0,
                None,
                Some(&mut flags),
                Some(&mut stamp),
                Some(&mut sample),
            )
        }
        .map_err(|e| failed("read", &e))?;
        let flags = flags as i32;
        if flags & MF_SOURCE_READERF_ERROR.0 != 0 {
            return Err(DecodeError::Failed(
                "the decoder stopped with an error".into(),
            ));
        }
        let changed = flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 != 0;
        if let Some(sample) = sample {
            return Ok(Some((sample, stamp, changed)));
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 != 0 {
            return Ok(None);
        }
        // A stream tick (a gap) or a type change without a sample: read again.
    }
}

/// Copies the picture of `sample` as `Rgba8` into `spare`'s buffer.
fn picture_bytes(
    sample: &IMFSample,
    shape: &VideoShape,
    spare: &mut crate::SwizzleSpare,
) -> Result<Vec<u8>, DecodeError> {
    // SAFETY: the sample is live; each buffer is locked and unlocked in this block and
    // the pointer is only read inside the lock. The returned `Vec` does not borrow it.
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| failed("picture buffer", &e))?;
        let copied = if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
            let mut scan0 = std::ptr::null_mut();
            let mut pitch = 0i32;
            two_d
                .Lock2D(&mut scan0, &mut pitch)
                .map_err(|e| failed("lock picture", &e))?;
            let pitch = pitch as isize;
            let rows = shape.frame_h as isize;
            // The rows span from the lowest to the highest address of scan line 0..h.
            let low = if pitch < 0 {
                scan0.offset(pitch * (rows - 1))
            } else {
                scan0
            };
            // Up to the end of the last row's pixels, not its padding, which the
            // buffer need not hold.
            let before_last = pitch.unsigned_abs() * (shape.frame_h as usize).saturating_sub(1);
            let len = before_last + shape.frame_w as usize * 4;
            let first = if pitch < 0 { before_last } else { 0 };
            let src = std::slice::from_raw_parts(low, len);
            let done = spare.swizzle(src, first, pitch, shape.crop);
            let _ = two_d.Unlock2D();
            done
        } else {
            let mut data = std::ptr::null_mut();
            let mut len = 0u32;
            buffer
                .Lock(&mut data, None, Some(&mut len))
                .map_err(|e| failed("lock picture", &e))?;
            let src = std::slice::from_raw_parts(data, len as usize);
            let stride = shape.stride;
            let first = if stride < 0 {
                stride.unsigned_abs() * (shape.frame_h as usize).saturating_sub(1)
            } else {
                0
            };
            let done = spare.swizzle(src, first, stride, shape.crop);
            let _ = buffer.Unlock();
            done
        };
        copied.ok_or_else(|| DecodeError::Failed("the picture buffer is too small".into()))
    }
}

impl MediaDecoder for MfDecoder {
    fn seek(&mut self, cursor: Duration) -> Result<(), DecodeError> {
        self.cursor = cursor;
        self.last_duration = None;
        seek_reader(&self.video, cursor)?;
        if let Some(sound) = &self.sound {
            seek_reader(&sound.reader, cursor)?;
        }
        Ok(())
    }

    fn next_picture(&mut self) -> Result<Option<PictureSample>, DecodeError> {
        let Some((sample, stamp, changed)) = read(&self.video, VIDEO)? else {
            return Ok(None);
        };
        if changed {
            self.shape = VideoShape::read(&self.video)?;
        }
        // SAFETY: an attribute read on a live sample.
        let stated = unsafe { sample.GetSampleDuration() }.ok().and_then(hns);
        let frame_duration = frame_duration_rule(stated, false, self.shape.nominal)
            .or(self.last_duration)
            .or(self.shape.nominal);
        self.last_duration = frame_duration;
        let pts = hns(stamp);
        if pts.is_none() && frame_duration.is_none() {
            return Err(DecodeError::NoTimestamp);
        }
        if let Some(gpu) = self.gpu.as_mut()
            && let Some(picture) = gpu.picture(&sample, self.shape.crop, self.rotation)?
        {
            return Ok(Some(PictureSample {
                pts,
                frame_duration,
                picture,
            }));
        }
        let mut bytes = picture_bytes(&sample, &self.shape, &mut self.rgba)?;
        let (mut width, mut height) = (self.shape.crop.width, self.shape.crop.height);
        if self.rotation != Rotation::None {
            let mut turned = Vec::new();
            let turned_size = rotate_rgba(&bytes, width, height, self.rotation, &mut turned);
            // The returned picture is `turned`. The swizzle buffer is not that picture.
            self.rgba.recycle(bytes);
            (width, height) = turned_size
                .ok_or_else(|| DecodeError::Failed("a picture of the wrong size".into()))?;
            bytes = turned;
        }
        Ok(Some(PictureSample {
            pts,
            frame_duration,
            picture: Picture::system(
                width,
                height,
                PictureLayout::Rgba8,
                SampleColor::Srgb,
                bytes,
            ),
        }))
    }

    fn next_sound(&mut self) -> Result<Option<SoundSample>, DecodeError> {
        if !self.has_audio {
            return Ok(None);
        }
        let edit = self.sound_edit;
        let sound = self.open_sound()?;
        loop {
            let Some((sample, stamp, _)) = read(&sound.reader, AUDIO)? else {
                return Ok(None);
            };
            let mut samples = Vec::new();
            // SAFETY: the buffer is locked and unlocked here; the pointer is only read
            // inside the lock.
            unsafe {
                let buffer = sample
                    .ConvertToContiguousBuffer()
                    .map_err(|e| failed("sound buffer", &e))?;
                let mut data = std::ptr::null_mut();
                let mut len = 0u32;
                buffer
                    .Lock(&mut data, None, Some(&mut len))
                    .map_err(|e| failed("lock sound", &e))?;
                samples.extend(f32_samples(std::slice::from_raw_parts(data, len as usize)));
                let _ = buffer.Unlock();
            }
            let mut pts = hns(stamp);
            // The priming before the edit and the padding after it are not played.
            if let (Some(edit), Some(at)) = (edit.as_ref(), pts) {
                match crate::edits::trim(edit, at, sound.sample_rate, sound.channels, &mut samples)
                {
                    Trimmed::Play(start) => pts = Some(start),
                    Trimmed::Skip => continue,
                    Trimmed::End => return Ok(None),
                }
            }
            return Ok(Some(SoundSample {
                pts,
                pcm: InterleavedPcm {
                    sample_rate: sound.sample_rate,
                    channels: sound.channels,
                    samples,
                },
            }));
        }
    }

    fn has_audio(&self) -> bool {
        self.has_audio
    }

    fn duration(&self) -> Option<Duration> {
        self.duration
    }
}
