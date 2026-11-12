//! Fake [`MediaDecoder`](rbs_media_decode::MediaDecoder) (ADR-0108 decision 10).
//!
//! Frames are preloaded in the factory. `open` does not read the file. `pull` does
//! not read the file. A file with no audio stream reports [`ScriptDecoder::audio_stream`]
//! false; the node still emits silence on its audio pin.
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
    DecodeError, InterleavedPcm, MediaDecoder, MediaDecoderFactory, MediaSample, Picture,
    PictureLayout, SampleColor,
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
        let n = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(4);
        let mut bytes = vec![0u8; n];
        for pixel in bytes.chunks_exact_mut(4) {
            pixel[0] = mark;
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
    /// `pull` calls.
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
    /// When set, `open` fails with `Media`.
    pub fail_open: Option<String>,
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
        self.counts.alive.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(ScriptDecoder {
            frames: self.clip.frames.clone(),
            audio: self.clip.audio.clone(),
            index: 0,
            counts: Arc::clone(&self.counts),
        }))
    }
}

struct ScriptDecoder {
    frames: Vec<ScriptFrame>,
    audio: Option<InterleavedPcm>,
    index: usize,
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
        Ok(())
    }

    fn pull(&mut self) -> Result<Option<MediaSample>, DecodeError> {
        self.counts.pulls.fetch_add(1, Ordering::Relaxed);
        let Some(frame) = self.frames.get(self.index) else {
            return Ok(None);
        };
        self.index += 1;
        let pcm = if self.index == 1 {
            self.audio.clone()
        } else {
            None
        };
        Ok(Some(MediaSample {
            pts: frame.pts,
            frame_duration: frame.frame_duration,
            picture: Some(Picture {
                width: frame.width,
                height: frame.height,
                layout: frame.layout,
                color: frame.color,
                bytes: frame.bytes.clone(),
            }),
            pcm,
        }))
    }

    fn audio_stream(&self) -> bool {
        self.audio.is_some()
    }
}
