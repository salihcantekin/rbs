//! Fake [`MediaDecoder`](rbs_media_decode::MediaDecoder) (ADR-0108 decision 10).
//!
//! Frames are preloaded in the factory. Neither `open` nor a read touches the file. A
//! clip with no audio stream reports `has_audio` false; the source still has a sound
//! node that writes silence. Pictures and sound are read per stream, as the port says.
//!
//! This crate is not an FFmpeg adapter and does not link `ffmpeg-sys`.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_media_decode::caps::PIXEL;
use rbs_media_decode::pixel::CPU_UPLOAD;
use rbs_media_decode::{
    DecodeError, InterleavedPcm, MediaDecoder, MediaDecoderFactory, Picture, PictureLayout,
    PictureSample, SampleColor, SoundSample, frame_duration_rule,
};

/// One scripted picture. `bytes` is straight-alpha sRGB `Rgba8` unless `layout` says
/// otherwise.
#[derive(Debug, Clone)]
pub struct ScriptFrame {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Container timestamp.
    pub pts: Option<Duration>,
    /// Container frame duration.
    pub frame_duration: Option<Duration>,
    /// Layout the node should see.
    pub layout: PictureLayout,
    /// Colour of `bytes`.
    pub color: SampleColor,
    /// Payload. Empty is allowed for a size-refusal probe.
    pub bytes: Vec<u8>,
}

impl ScriptFrame {
    /// A straight-alpha sRGB `Rgba8` frame of `width` by `height`, filled with `mark`.
    pub fn rgba(
        width: u32,
        height: u32,
        pts: Duration,
        frame_duration: Duration,
        mark: u8,
    ) -> Self {
        Self::rgba_rgb(width, height, pts, frame_duration, [mark, mark, mark])
    }

    /// A straight-alpha sRGB `Rgba8` frame filled with `rgb` (alpha 255).
    pub fn rgba_rgb(
        width: u32,
        height: u32,
        pts: Duration,
        frame_duration: Duration,
        rgb: [u8; 3],
    ) -> Self {
        let n = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(4);
        let mut bytes = vec![0u8; n];
        for pixel in bytes.chunks_exact_mut(4) {
            pixel[0] = rgb[0];
            pixel[1] = rgb[1];
            pixel[2] = rgb[2];
            pixel[3] = 255;
        }
        Self {
            width,
            height,
            pts: Some(pts),
            frame_duration: Some(frame_duration),
            layout: PictureLayout::Rgba8,
            color: SampleColor::Srgb,
            bytes,
        }
    }

    /// Cyan/blue checker like the browser stand-in helper; `phase` shifts the pattern.
    pub fn checkerboard(
        width: u32,
        height: u32,
        pts: Duration,
        frame_duration: Duration,
        phase: u32,
    ) -> Self {
        let width_usize = width as usize;
        let height_usize = height as usize;
        let mut bytes = vec![0u8; width_usize.saturating_mul(height_usize).saturating_mul(4)];
        let shift = (phase as usize).saturating_mul(16);
        for (index, pixel) in bytes.chunks_exact_mut(4).enumerate() {
            let x = index % width_usize;
            let y = index / width_usize;
            let light = ((x.saturating_add(shift)) / 32 + y / 32).is_multiple_of(2);
            if light {
                pixel[0] = 40;
                pixel[1] = 180;
                pixel[2] = 255;
            } else {
                pixel[0] = 20;
                pixel[1] = 40;
                pixel[2] = 120;
            }
            pixel[3] = 255;
        }
        Self {
            width,
            height,
            pts: Some(pts),
            frame_duration: Some(frame_duration),
            layout: PictureLayout::Rgba8,
            color: SampleColor::Srgb,
            bytes,
        }
    }
}

/// Counts opens and pulls. `file_bytes` stays 0: this adapter never reads the path.
#[derive(Debug, Default)]
pub struct ScriptCounts {
    /// `open` calls.
    pub opens: AtomicU64,
    /// `next_picture` and `next_sound` calls.
    pub pulls: AtomicU64,
    /// Bytes read from the filesystem. Always 0.
    pub file_bytes: AtomicU64,
    /// Live decoders.
    pub alive: AtomicU64,
    /// Last `seek` cursor.
    pub last_seek: Mutex<Option<Duration>>,
}

/// A clip the factory hands out. `audio` is `None` when the file has no audio stream.
#[derive(Debug, Clone)]
pub struct ScriptClip {
    /// Pictures in order.
    pub frames: Vec<ScriptFrame>,
    /// Interleaved PCM, or `None` for silence on the node's pin.
    pub audio: Option<InterleavedPcm>,
    /// When set, `open` fails with `DecodeError::Failed`.
    pub fail_open: Option<String>,
    /// When set, `open` fails with `DecodeError::Unsupported` (a missing codec).
    pub unsupported: Option<String>,
    /// An animated picture (GIF, APNG, WebP): short delays read as 100 ms.
    pub animated: bool,
    /// The stated duration, if any.
    pub duration: Option<Duration>,
}

impl ScriptClip {
    /// Three 2×2 frames at 0, 100 ms, and 200 ms. No audio stream.
    pub fn silent_frames() -> Self {
        let step = Duration::from_millis(100);
        Self {
            frames: vec![
                ScriptFrame::rgba(2, 2, Duration::ZERO, step, 1),
                ScriptFrame::rgba(2, 2, step, step, 2),
                ScriptFrame::rgba(2, 2, step + step, step, 3),
            ],
            audio: None,
            fail_open: None,
            unsupported: None,
            animated: false,
            duration: None,
        }
    }

    /// Three 320×180 frames at 0, 100 ms, and 200 ms with a moving checker. No audio.
    pub fn demo_pattern() -> Self {
        let step = Duration::from_millis(100);
        let w = 320;
        let h = 180;
        Self {
            frames: vec![
                ScriptFrame::checkerboard(w, h, Duration::ZERO, step, 0),
                ScriptFrame::checkerboard(w, h, step, step, 1),
                ScriptFrame::checkerboard(w, h, step + step, step, 2),
            ],
            audio: None,
            fail_open: None,
            unsupported: None,
            animated: false,
            duration: None,
        }
    }

    /// A picture whose side is over 8192. No payload is stored.
    pub fn oversized_side() -> Self {
        Self {
            frames: vec![ScriptFrame {
                width: 8193,
                height: 2,
                pts: Some(Duration::ZERO),
                frame_duration: Some(Duration::from_millis(100)),
                layout: PictureLayout::Rgba8,
                color: SampleColor::Srgb,
                bytes: Vec::new(),
            }],
            audio: None,
            fail_open: None,
            unsupported: None,
            animated: false,
            duration: None,
        }
    }
}

/// `cpu-upload` factory. Id `org.rbs.media-decode.test`.
pub struct ScriptFactory {
    id: AdapterId,
    clip: ScriptClip,
    /// Shared counters.
    pub counts: Arc<ScriptCounts>,
    pixel: &'static str,
}

impl ScriptFactory {
    /// A `cpu-upload` factory for `clip`.
    pub fn new(clip: ScriptClip) -> Self {
        Self::with_pixel("org.rbs.media-decode.test", clip, CPU_UPLOAD)
    }

    /// A factory with an explicit id and pixel capability (`zero-copy` or `cpu-upload`).
    pub fn with_pixel(id: &str, clip: ScriptClip, pixel: &'static str) -> Self {
        Self {
            id: AdapterId::new(id),
            clip,
            counts: Arc::new(ScriptCounts::default()),
            pixel,
        }
    }
}

impl AdapterFactory for ScriptFactory {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    fn display_name(&self) -> &str {
        "Scripted media"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new().with(PIXEL, CapabilityValue::Set(vec![self.pixel.to_owned()])))
    }
}

impl MediaDecoderFactory for ScriptFactory {
    fn open(&self, _path: &Path) -> Result<Box<dyn MediaDecoder>, DecodeError> {
        self.counts.opens.fetch_add(1, Ordering::Relaxed);
        if let Some(text) = &self.clip.fail_open {
            return Err(DecodeError::Failed(text.clone()));
        }
        if let Some(codec) = &self.clip.unsupported {
            return Err(DecodeError::Unsupported(codec.clone()));
        }
        self.counts.alive.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(ScriptDecoder {
            frames: self.clip.frames.clone(),
            audio: self.clip.audio.clone(),
            animated: self.clip.animated,
            duration: self.clip.duration,
            index: 0,
            sound_frame: 0,
            counts: Arc::clone(&self.counts),
        }))
    }
}

/// PCM frames handed out per `next_sound` call.
pub const SOUND_CHUNK_FRAMES: usize = 1024;

struct ScriptDecoder {
    frames: Vec<ScriptFrame>,
    audio: Option<InterleavedPcm>,
    animated: bool,
    duration: Option<Duration>,
    index: usize,
    sound_frame: usize,
    counts: Arc<ScriptCounts>,
}

impl Drop for ScriptDecoder {
    fn drop(&mut self) {
        self.counts.alive.fetch_sub(1, Ordering::Relaxed);
    }
}

impl MediaDecoder for ScriptDecoder {
    fn seek(&mut self, cursor: Duration) -> Result<(), DecodeError> {
        if let Ok(mut slot) = self.counts.last_seek.lock() {
            *slot = Some(cursor);
        }
        self.index = self
            .frames
            .iter()
            .position(|frame| frame.pts.unwrap_or(Duration::ZERO) >= cursor)
            .unwrap_or(self.frames.len());
        if let Some(audio) = &self.audio {
            let rate = u128::from(audio.sample_rate.max(1));
            self.sound_frame = (cursor.as_nanos() * rate / 1_000_000_000) as usize;
        }
        Ok(())
    }

    fn next_picture(&mut self) -> Result<Option<PictureSample>, DecodeError> {
        self.counts.pulls.fetch_add(1, Ordering::Relaxed);
        let Some(frame) = self.frames.get(self.index) else {
            return Ok(None);
        };
        self.index += 1;
        Ok(Some(PictureSample {
            pts: frame.pts,
            frame_duration: frame_duration_rule(frame.frame_duration, self.animated, None),
            picture: Picture::system(
                frame.width,
                frame.height,
                frame.layout,
                frame.color,
                frame.bytes.clone(),
            ),
        }))
    }

    fn next_sound(&mut self) -> Result<Option<SoundSample>, DecodeError> {
        self.counts.pulls.fetch_add(1, Ordering::Relaxed);
        let Some(audio) = &self.audio else {
            return Ok(None);
        };
        let channels = usize::from(audio.channels.max(1));
        let total = audio.samples.len() / channels;
        if self.sound_frame >= total {
            return Ok(None);
        }
        let end = (self.sound_frame + SOUND_CHUNK_FRAMES).min(total);
        let rate = u64::from(audio.sample_rate.max(1));
        let pts = Duration::from_nanos(self.sound_frame as u64 * 1_000_000_000 / rate);
        let samples = audio.samples[self.sound_frame * channels..end * channels].to_vec();
        self.sound_frame = end;
        Ok(Some(SoundSample {
            pts: Some(pts),
            pcm: InterleavedPcm {
                sample_rate: audio.sample_rate,
                channels: audio.channels,
                samples,
            },
        }))
    }

    fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    fn duration(&self) -> Option<Duration> {
        self.duration
    }
}
