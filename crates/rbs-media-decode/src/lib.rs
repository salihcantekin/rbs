//! Media-file decode port (ADR-0108).
//!
//! A [`MediaDecoderFactory`] opens a path the host has already accepted and reads its
//! picture stream and first audio stream separately, with container timestamps. It does not own the master
//! clock, `loop`, `restart_when_shown`, the playback cursor, or the queues. Those
//! belong to the `org.rbs.media` node.
//!
//! The first adapters are the platform decoders (`rbs-media-mf` on Windows,
//! `rbs-media-avf` on macOS) and the pure-Rust `rbs-media-anim` for animated pictures.
//! FFmpeg may later be `rbs-media-ffmpeg` (ADR-0007). Conformance uses
//! `rbs-media-decode-test`. This crate links no decoder.
//!
//! Capability `media.decode.pixel` is `zero-copy` or `cpu-upload`. The host prefers
//! `zero-copy`, then registration order, unless the source's `adapter` setting names
//! one id.

mod path;

pub mod caps;

use std::path::Path;
use std::time::Duration;

use rbs_core::registry::AdapterFactory;

pub use path::{PathError, accept_local_media_path, canonical_is_local};

/// Largest picture side, in pixels (ADR-0108 decision 6, same ceiling as a still).
pub const MAX_SIDE: u32 = 8192;
/// Largest one-frame payload, in bytes (128 MiB, the still decoder-buffer ceiling).
pub const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;
/// Video lookahead slot cap. The byte cap can stop the queue earlier.
pub const VIDEO_LOOKAHEAD: usize = 8;
/// Audio blocks kept ahead of the graph. Each block is the graph's `block_frames`.
pub const AUDIO_LOOKAHEAD: usize = 4;

/// `media.decode.pixel` values.
pub mod pixel {
    /// GPU handle the existing YUV decoder can sample.
    pub const ZERO_COPY: &str = "zero-copy";
    /// System memory the worker uploads through `SourceUpload` before enqueue.
    pub const CPU_UPLOAD: &str = "cpu-upload";
}

/// Why a picture must not be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PictureRefuse {
    /// A side is 0 or over [`MAX_SIDE`].
    Side,
    /// The payload is over [`MAX_FRAME_BYTES`].
    FrameBytes,
}

impl PictureRefuse {
    /// What `InvalidSetting` should say.
    pub const fn expected(self) -> &'static str {
        match self {
            PictureRefuse::Side => "at most 8192 pixels on a side",
            PictureRefuse::FrameBytes => "at most 128 MiB in one frame",
        }
    }
}

/// Refuses a picture that must not allocate a queue (ADR-0108 decision 6).
pub fn refuse_picture(width: u32, height: u32, payload_bytes: usize) -> Option<PictureRefuse> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Some(PictureRefuse::Side);
    }
    if payload_bytes > MAX_FRAME_BYTES {
        Some(PictureRefuse::FrameBytes)
    } else {
        None
    }
}

/// Pixel layout the node will enqueue. Anything else is converted by the adapter or
/// `open` fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PictureLayout {
    /// Straight-alpha sRGB `Rgba8`, the still-source rule (GIF, APNG, animated WebP).
    Rgba8,
    /// 8-bit 4:2:0. The node uses the existing `YuvDecoder` (`check_color`).
    Nv12,
    /// 10-bit 4:2:0. The node uses the existing `YuvDecoder` (`check_color`).
    P010,
}

/// Colour the adapter already converted to something `check_color` can accept, or sRGB
/// for a full-frame picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SampleColor {
    /// Straight-alpha sRGB.
    Srgb,
    /// BT.709 limited-range Y'CbCr.
    Bt709Limited,
    /// BT.709 full-range Y'CbCr.
    Bt709Full,
}

/// Where a picture's pixels are. More payload kinds may be added without a breaking
/// change (ADR-0108 decision 4).
#[derive(Debug)]
#[non_exhaustive]
pub enum PictureData {
    /// System memory after the adapter's conversion. `bytes` is not copied by the port.
    System {
        /// Layout of `bytes`.
        layout: PictureLayout,
        /// Colour space of `bytes`.
        color: SampleColor,
        /// Plane bytes.
        bytes: Vec<u8>,
    },
    /// A native surface the render partition already imports for capture
    /// (`IoSurface` on macOS, `D3dSharedHandle` on Windows). It follows the capture
    /// port's native surface rules; the frame's lease returns it.
    Native(rbs_core::media::VideoFrame),
}

/// One picture. Built with [`Picture::system`] or [`Picture::native`], so a native
/// picture always carries a native surface.
#[derive(Debug)]
pub struct Picture {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    data: PictureData,
}

impl Picture {
    /// A straight-alpha sRGB, NV12, or P010 picture in system memory.
    pub fn system(
        width: u32,
        height: u32,
        layout: PictureLayout,
        color: SampleColor,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            width,
            height,
            data: PictureData::System {
                layout,
                color,
                bytes,
            },
        }
    }

    /// A native surface (`IoSurface` or `D3dSharedHandle`). Returns `None` when the
    /// frame's payload is not a native surface (a texture or system memory), which the
    /// port does not accept on this path.
    pub fn native(frame: rbs_core::media::VideoFrame) -> Option<Self> {
        if !matches!(
            frame.payload(),
            rbs_core::media::FramePayload::Native { .. }
        ) {
            return None;
        }
        let size = frame.format().size;
        Some(Self {
            width: size.width,
            height: size.height,
            data: PictureData::Native(frame),
        })
    }

    /// The pixels.
    pub fn data(&self) -> &PictureData {
        &self.data
    }

    /// The pixels, by value: a native surface moves on with its lease (to the
    /// importer, which takes it exactly once).
    pub fn into_data(self) -> PictureData {
        self.data
    }

    /// System-memory payload bytes, counted against [`MAX_FRAME_BYTES`]. A native
    /// surface counts against the surface budget instead and returns 0.
    pub fn system_bytes(&self) -> usize {
        match &self.data {
            PictureData::System { bytes, .. } => bytes.len(),
            PictureData::Native(_) => 0,
        }
    }

    /// True for a native surface.
    pub fn is_native(&self) -> bool {
        matches!(self.data, PictureData::Native(_))
    }
}

/// Interleaved `f32` PCM from the first audio stream. Further streams are ignored by
/// the node.
#[derive(Debug, Clone, PartialEq)]
pub struct InterleavedPcm {
    /// Source sample rate.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// Interleaved samples.
    pub samples: Vec<f32>,
}

/// The next picture of the picture stream. A missing timestamp continues from the
/// previous stamp by `frame_duration`; neither fails the open (`Media`). The adapter
/// has already applied [`frame_duration_rule`].
#[derive(Debug)]
pub struct PictureSample {
    /// Container presentation time, when the container states one.
    pub pts: Option<Duration>,
    /// Container frame duration, when the container states one.
    pub frame_duration: Option<Duration>,
    /// The picture.
    pub picture: Picture,
}

/// The next PCM of the first audio stream.
#[derive(Debug, Clone, PartialEq)]
pub struct SoundSample {
    /// Container presentation time of the first sample, when the container states one.
    /// A missing one continues from the end of the previous sample.
    pub pts: Option<Duration>,
    /// The samples.
    pub pcm: InterleavedPcm,
}

/// The duration an animated picture shows for a stated delay of 0 or 10 ms or less
/// (the browser rule, ADR-0108 decision 4).
pub const SHORT_DELAY_AS: Duration = Duration::from_millis(100);

/// Applies the frame-duration rule of ADR-0108 decision 4. For an animated picture
/// (`animated` true) a stated delay of 10 ms or less reads as 100 ms. For video, a zero
/// duration continues by `nominal`, the stream's nominal frame duration.
pub fn frame_duration_rule(
    stated: Option<Duration>,
    animated: bool,
    nominal: Option<Duration>,
) -> Option<Duration> {
    match stated {
        Some(d) if animated && d <= Duration::from_millis(10) => Some(SHORT_DELAY_AS),
        Some(d) if d.is_zero() => nominal.filter(|n| !n.is_zero()),
        other => other,
    }
}

/// Why decode failed. The host maps this to `SourceError::Media`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The file, codec, or a mid-stream fault. The text is for logs.
    Failed(String),
    /// A sample had neither a timestamp nor a frame duration to continue from.
    NoTimestamp,
    /// A layout the adapter will not convert, or a codec the adapter or the platform
    /// lacks (for example HEVC without the Store extension). The host tries the next
    /// adapter and tells the user what is missing.
    Unsupported(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Failed(text) => f.write_str(text),
            DecodeError::NoTimestamp => {
                f.write_str("the container has neither a timestamp nor a frame duration")
            }
            DecodeError::Unsupported(what) => write!(f, "unsupported {what}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// An open file. The node seeks and reads each stream separately (ADR-0108
/// decision 4). Dropping it releases the file.
///
/// New methods are added with a default, so they do not break an adapter.
pub trait MediaDecoder: Send {
    /// Seeks both streams to `cursor` from the start of the file. Called by the worker,
    /// never from `process`. The next picture is the first at or after `cursor`, or the
    /// keyframe before it when the adapter cannot decode forward; the worker skips
    /// pictures stamped before `cursor`.
    fn seek(&mut self, cursor: Duration) -> Result<(), DecodeError>;

    /// The next picture, or `Ok(None)` at the end of the picture stream.
    fn next_picture(&mut self) -> Result<Option<PictureSample>, DecodeError>;

    /// The next PCM of the first audio stream, or `Ok(None)` at its end or when the
    /// file has no audio stream.
    fn next_sound(&mut self) -> Result<Option<SoundSample>, DecodeError>;

    /// Whether the container has an audio stream. The source still has a sound node
    /// when this is false and writes silence (ADR-0108 decision 1).
    fn has_audio(&self) -> bool;

    /// The container's stated length, read once after open. `None` when it states none.
    fn duration(&self) -> Option<Duration> {
        None
    }
}

/// Where native pictures will be imported, so an adapter that must place its surfaces
/// on one GPU (a shared D3D11 texture on Windows) can choose it. Built with
/// [`DecodeTarget::none`] or [`DecodeTarget::adapter`], so fields can be added.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecodeTarget {
    /// The LUID of the render adapter (the `adapter_luid` of a `NativeSurface`), or
    /// `None` when it is not known: the adapter then gives system pictures, or native
    /// ones only of a kind any device opens (an `IoSurface`).
    pub adapter_luid: Option<u64>,
}

impl DecodeTarget {
    /// No known import device (a probe, or a run the sound node started).
    pub const fn none() -> Self {
        Self { adapter_luid: None }
    }

    /// Native pictures will be imported on the adapter with this LUID.
    pub const fn adapter(luid: u64) -> Self {
        Self {
            adapter_luid: Some(luid),
        }
    }
}

/// Factory registered by a composition root. `open` is only called with a path
/// [`accept_local_media_path`] already accepted.
pub trait MediaDecoderFactory: AdapterFactory {
    /// Opens `path` with no known import device: the same as
    /// [`MediaDecoderFactory::open_for`] with [`DecodeTarget::none`]. The returned
    /// decoder is dropped by the caller when the probe or the worker ends. A probe must
    /// not keep it.
    fn open(&self, path: &Path) -> Result<Box<dyn MediaDecoder>, DecodeError>;

    /// Opens `path` for pictures imported on `target`. The default ignores the target
    /// and calls [`MediaDecoderFactory::open`]. An adapter whose native pictures must
    /// live on the import device overrides it; without a device it gives system
    /// pictures.
    fn open_for(
        &self,
        path: &Path,
        target: &DecodeTarget,
    ) -> Result<Box<dyn MediaDecoder>, DecodeError> {
        let _ = target;
        self.open(path)
    }
}

/// How a stored picture turns to display upright: a track's rotation (an MP4 or
/// QuickTime display matrix, Media Foundation's `MF_MT_VIDEO_ROTATION`). An adapter
/// hands out upright pictures (port invariant 12), turning them itself.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    /// Upright as stored.
    #[default]
    None,
    /// A quarter turn clockwise (a phone held upright, the camera on the left).
    Clockwise90,
    /// A half turn.
    Half,
    /// A quarter turn counter-clockwise.
    Clockwise270,
}

impl Rotation {
    /// The rotation of a turn of `degrees` clockwise, to the nearest quarter turn.
    pub fn from_clockwise_degrees(degrees: f64) -> Self {
        if !degrees.is_finite() {
            return Self::None;
        }
        match ((degrees / 90.0).round() as i64).rem_euclid(4) {
            1 => Self::Clockwise90,
            2 => Self::Half,
            3 => Self::Clockwise270,
            _ => Self::None,
        }
    }

    /// Whether the width and height swap.
    pub fn swaps_sides(self) -> bool {
        matches!(self, Self::Clockwise90 | Self::Clockwise270)
    }

    /// The upright size of a `width` x `height` stored picture.
    pub fn upright(self, width: u32, height: u32) -> (u32, u32) {
        if self.swaps_sides() {
            (height, width)
        } else {
            (width, height)
        }
    }
}

/// Turns a tightly packed 4-byte-per-pixel picture of `width` x `height` upright into
/// `out` (cleared first), for the copy path; returns the upright size. `None` when
/// `src` is shorter than the picture.
pub fn rotate_rgba(
    src: &[u8],
    width: u32,
    height: u32,
    rotation: Rotation,
    out: &mut Vec<u8>,
) -> Option<(u32, u32)> {
    let (w, h) = (width as usize, height as usize);
    let len = w.checked_mul(h)?.checked_mul(4)?;
    let src = src.get(..len)?;
    out.clear();
    let (uw, uh) = rotation.upright(width, height);
    if rotation == Rotation::None {
        out.extend_from_slice(src);
        return Some((uw, uh));
    }
    out.resize(len, 0);
    let ow = uw as usize;
    // Upright row `y` starts at stored pixel `base + y * down`, and each next upright
    // pixel lies `step` stored pixels on: strides, with no choice inside the loop.
    let (w_i, h_i) = (w as isize, h as isize);
    let (base, down, step) = match rotation {
        // Upright (x, y) is stored (y, h - 1 - x).
        Rotation::Clockwise90 => ((h_i - 1) * w_i, 1, -w_i),
        // Upright (x, y) is stored (w - 1 - x, h - 1 - y).
        Rotation::Half => ((h_i - 1) * w_i + w_i - 1, -w_i, -1),
        // Upright (x, y) is stored (w - 1 - y, x).
        _ => (w_i - 1, -1, w_i),
    };
    for (y, row) in out.chunks_exact_mut(ow * 4).enumerate() {
        let mut at = base + y as isize * down;
        for pixel in row.chunks_exact_mut(4) {
            let i = at as usize * 4;
            pixel.copy_from_slice(&src[i..i + 4]);
            at += step;
        }
    }
    Some((uw, uh))
}

#[cfg(test)]
mod rotation_tests {
    use super::*;

    /// A 3 x 2 picture whose pixels are numbered 0..6 in the first byte.
    fn numbered() -> Vec<u8> {
        (0u8..6).flat_map(|n| [n, 0, 0, 255]).collect()
    }

    fn firsts(bytes: &[u8]) -> Vec<u8> {
        bytes.chunks_exact(4).map(|p| p[0]).collect()
    }

    #[test]
    fn each_quarter_turn_moves_the_pixels_clockwise() {
        // 0 1 2
        // 3 4 5
        let mut out = Vec::new();
        let cases = [
            (Rotation::None, (3, 2), vec![0, 1, 2, 3, 4, 5]),
            // 3 0
            // 4 1
            // 5 2
            (Rotation::Clockwise90, (2, 3), vec![3, 0, 4, 1, 5, 2]),
            (Rotation::Half, (3, 2), vec![5, 4, 3, 2, 1, 0]),
            // 2 5
            // 1 4
            // 0 3
            (Rotation::Clockwise270, (2, 3), vec![2, 5, 1, 4, 0, 3]),
        ];
        for (rotation, size, want) in cases {
            assert_eq!(
                rotate_rgba(&numbered(), 3, 2, rotation, &mut out),
                Some(size)
            );
            assert_eq!(firsts(&out), want, "{rotation:?}");
        }
        assert_eq!(
            rotate_rgba(&numbered()[..20], 3, 2, Rotation::Half, &mut out),
            None
        );
    }

    #[test]
    fn degrees_round_to_quarter_turns() {
        assert_eq!(
            Rotation::from_clockwise_degrees(90.0),
            Rotation::Clockwise90
        );
        assert_eq!(
            Rotation::from_clockwise_degrees(-90.0),
            Rotation::Clockwise270
        );
        assert_eq!(Rotation::from_clockwise_degrees(180.0), Rotation::Half);
        assert_eq!(Rotation::from_clockwise_degrees(-180.0), Rotation::Half);
        assert_eq!(Rotation::from_clockwise_degrees(359.0), Rotation::None);
        assert_eq!(Rotation::from_clockwise_degrees(f64::NAN), Rotation::None);
        assert_eq!(Rotation::Clockwise90.upright(160, 90), (90, 160));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_animation_delays_read_as_100_ms() {
        for ms in [0, 1, 10] {
            assert_eq!(
                frame_duration_rule(Some(Duration::from_millis(ms)), true, None),
                Some(SHORT_DELAY_AS)
            );
        }
        let d = Duration::from_millis(20);
        assert_eq!(frame_duration_rule(Some(d), true, None), Some(d));
    }

    #[test]
    fn a_zero_video_duration_continues_by_the_nominal_one() {
        let nominal = Duration::from_micros(16_667);
        assert_eq!(
            frame_duration_rule(Some(Duration::ZERO), false, Some(nominal)),
            Some(nominal)
        );
        assert_eq!(frame_duration_rule(Some(Duration::ZERO), false, None), None);
        let d = Duration::from_millis(5);
        assert_eq!(frame_duration_rule(Some(d), false, Some(nominal)), Some(d));
        assert_eq!(frame_duration_rule(None, false, Some(nominal)), None);
    }

    #[test]
    fn a_system_picture_counts_its_bytes() {
        let picture = Picture::system(2, 2, PictureLayout::Rgba8, SampleColor::Srgb, vec![0; 16]);
        assert_eq!(picture.system_bytes(), 16);
    }
}
