//! The recording's output settings in the app (glossary: output profile): folder,
//! format, encoder, output size, and every encoder setting that changes the
//! recording's quality (rate control and bitrates, quality level, keyframe interval,
//! profile, preset, B-frames, CBR padding). Kept in the `recording` section of the
//! output profile in use (ADR-0051, `user_session`); `recording.toml`, which older
//! versions kept, is read once to make the first profile. Toolkit-free and tested
//! without a window.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rbs_codec::{EncoderPreset, KeyframeInterval, RateControl};
use rbs_core::media::{CodecId, ColorSpace, ContainerId};
use rbs_record::VideoSettings;
use serde::{Deserialize, Serialize};

/// The file format version this build writes and reads.
const FORMAT: u32 = 1;

/// Audio bitrates the AAC encoders take, in kbit/s (Media Foundation AAC accepts only
/// these).
pub const AUDIO_KBPS: [u32; 4] = [96, 128, 160, 192];

/// Video bitrates offered, in kbit/s.
pub const VIDEO_KBPS: [u32; 8] = [2_500, 4_000, 6_000, 8_000, 12_000, 20_000, 40_000, 80_000];

/// H.264 video bitrate recommended at 30 fps and below, by pixel count
/// (width times height). The next step starts above that count. See
/// [`recommended_video_kbps`].
const RECOMMENDED_H264_30: [(u64, u32); 4] = [
    (1_280 * 720, 2_500),
    (1_920 * 1_080, 6_000),
    (2_560 * 1_440, 9_000),
    (u64::MAX, 20_000),
];

/// The video bitrate (kbit/s) recommended for an encoded picture of `width` by
/// `height` at `fps`, for `codec`. This is the warning under Video bitrate
/// (design system, Settings, Recording): it does not change the saved rate.
/// `None` when a side or the frame rate is zero.
///
/// H.264 at 30 fps and below uses [`RECOMMENDED_H264_30`]. Above 30 fps the
/// figure is that value times `(30 + fps) / 60`, and 60 fps and above stay at
/// 1.5 times (the fps used in the scale is capped at 60). HEVC is two thirds
/// of the H.264 figure and AV1 is half, rounded down.
pub fn recommended_video_kbps(width: u32, height: u32, fps: u32, codec: VideoCodec) -> Option<u32> {
    if width == 0 || height == 0 || fps == 0 {
        return None;
    }
    let pixels = u64::from(width) * u64::from(height);
    let base = RECOMMENDED_H264_30
        .iter()
        .find(|(limit, _)| pixels <= *limit)
        .map(|(_, kbps)| *kbps)
        .unwrap_or(20_000);
    let scaled = if fps <= 30 {
        u64::from(base)
    } else {
        let fps = u64::from(fps.min(60));
        u64::from(base) * (30 + fps) / 60
    };
    let (num, den) = match codec {
        VideoCodec::H264 => (1, 1),
        VideoCodec::Hevc => (2, 3),
        VideoCodec::Av1 => (1, 2),
    };
    u32::try_from(scaled * num / den).ok()
}

/// Constant-quality levels offered (0 best ..= 51).
pub const QUALITY_LEVELS: [u8; 10] = [14, 16, 18, 20, 22, 23, 25, 28, 32, 36];

/// Keyframe intervals offered, in seconds.
pub const KEYFRAME_SECONDS: [u32; 6] = [1, 2, 3, 4, 5, 10];

/// Most B-frames a profile may ask for.
pub const MAX_B_FRAMES: u32 = 4;

/// Smallest side of an explicit output size (ADR-0102 decision 7).
pub const OUTPUT_SIZE_MIN: u32 = 2;
/// Largest side of an explicit output size. A canvas may be taller; an absent size
/// keeps that canvas.
pub const OUTPUT_SIZE_MAX: u32 = 7680;
/// Long sides Settings offers, each fitted to the canvas (ADR-0102 decision 4).
pub const OUTPUT_LONG_SIDES: [u32; 4] = [1280, 1920, 2560, 3840];

/// Encoded picture size of one recording or one stream (ADR-0102). Absent in a file
/// and on the wire means the same as that output's canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSize {
    /// Width in pixels. Even, from [`OUTPUT_SIZE_MIN`] to [`OUTPUT_SIZE_MAX`].
    pub width: u32,
    /// Height in pixels. Even, from [`OUTPUT_SIZE_MIN`] to [`OUTPUT_SIZE_MAX`].
    pub height: u32,
}

impl OutputSize {
    /// Both sides.
    pub fn pair(self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// One row of the Output size list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSizeOption {
    /// `None`: same as this output's canvas. Nothing is stored.
    pub size: Option<OutputSize>,
    /// The words in the list.
    pub label: String,
}

/// Why `size` cannot be stored, or `None` when it is absent or both sides are even
/// and in range. Matching the canvas aspect is [`output_size_matches`].
pub fn output_size_problem(size: Option<OutputSize>) -> Option<String> {
    let size = size?;
    let even = size.width.is_multiple_of(2) && size.height.is_multiple_of(2);
    let in_range = (OUTPUT_SIZE_MIN..=OUTPUT_SIZE_MAX).contains(&size.width)
        && (OUTPUT_SIZE_MIN..=OUTPUT_SIZE_MAX).contains(&size.height);
    if even && in_range {
        None
    } else {
        Some(format!(
            "Output size {} x {} must have even sides, each from {OUTPUT_SIZE_MIN} to {OUTPUT_SIZE_MAX}.",
            size.width, size.height
        ))
    }
}

/// The picture for long side `long` on `canvas`. The long side stays on the canvas's
/// long axis (a square canvas stays square). The short side is the even integer
/// closest to the canvas aspect (ADR-0102 decision 4). `None` when a side is outside
/// 2..=7680 or the canvas has a zero side.
pub fn fit_output_size(long: u32, canvas: (u32, u32)) -> Option<OutputSize> {
    if canvas.0 == 0 || canvas.1 == 0 || long == 0 {
        return None;
    }
    let size = if canvas.0 == canvas.1 {
        OutputSize {
            width: long,
            height: long,
        }
    } else if canvas.0 > canvas.1 {
        OutputSize {
            width: long,
            height: fit_short_side(long, canvas.1, canvas.0)?,
        }
    } else {
        OutputSize {
            width: fit_short_side(long, canvas.0, canvas.1)?,
            height: long,
        }
    };
    let in_range = (OUTPUT_SIZE_MIN..=OUTPUT_SIZE_MAX).contains(&size.width)
        && (OUTPUT_SIZE_MIN..=OUTPUT_SIZE_MAX).contains(&size.height);
    let even = size.width.is_multiple_of(2) && size.height.is_multiple_of(2);
    if in_range && even { Some(size) } else { None }
}

/// Even short side for `long` on a canvas whose short and long sides are
/// `canvas_short` and `canvas_long`. A half rounds away from zero; an odd nearest
/// integer takes the closer even neighbour, and a tie keeps the smaller one.
fn fit_short_side(long: u32, canvas_short: u32, canvas_long: u32) -> Option<u32> {
    if canvas_long == 0 {
        return None;
    }
    let num = u64::from(long) * u64::from(canvas_short);
    let den = u64::from(canvas_long);
    let nearest = (2 * num + den) / (2 * den);
    let short = if nearest.is_multiple_of(2) {
        nearest
    } else if num > nearest * den {
        nearest + 1
    } else {
        nearest.saturating_sub(1)
    };
    u32::try_from(short).ok()
}

/// True when `size` is the canvas-aspect fit of its long side and both sides are in
/// range (ADR-0102 decisions 4 and 7).
pub fn output_size_matches(size: (u32, u32), canvas: (u32, u32)) -> bool {
    let long = size.0.max(size.1);
    fit_output_size(long, canvas).is_some_and(|fitted| fitted.pair() == size)
}

/// Output size rows for `canvas`: "Same as canvas", then the long sides
/// [`OUTPUT_LONG_SIDES`] fitted to the canvas. A result equal to the canvas, or
/// outside 2..=7680, is left out. A saved size that is neither is appended so the
/// list still shows it. Fewer pixels than the canvas is "downscale"; more is
/// "upscale".
pub fn output_size_options(canvas: (u32, u32), saved: Option<OutputSize>) -> Vec<OutputSizeOption> {
    let mut options = vec![OutputSizeOption {
        size: None,
        label: "Same as canvas".to_owned(),
    }];
    let canvas_pixels = u64::from(canvas.0) * u64::from(canvas.1);
    for long in OUTPUT_LONG_SIDES {
        let Some(size) = fit_output_size(long, canvas) else {
            continue;
        };
        if size.pair() == canvas {
            continue;
        }
        options.push(OutputSizeOption {
            size: Some(size),
            label: output_size_label(size, canvas_pixels),
        });
    }
    if let Some(size) = saved
        && size.pair() != canvas
        && !options.iter().any(|o| o.size == Some(size))
    {
        options.push(OutputSizeOption {
            size: Some(size),
            label: output_size_label(size, canvas_pixels),
        });
    }
    options
}

/// The chip's title and detail for an explicit output size. A preset long side
/// that keeps the canvas shape is 720p, 1080p, 1440p or 4K; any other size is
/// named by its pixels. The detail carries the pixels and downscale or upscale.
pub fn output_choice_copy(size: OutputSize, canvas: (u32, u32)) -> (String, String) {
    let long = size.width.max(size.height);
    let named = output_size_matches(size.pair(), canvas).then_some(match long {
        1280 => Some("720p"),
        1920 => Some("1080p"),
        2560 => Some("1440p"),
        3840 => Some("4K"),
        _ => None,
    });
    let pixels = format!("{} × {}", size.width, size.height);
    let canvas_pixels = u64::from(canvas.0) * u64::from(canvas.1);
    let area = u64::from(size.width) * u64::from(size.height);
    let scale = if area < canvas_pixels {
        "downscale"
    } else if area > canvas_pixels {
        "upscale"
    } else {
        ""
    };
    match named.flatten() {
        Some(name) if scale.is_empty() => (name.to_owned(), pixels),
        Some(name) => (name.to_owned(), format!("{pixels} · {scale}")),
        None if scale.is_empty() => (pixels, String::new()),
        None => (pixels, scale.to_owned()),
    }
}

fn output_size_label(size: OutputSize, canvas_pixels: u64) -> String {
    let pixels = u64::from(size.width) * u64::from(size.height);
    let scale = if pixels < canvas_pixels {
        "downscale"
    } else if pixels > canvas_pixels {
        "upscale"
    } else {
        return format!("{} x {}", size.width, size.height);
    };
    format!("{} x {} ({scale})", size.width, size.height)
}

/// The first `recording-vertical` section: the main recording section, with no
/// output size. A size fitted to the horizontal canvas is not stored on the
/// vertical one (ADR-0102 decision 1).
pub fn vertical_recording_from_main(main: &RecordProfile) -> RecordProfile {
    let mut copied = main.clone();
    copied.output_size = None;
    copied
}

/// How the encoder spends bits, as the user picks it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateMode {
    /// Constant bitrate.
    #[default]
    Cbr,
    /// Variable bitrate with a peak.
    Vbr,
    /// Constant quality.
    ConstantQuality,
}

impl RateMode {
    /// All, in the order the UI lists them.
    pub const ALL: [RateMode; 3] = [RateMode::Cbr, RateMode::Vbr, RateMode::ConstantQuality];

    /// The name in the UI.
    pub fn label(self) -> &'static str {
        match self {
            RateMode::Cbr => "CBR",
            RateMode::Vbr => "VBR",
            RateMode::ConstantQuality => "Constant quality",
        }
    }

    /// The `codec.video.rate_control` value.
    pub fn capability(self) -> &'static str {
        match self {
            RateMode::Cbr => "cbr",
            RateMode::Vbr => "vbr",
            RateMode::ConstantQuality => "constant_quality",
        }
    }
}

/// Speed versus quality, as the user picks it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    /// Best quality the machine sustains.
    Quality,
    /// The default.
    #[default]
    Balanced,
    /// Least GPU use.
    Speed,
}

impl Preset {
    /// All, in the order the UI lists them.
    pub const ALL: [Preset; 3] = [Preset::Quality, Preset::Balanced, Preset::Speed];

    /// The name in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Preset::Quality => "Quality",
            Preset::Balanced => "Balanced",
            Preset::Speed => "Speed",
        }
    }

    fn encoder(self) -> EncoderPreset {
        match self {
            Preset::Quality => EncoderPreset::Quality,
            Preset::Balanced => EncoderPreset::Balanced,
            Preset::Speed => EncoderPreset::Speed,
        }
    }
}

fn default_quality() -> u8 {
    23
}

fn default_keyframe_seconds() -> u32 {
    2
}

/// PCM widths a WAV recording offers, in the order Settings lists them.
pub const PCM_BITS: [u16; 3] = [16, 24, 32];

/// Labels for [`PCM_BITS`].
pub const PCM_LABELS: [&str; 3] = ["16-bit", "24-bit", "32-bit float"];

/// Integer widths a FLAC recording offers. FLAC has no float samples.
pub const FLAC_BITS: [u16; 2] = [16, 24];

/// Labels for [`FLAC_BITS`].
pub const FLAC_LABELS: [&str; 2] = ["16-bit", "24-bit"];

/// The words for a PCM width on the output card: "PCM 16-bit", "PCM 24-bit" or
/// "PCM 32-bit float". Anything else reads as 16-bit.
pub fn pcm_line(bits: u16) -> &'static str {
    match bits {
        24 => "PCM 24-bit",
        32 => "PCM 32-bit float",
        _ => "PCM 16-bit",
    }
}

fn default_pcm_bits() -> u16 {
    16
}

fn default_video_format() -> Format {
    Format::Mkv
}

fn default_audio_format() -> Format {
    Format::Wav
}

fn is_pcm_16(bits: &u16) -> bool {
    *bits == 16
}

/// The recording's container, as the user picks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// Matroska (`.mkv`): survives a crash.
    Mkv,
    /// Fragmented MP4 (`.mp4`).
    Mp4,
    /// WAV (`.wav`): the mix as PCM, with no picture (ADR-0085).
    Wav,
    /// FLAC (`.flac`): the mix, lossless, with no picture (ADR-0085).
    Flac,
}

impl Format {
    /// All of them, in the order the UI lists them.
    pub const ALL: [Format; 4] = [Format::Mkv, Format::Mp4, Format::Wav, Format::Flac];

    /// The container.
    pub fn container(self) -> ContainerId {
        match self {
            Format::Mkv => ContainerId::Matroska,
            Format::Mp4 => ContainerId::Mp4Fragmented,
            Format::Wav => ContainerId::Wav,
            Format::Flac => ContainerId::Flac,
        }
    }

    /// The file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Mkv => "mkv",
            Format::Mp4 => "mp4",
            Format::Wav => "wav",
            Format::Flac => "flac",
        }
    }

    /// The name in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Format::Mkv => "MKV",
            Format::Mp4 => "MP4",
            Format::Wav => "WAV",
            Format::Flac => "FLAC",
        }
    }

    /// WAV and FLAC are one audio file of the mix. MKV and MP4 carry a picture.
    pub fn is_audio_only(self) -> bool {
        matches!(self, Format::Wav | Format::Flac)
    }

    /// Video containers, in the order the container menu lists them.
    pub const VIDEO: [Format; 2] = [Format::Mkv, Format::Mp4];

    /// Audio containers, in the order the container menu lists them.
    pub const AUDIO: [Format; 2] = [Format::Wav, Format::Flac];

    /// The containers of this format's category.
    pub fn category(self) -> &'static [Format] {
        if self.is_audio_only() {
            &Self::AUDIO
        } else {
            &Self::VIDEO
        }
    }

    /// This format's place in [`Format::category`].
    pub fn category_index(self) -> usize {
        self.category().iter().position(|f| *f == self).unwrap_or(0)
    }

    /// Sample widths this format offers, in UI order. MKV and MP4 do not use them.
    pub fn widths(self) -> &'static [u16] {
        match self {
            Format::Flac => &FLAC_BITS,
            _ => &PCM_BITS,
        }
    }

    /// Labels for [`Format::widths`].
    pub fn width_labels(self) -> &'static [&'static str] {
        match self {
            Format::Flac => &FLAC_LABELS,
            _ => &PCM_LABELS,
        }
    }

    /// The audio codec an audio-only recording of this format opens.
    pub fn audio_codec(self) -> CodecId {
        match self {
            Format::Flac => CodecId::Flac,
            _ => CodecId::Pcm,
        }
    }
}

/// The words for an audio-only recording's width on the output card.
/// WAV reads "PCM 16-bit". FLAC reads "16-bit" or "24-bit".
pub fn audio_fact(format: Format, bits: u16) -> &'static str {
    match format {
        Format::Flac => match bits {
            24 => "24-bit",
            _ => "16-bit",
        },
        _ => pcm_line(bits),
    }
}

/// The video codec a recording asks for. H.264 is the default and is omitted from
/// saved files. HEVC and AV1 are offered only where an encoder lists them. A stream
/// stays H.264: RTMP and MPEG-TS do not carry AV1 yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    /// H.264.
    #[default]
    H264,
    /// HEVC (H.265), 8-bit.
    Hevc,
    /// AV1 Main, 8-bit.
    Av1,
}

impl VideoCodec {
    /// All of them, in the order the UI lists them.
    pub const ALL: [VideoCodec; 3] = [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1];

    /// True for [`VideoCodec::H264`], so a saved file can omit the field.
    pub fn is_h264(self) -> bool {
        self == Self::H264
    }

    /// The name in the UI.
    pub fn label(self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::Hevc => "HEVC",
            VideoCodec::Av1 => "AV1",
        }
    }

    /// The codec id the encoder and the muxer take.
    pub fn id(self) -> CodecId {
        match self {
            VideoCodec::H264 => CodecId::H264,
            VideoCodec::Hevc => CodecId::Hevc,
            VideoCodec::Av1 => CodecId::Av1,
        }
    }

    /// True when `profile` is a name this codec can ask an encoder for. `None` is
    /// automatic and always allowed.
    pub fn allows_profile(self, profile: Option<&str>) -> bool {
        matches!(
            (self, profile),
            (_, None)
                | (VideoCodec::H264, Some("high" | "main" | "baseline"))
                | (VideoCodec::Hevc, Some("main" | "main10"))
                | (VideoCodec::Av1, Some("main"))
        )
    }
}

/// How an output encodes colour. Absent in a saved profile means [`Delivery::Video`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    /// Rec. 709 camera encoding, limited range. YouTube, TV and streams.
    #[default]
    Video,
    /// sRGB transfer, full range. Matches a desktop display.
    Screen,
}

impl Delivery {
    /// True for [`Delivery::Video`], so a saved profile can omit the field.
    pub fn is_video(self) -> bool {
        self == Self::Video
    }

    /// The colour description the export and the encoder signal.
    pub fn color_space(self) -> ColorSpace {
        match self {
            Self::Video => ColorSpace::BT709_LIMITED,
            Self::Screen => ColorSpace::SRGB_VIDEO,
        }
    }
}

fn is_video(delivery: &Delivery) -> bool {
    delivery.is_video()
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_h264(codec: &VideoCodec) -> bool {
    codec.is_h264()
}

/// The encoder settings of one output, a recording or a stream: the encoder and every
/// setting that changes the output's quality. `stream.toml` keeps them as its
/// `[encoding]` table; `recording.toml` keeps the same fields at its top level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Encoding {
    /// The video encoder's adapter id, or `None` for the best one this system has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder: Option<String>,
    /// H.264, or HEVC or AV1 where an encoder lists it. Omitted when H.264.
    #[serde(default, skip_serializing_if = "is_h264")]
    pub video_codec: VideoCodec,
    /// How the encoder spends bits.
    #[serde(default)]
    pub rate_control: RateMode,
    /// Video bitrate in kbit/s: the CBR rate, or the VBR target.
    pub video_kbps: u32,
    /// VBR peak in kbit/s; `None` is 1.5 times the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_video_kbps: Option<u32>,
    /// Constant-quality level, 0 (best) ..= 51.
    #[serde(default = "default_quality")]
    pub quality: u8,
    /// Seconds between keyframes.
    #[serde(default = "default_keyframe_seconds")]
    pub keyframe_seconds: u32,
    /// Codec profile (`high`, `main`, `baseline`); `None` lets the encoder pick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Speed versus quality.
    #[serde(default)]
    pub preset: Preset,
    /// Most B-frames; encoders with fewer use fewer.
    #[serde(default)]
    pub b_frames: u32,
    /// With CBR, pad the video to its full bitrate (filler data).
    #[serde(default)]
    pub cbr_padding: bool,
    /// Audio bitrate in kbit/s, one of [`AUDIO_KBPS`].
    pub audio_kbps: u32,
    /// Colour encoding. Omitted when [`Delivery::Video`].
    #[serde(default, skip_serializing_if = "is_video")]
    pub delivery: Delivery,
    /// Lifts only the darkest tones on a Video encode. Omitted when off. Has no
    /// effect while [`Delivery::Screen`] is selected; the choice is kept.
    #[serde(default, skip_serializing_if = "is_false")]
    pub open_shadows: bool,
}

impl Encoding {
    /// The best encoder, CBR at `video_kbps` without padding, a keyframe every 2 s, the
    /// encoder's profile, the balanced preset, no B-frames and 160 kbit/s audio.
    pub fn new(video_kbps: u32) -> Self {
        Self {
            encoder: None,
            video_codec: VideoCodec::H264,
            rate_control: RateMode::Cbr,
            video_kbps,
            max_video_kbps: None,
            quality: default_quality(),
            keyframe_seconds: default_keyframe_seconds(),
            profile: None,
            preset: Preset::Balanced,
            b_frames: 0,
            cbr_padding: false,
            audio_kbps: 160,
            delivery: Delivery::Video,
            open_shadows: false,
        }
    }

    /// The VBR peak in kbit/s: the saved one, or 1.5 times the target.
    pub fn peak_kbps(&self) -> u32 {
        self.max_video_kbps
            .unwrap_or(self.video_kbps.saturating_mul(3) / 2)
    }

    /// The video encoding these settings ask for. Padding makes CBR `cbr_padded`; it
    /// stays saved while another mode is picked and does nothing there.
    pub fn video_settings(&self) -> VideoSettings {
        let mut v = VideoSettings::default();
        v.codec = self.video_codec.id();
        let bps = |kbps: u32| u64::from(kbps) * 1000;
        v.rate_control = match self.rate_control {
            RateMode::Cbr if self.cbr_padding => RateControl::CbrPadded {
                bitrate_bps: bps(self.video_kbps),
            },
            RateMode::Cbr => RateControl::Cbr {
                bitrate_bps: bps(self.video_kbps),
            },
            RateMode::Vbr => RateControl::Vbr {
                target_bps: bps(self.video_kbps),
                max_bps: bps(self.peak_kbps()),
            },
            RateMode::ConstantQuality => RateControl::ConstantQuality {
                level: self.quality,
            },
        };
        v.keyframe_interval =
            KeyframeInterval::Duration(Duration::from_secs(u64::from(self.keyframe_seconds)));
        v.max_b_frames = self.b_frames;
        v.profile = self.profile.clone();
        v.preset = self.preset.encoder();
        v.color = self.delivery.color_space();
        v.open_shadows = self.delivery.is_video() && self.open_shadows;
        v
    }

    /// The audio bitrate in bit/s.
    pub fn audio_bps(&self) -> u64 {
        u64::from(self.audio_kbps) * 1000
    }

    /// The summary's video part: "video 12000 kbit/s", "video 12000 kbit/s padded",
    /// "video VBR 12000 kbit/s, up to 18000" or "video quality 23".
    pub fn video_summary(&self) -> String {
        match self.rate_control {
            RateMode::Cbr if self.cbr_padding => format!("video {} kbit/s padded", self.video_kbps),
            RateMode::Cbr => format!("video {} kbit/s", self.video_kbps),
            RateMode::Vbr => format!(
                "video VBR {} kbit/s, up to {}",
                self.video_kbps,
                self.peak_kbps()
            ),
            RateMode::ConstantQuality => format!("video quality {}", self.quality),
        }
    }

    /// Why these settings cannot be used, in words, or `None`.
    pub fn problem(&self) -> Option<String> {
        if !AUDIO_KBPS.contains(&self.audio_kbps) {
            return Some(format!(
                "Audio bitrate {} kbit/s is not supported; use 96, 128, 160 or 192.",
                self.audio_kbps
            ));
        }
        if !(500..=200_000).contains(&self.video_kbps) {
            return Some(format!(
                "Video bitrate {} kbit/s is outside 500 to 200000.",
                self.video_kbps
            ));
        }
        if self.rate_control == RateMode::Vbr && self.peak_kbps() < self.video_kbps {
            return Some(format!(
                "The maximum bitrate {} kbit/s is below the bitrate {} kbit/s.",
                self.peak_kbps(),
                self.video_kbps
            ));
        }
        if self.quality > 51 {
            return Some(format!(
                "Quality level {} is outside 0 (best) to 51.",
                self.quality
            ));
        }
        if !(1..=60).contains(&self.keyframe_seconds) {
            return Some(format!(
                "A keyframe every {} s is outside 1 to 60 s.",
                self.keyframe_seconds
            ));
        }
        if self.b_frames > MAX_B_FRAMES {
            return Some(format!(
                "{} B-frames is more than {MAX_B_FRAMES}.",
                self.b_frames
            ));
        }
        None
    }
}

/// What [`RecordProfile::recording_start`] asks a recording to open.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingStart {
    /// The container.
    pub container: ContainerId,
    /// `None`: audio only, no picture encoder.
    pub video: Option<VideoSettings>,
    /// Audio bitrate in bit/s. Zero for PCM.
    pub audio_bitrate_bps: u64,
    /// PCM width when `video` is `None`.
    pub bit_depth: Option<u16>,
    /// The video encoder to try alone. `None` for WAV, or for the best encoder.
    pub encoder: Option<String>,
    /// Encoded picture size. `None` for WAV, and for "same as canvas".
    pub size: Option<(u32, u32)>,
}

/// What the next recording uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordProfile {
    /// File format version (1).
    pub format_version: u32,
    /// Where recordings are written. A profile exported from another computer has
    /// none (ADR-0051): this computer's default is used.
    #[serde(default = "default_folder")]
    pub folder: PathBuf,
    /// The container of the next recording.
    pub format: Format,
    /// The video container to restore when the category returns to Video.
    #[serde(default = "default_video_format")]
    pub video_format: Format,
    /// The audio container to restore when the category returns to Audio.
    #[serde(default = "default_audio_format")]
    pub audio_format: Format,
    /// The video encoder's adapter id, or `None` for the best one this system has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder: Option<String>,
    /// H.264, or HEVC or AV1 where an encoder lists it. Omitted when H.264.
    #[serde(default, skip_serializing_if = "is_h264")]
    pub video_codec: VideoCodec,
    /// How the encoder spends bits. Fields below that a file from an earlier version
    /// lacks take their defaults.
    #[serde(default)]
    pub rate_control: RateMode,
    /// Video bitrate in kbit/s: the CBR rate, or the VBR target.
    pub video_kbps: u32,
    /// VBR peak in kbit/s; `None` is 1.5 times the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_video_kbps: Option<u32>,
    /// Constant-quality level, 0 (best) ..= 51.
    #[serde(default = "default_quality")]
    pub quality: u8,
    /// Seconds between keyframes.
    #[serde(default = "default_keyframe_seconds")]
    pub keyframe_seconds: u32,
    /// Codec profile (`high`, `main`, `baseline`); `None` lets the encoder pick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Speed versus quality.
    #[serde(default)]
    pub preset: Preset,
    /// Most B-frames; encoders with fewer use fewer.
    #[serde(default)]
    pub b_frames: u32,
    /// With CBR, pad the video to its full bitrate (filler data).
    #[serde(default)]
    pub cbr_padding: bool,
    /// Audio bitrate in kbit/s, one of [`AUDIO_KBPS`]. Unused while the format is WAV.
    pub audio_kbps: u32,
    /// PCM sample width of a WAV recording: 16, 24 or 32 (float). Omitted when 16,
    /// so an older file stays 16-bit. Ignored for MKV and MP4, and kept when the
    /// format changes back.
    #[serde(default = "default_pcm_bits", skip_serializing_if = "is_pcm_16")]
    pub bit_depth: u16,
    /// Colour encoding. Omitted when [`Delivery::Video`].
    #[serde(default, skip_serializing_if = "is_video")]
    pub delivery: Delivery,
    /// Lifts only the darkest tones on a Video encode. Omitted when off.
    #[serde(default, skip_serializing_if = "is_false")]
    pub open_shadows: bool,
    /// Encoded picture size. Omitted until the user picks one: absent means this
    /// recording's canvas (ADR-0102).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_size: Option<OutputSize>,
}

impl RecordProfile {
    /// MKV in the user's videos folder, the best encoder, CBR 12 Mbit/s video without
    /// padding, a keyframe every 2 s, the encoder's profile, the balanced preset, no
    /// B-frames and 160 kbit/s audio.
    pub fn new() -> Self {
        let mut p = Self {
            format_version: FORMAT,
            folder: default_folder(),
            format: Format::Mkv,
            video_format: Format::Mkv,
            audio_format: Format::Wav,
            encoder: None,
            video_codec: VideoCodec::H264,
            rate_control: RateMode::Cbr,
            video_kbps: 0,
            max_video_kbps: None,
            quality: 0,
            keyframe_seconds: 0,
            profile: None,
            preset: Preset::Balanced,
            b_frames: 0,
            cbr_padding: false,
            audio_kbps: 0,
            bit_depth: default_pcm_bits(),
            delivery: Delivery::Video,
            open_shadows: false,
            output_size: None,
        };
        p.set_encoding(Encoding::new(12_000));
        p
    }

    /// The encoder settings.
    pub fn encoding(&self) -> Encoding {
        Encoding {
            encoder: self.encoder.clone(),
            video_codec: self.video_codec,
            rate_control: self.rate_control,
            video_kbps: self.video_kbps,
            max_video_kbps: self.max_video_kbps,
            quality: self.quality,
            keyframe_seconds: self.keyframe_seconds,
            profile: self.profile.clone(),
            preset: self.preset,
            b_frames: self.b_frames,
            cbr_padding: self.cbr_padding,
            audio_kbps: self.audio_kbps,
            delivery: self.delivery,
            open_shadows: self.open_shadows,
        }
    }

    /// Replaces the encoder settings.
    pub fn set_encoding(&mut self, e: Encoding) {
        let Encoding {
            encoder,
            video_codec,
            rate_control,
            video_kbps,
            max_video_kbps,
            quality,
            keyframe_seconds,
            profile,
            preset,
            b_frames,
            cbr_padding,
            audio_kbps,
            delivery,
            open_shadows,
        } = e;
        self.encoder = encoder;
        self.video_codec = video_codec;
        self.rate_control = rate_control;
        self.video_kbps = video_kbps;
        self.max_video_kbps = max_video_kbps;
        self.quality = quality;
        self.keyframe_seconds = keyframe_seconds;
        self.profile = profile;
        self.preset = preset;
        self.b_frames = b_frames;
        self.cbr_padding = cbr_padding;
        self.audio_kbps = audio_kbps;
        self.delivery = delivery;
        self.open_shadows = open_shadows;
    }

    /// The VBR peak in kbit/s: the saved one, or 1.5 times the target.
    pub fn peak_kbps(&self) -> u32 {
        self.encoding().peak_kbps()
    }

    /// The video encoding these settings ask for (see [`Encoding::video_settings`]).
    pub fn video_settings(&self) -> VideoSettings {
        self.encoding().video_settings()
    }

    /// Selects `format` and remembers it for its category. FLAC has no 32-bit float,
    /// so a WAV width of 32 becomes 16. Video settings are left as they are.
    pub fn use_format(&mut self, format: Format) {
        self.format = format;
        if format == Format::Flac && !FLAC_BITS.contains(&self.bit_depth) {
            self.bit_depth = default_pcm_bits();
        }
        self.remember_active();
    }

    /// Video (`false`) or Audio (`true`). The other category comes back as it was.
    pub fn use_media(&mut self, audio: bool) {
        if audio == self.format.is_audio_only() {
            return;
        }
        let next = if audio {
            self.audio_format
        } else {
            self.video_format
        };
        let next = if next.is_audio_only() == audio {
            next
        } else if audio {
            Format::Wav
        } else {
            Format::Mkv
        };
        self.use_format(next);
    }

    /// The active container is the one its category restores.
    pub fn remember_active(&mut self) {
        if self.format.is_audio_only() {
            self.audio_format = self.format;
        } else {
            self.video_format = self.format;
        }
    }

    /// Why this profile cannot be used, in words, or `None`.
    pub fn problem(&self) -> Option<String> {
        if self.format_version != FORMAT {
            return Some(format!(
                "settings version {} is not supported",
                self.format_version
            ));
        }
        if !self.folder.is_absolute() {
            return Some("The folder must be a full path, for example C:\\Videos.".into());
        }
        if self.format == Format::Flac {
            return (!FLAC_BITS.contains(&self.bit_depth))
                .then(|| format!("FLAC width {} is not 16-bit or 24-bit.", self.bit_depth));
        }
        if self.format == Format::Wav {
            return (!PCM_BITS.contains(&self.bit_depth)).then(|| {
                format!(
                    "PCM width {} is not 16-bit, 24-bit or 32-bit float.",
                    self.bit_depth
                )
            });
        }
        self.encoding()
            .problem()
            .or_else(|| output_size_problem(self.output_size))
    }

    /// What a recording of this profile opens. WAV is PCM of the mix and FLAC is
    /// the same mix compressed: no picture, no video encoder and no output size.
    /// `encoder` is the video encoder the UI picked; an audio-only format does not
    /// use it.
    pub fn recording_start(
        &self,
        encoder: Option<String>,
        size: Option<(u32, u32)>,
    ) -> RecordingStart {
        if self.format.is_audio_only() {
            RecordingStart {
                container: self.format.container(),
                video: None,
                audio_bitrate_bps: 0,
                bit_depth: Some(self.bit_depth),
                encoder: None,
                size: None,
            }
        } else {
            RecordingStart {
                container: self.format.container(),
                video: Some(self.video_settings()),
                audio_bitrate_bps: u64::from(self.audio_kbps) * 1000,
                bit_depth: None,
                encoder,
                size,
            }
        }
    }

    /// The file a recording started at `now` (local time) is written to:
    /// `RBS 2026-11-03 21-30-05.mkv` in the folder.
    pub fn file_for(&self, now: chrono::NaiveDateTime) -> PathBuf {
        self.folder.join(format!(
            "RBS {}.{}",
            now.format("%Y-%m-%d %H-%M-%S"),
            self.format.extension()
        ))
    }

    /// The file of a vertical canvas's recording started at `now` (ADR-0093
    /// decision 6): `RBS <date> <time> vertical.<ext>` in the folder.
    pub fn vertical_file_for(&self, now: chrono::NaiveDateTime) -> PathBuf {
        self.folder.join(format!(
            "RBS {} vertical.{}",
            now.format("%Y-%m-%d %H-%M-%S"),
            self.format.extension()
        ))
    }
}

/// The id of the one recording profile the app has today, in the control API.
pub const API_PROFILE_ID: &str = "default";

impl RecordProfile {
    /// These settings as the control API's recording profile.
    pub fn to_api(&self) -> rbs_api::types::RecordingProfile {
        use rbs_api::types as api;
        api::RecordingProfile {
            id: API_PROFILE_ID.into(),
            name: "Recording".into(),
            folder: self.folder.display().to_string(),
            container: match self.format {
                Format::Mkv => api::Container::Mkv,
                Format::Mp4 => api::Container::Mp4,
                Format::Wav => api::Container::Wav,
                Format::Flac => api::Container::Flac,
            },
            encoder: self.encoder.clone(),
            video_codec: match self.video_codec {
                VideoCodec::H264 => api::VideoCodec::H264,
                VideoCodec::Hevc => api::VideoCodec::Hevc,
                VideoCodec::Av1 => api::VideoCodec::Av1,
            },
            rate_control: match self.rate_control {
                RateMode::Cbr => api::RateMode::Cbr,
                RateMode::Vbr => api::RateMode::Vbr,
                RateMode::ConstantQuality => api::RateMode::ConstantQuality,
            },
            video_kbps: self.video_kbps,
            max_video_kbps: self.max_video_kbps,
            quality: self.quality,
            keyframe_seconds: self.keyframe_seconds,
            profile: self.profile.clone(),
            preset: match self.preset {
                Preset::Quality => api::Preset::Quality,
                Preset::Balanced => api::Preset::Balanced,
                Preset::Speed => api::Preset::Speed,
            },
            b_frames: self.b_frames,
            cbr_padding: self.cbr_padding,
            audio_kbps: self.audio_kbps,
            bit_depth: self.bit_depth,
            delivery: match self.delivery {
                Delivery::Video => api::Delivery::Video,
                Delivery::Screen => api::Delivery::Screen,
            },
            open_shadows: self.open_shadows,
            output_size: self.output_size.map(|s| api::Dimensions {
                width: s.width,
                height: s.height,
            }),
        }
    }

    /// The listing `output.profiles` shows (no folder: that needs `admin`).
    pub fn to_api_listing(&self) -> rbs_api::types::OutputProfile {
        let p = self.to_api();
        rbs_api::types::OutputProfile {
            id: p.id,
            name: p.name,
            container: p.container,
            encoder: p.encoder,
            rate_control: p.rate_control,
            video_kbps: p.video_kbps,
            audio_kbps: p.audio_kbps,
        }
    }

    /// Settings from the control API's recording profile. The caller checks
    /// [`RecordProfile::problem`].
    pub fn from_api(p: &rbs_api::types::RecordingProfile) -> Self {
        use rbs_api::types as api;
        let mut profile = Self {
            format_version: FORMAT,
            folder: PathBuf::from(&p.folder),
            format: match p.container {
                api::Container::Mkv => Format::Mkv,
                api::Container::Mp4 => Format::Mp4,
                api::Container::Wav => Format::Wav,
                api::Container::Flac => Format::Flac,
            },
            video_format: Format::Mkv,
            audio_format: Format::Wav,
            encoder: p.encoder.clone(),
            video_codec: match p.video_codec {
                api::VideoCodec::H264 => VideoCodec::H264,
                api::VideoCodec::Hevc => VideoCodec::Hevc,
                api::VideoCodec::Av1 => VideoCodec::Av1,
            },
            rate_control: match p.rate_control {
                api::RateMode::Cbr => RateMode::Cbr,
                api::RateMode::Vbr => RateMode::Vbr,
                api::RateMode::ConstantQuality => RateMode::ConstantQuality,
            },
            video_kbps: p.video_kbps,
            max_video_kbps: p.max_video_kbps,
            quality: p.quality,
            keyframe_seconds: p.keyframe_seconds,
            profile: p.profile.clone(),
            preset: match p.preset {
                api::Preset::Quality => Preset::Quality,
                api::Preset::Balanced => Preset::Balanced,
                api::Preset::Speed => Preset::Speed,
            },
            b_frames: p.b_frames,
            cbr_padding: p.cbr_padding,
            audio_kbps: p.audio_kbps,
            bit_depth: p.bit_depth,
            delivery: match p.delivery {
                api::Delivery::Video => Delivery::Video,
                api::Delivery::Screen => Delivery::Screen,
            },
            open_shadows: p.open_shadows,
            output_size: p.output_size.map(|s| OutputSize {
                width: s.width,
                height: s.height,
            }),
        };
        profile.remember_active();
        profile
    }
}

impl Default for RecordProfile {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoding {
    /// These settings as the control API's encoding (stream settings).
    pub fn to_api(&self) -> rbs_api::types::Encoding {
        use rbs_api::types as api;
        api::Encoding {
            encoder: self.encoder.clone(),
            video_codec: match self.video_codec {
                VideoCodec::H264 => api::VideoCodec::H264,
                VideoCodec::Hevc => api::VideoCodec::Hevc,
                VideoCodec::Av1 => api::VideoCodec::Av1,
            },
            rate_control: match self.rate_control {
                RateMode::Cbr => api::RateMode::Cbr,
                RateMode::Vbr => api::RateMode::Vbr,
                RateMode::ConstantQuality => api::RateMode::ConstantQuality,
            },
            video_kbps: self.video_kbps,
            max_video_kbps: self.max_video_kbps,
            quality: self.quality,
            keyframe_seconds: self.keyframe_seconds,
            profile: self.profile.clone(),
            preset: match self.preset {
                Preset::Quality => api::Preset::Quality,
                Preset::Balanced => api::Preset::Balanced,
                Preset::Speed => api::Preset::Speed,
            },
            b_frames: self.b_frames,
            cbr_padding: self.cbr_padding,
            audio_kbps: self.audio_kbps,
            delivery: match self.delivery {
                Delivery::Video => api::Delivery::Video,
                Delivery::Screen => api::Delivery::Screen,
            },
            open_shadows: self.open_shadows,
        }
    }

    /// Settings from the control API's encoding. The caller checks
    /// [`Encoding::problem`].
    pub fn from_api(e: &rbs_api::types::Encoding) -> Self {
        use rbs_api::types as api;
        Self {
            encoder: e.encoder.clone(),
            video_codec: match e.video_codec {
                api::VideoCodec::H264 => VideoCodec::H264,
                api::VideoCodec::Hevc => VideoCodec::Hevc,
                api::VideoCodec::Av1 => VideoCodec::Av1,
            },
            rate_control: match e.rate_control {
                api::RateMode::Cbr => RateMode::Cbr,
                api::RateMode::Vbr => RateMode::Vbr,
                api::RateMode::ConstantQuality => RateMode::ConstantQuality,
            },
            video_kbps: e.video_kbps,
            max_video_kbps: e.max_video_kbps,
            quality: e.quality,
            keyframe_seconds: e.keyframe_seconds,
            profile: e.profile.clone(),
            preset: match e.preset {
                api::Preset::Quality => Preset::Quality,
                api::Preset::Balanced => Preset::Balanced,
                api::Preset::Speed => Preset::Speed,
            },
            b_frames: e.b_frames,
            cbr_padding: e.cbr_padding,
            audio_kbps: e.audio_kbps,
            delivery: match e.delivery {
                api::Delivery::Video => Delivery::Video,
                api::Delivery::Screen => Delivery::Screen,
            },
            open_shadows: e.open_shadows,
        }
    }
}

/// The user's videos folder: `%USERPROFILE%\Videos` on Windows, `~/Movies` on macOS,
/// `~/Videos` elsewhere; the current directory if no home is known.
pub fn default_folder() -> PathBuf {
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    };
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return std::env::current_dir().unwrap_or_default();
    };
    let sub = if cfg!(target_os = "macos") {
        "Movies"
    } else {
        "Videos"
    };
    PathBuf::from(home).join(sub)
}

fn file(config: &Path) -> PathBuf {
    config.join("recording.toml")
}

/// The saved profile; the defaults when there is none. A file that cannot be read gives
/// the defaults and the reason.
pub fn load(config: &Path) -> (RecordProfile, Option<String>) {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (RecordProfile::new(), None),
        Err(e) => {
            return (
                RecordProfile::new(),
                Some(format!("{}: {e}", path.display())),
            );
        }
    };
    match toml::from_str::<RecordProfile>(&text) {
        Ok(mut p) => {
            p.remember_active();
            match p.problem() {
                None => (p, None),
                Some(why) => (
                    RecordProfile::new(),
                    Some(format!("{}: {why}", path.display())),
                ),
            }
        }
        Err(e) => (
            RecordProfile::new(),
            Some(format!("{}: {e}", path.display())),
        ),
    }
}

/// Writes the profile (replacing the file in one step).
pub fn save(config: &Path, profile: &RecordProfile) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(profile).map_err(std::io::Error::other)?;
    let path = file(config);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use rbs_core::media::ColorSpace;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_api_profile_round_trips() {
        for (format, rate, preset) in [
            (Format::Mkv, RateMode::Cbr, Preset::Quality),
            (Format::Mp4, RateMode::Vbr, Preset::Balanced),
            (Format::Mkv, RateMode::ConstantQuality, Preset::Speed),
        ] {
            let mut p = RecordProfile::new();
            p.folder = PathBuf::from("rec");
            p.use_format(format);
            p.rate_control = rate;
            p.preset = preset;
            p.encoder = Some("org.rbs.codec.nvenc".into());
            p.max_video_kbps = Some(20_000);
            p.profile = Some("high".into());
            p.b_frames = 2;
            p.cbr_padding = true;
            let api = p.to_api();
            assert_eq!(api.id, API_PROFILE_ID);
            assert_eq!(RecordProfile::from_api(&api), p);
            let listing = p.to_api_listing();
            assert_eq!(listing.video_kbps, p.video_kbps);
            assert_eq!(listing.encoder, p.encoder);
        }
    }

    #[test]
    fn the_recommended_bitrate_follows_the_output_picture() {
        let h264 = VideoCodec::H264;
        assert_eq!(recommended_video_kbps(1920, 1080, 30, h264), Some(6_000));
        assert_eq!(recommended_video_kbps(2560, 1440, 30, h264), Some(9_000));
        assert_eq!(recommended_video_kbps(1920, 1080, 60, h264), Some(9_000));
        assert_eq!(recommended_video_kbps(3840, 2160, 30, h264), Some(20_000));
        assert_eq!(recommended_video_kbps(1280, 720, 30, h264), Some(2_500));
        // 24 fps stays on the 30 fps figure. 50 fps is (30 + 50) / 60.
        assert_eq!(recommended_video_kbps(1920, 1080, 24, h264), Some(6_000));
        assert_eq!(recommended_video_kbps(1920, 1080, 50, h264), Some(8_000));
        // A vertical 1080p picture has the same pixel count as 1920 x 1080.
        assert_eq!(recommended_video_kbps(1080, 1920, 30, h264), Some(6_000));
        assert_eq!(
            recommended_video_kbps(2560, 1440, 30, VideoCodec::Hevc),
            Some(6_000)
        );
        assert_eq!(
            recommended_video_kbps(2560, 1440, 30, VideoCodec::Av1),
            Some(4_500)
        );
        assert_eq!(recommended_video_kbps(2560, 1440, 60, h264), Some(13_500));
        assert_eq!(recommended_video_kbps(0, 1080, 30, h264), None);
        assert_eq!(recommended_video_kbps(1920, 1080, 0, h264), None);
    }

    #[test]
    fn defaults_are_valid() {
        let p = RecordProfile::new();
        assert_eq!(p.problem(), None, "{p:?}");
        assert!(VIDEO_KBPS.contains(&p.video_kbps));
        assert!(AUDIO_KBPS.contains(&p.audio_kbps));
    }

    #[test]
    fn a_saved_profile_loads_back() {
        let d = dir("profile");
        assert_eq!(load(&d), (RecordProfile::new(), None));
        let mut p = RecordProfile::new();
        p.folder = d.join("rec");
        p.use_format(Format::Mp4);
        p.encoder = Some("org.rbs.codec.nvenc".into());
        p.video_kbps = 8_000;
        p.audio_kbps = 192;
        p.rate_control = RateMode::Vbr;
        p.max_video_kbps = Some(20_000);
        p.quality = 18;
        p.keyframe_seconds = 4;
        p.profile = Some("main".into());
        p.preset = Preset::Quality;
        p.b_frames = 2;
        p.cbr_padding = true;
        save(&d, &p).unwrap();
        assert_eq!(load(&d), (p, None));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_bad_file_gives_the_defaults_and_why() {
        let d = dir("bad-profile");
        std::fs::create_dir_all(&d).unwrap();
        let mut p = RecordProfile::new();
        p.folder = d.clone();
        p.audio_kbps = 100;
        std::fs::write(file(&d), toml::to_string(&p).unwrap()).unwrap();
        let (got, why) = load(&d);
        assert_eq!(got, RecordProfile::new());
        assert!(why.unwrap().contains("96, 128, 160 or 192"));
        std::fs::write(file(&d), "not toml [").unwrap();
        assert!(load(&d).1.is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_from_before_the_encoder_settings_loads_with_their_defaults() {
        let d = dir("v1-profile");
        std::fs::create_dir_all(&d).unwrap();
        let folder = d.join("rec");
        let text = format!(
            "format_version = 1\nfolder = {:?}\nformat = \"mp4\"\nvideo_kbps = 8000\naudio_kbps = 128\n",
            folder.display().to_string()
        );
        std::fs::write(file(&d), text).unwrap();
        let (p, why) = load(&d);
        assert_eq!(why, None);
        let mut want = RecordProfile::new();
        want.folder = folder;
        want.use_format(Format::Mp4);
        want.video_kbps = 8_000;
        want.audio_kbps = 128;
        assert_eq!(p, want);
        assert_eq!(p.delivery, Delivery::Video);
        assert_eq!(p.output_size, None, "an old file encodes at its canvas");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn screen_delivery_is_saved_and_video_is_omitted() {
        let d = dir("delivery");
        let mut p = RecordProfile::new();
        p.folder = d.join("rec");
        p.delivery = Delivery::Screen;
        save(&d, &p).unwrap();
        let text = std::fs::read_to_string(file(&d)).unwrap();
        assert!(text.contains("delivery = \"screen\""));
        assert_eq!(load(&d).0.delivery, Delivery::Screen);
        assert_eq!(p.video_settings().color, Delivery::Screen.color_space());
        p.delivery = Delivery::Video;
        save(&d, &p).unwrap();
        let text = std::fs::read_to_string(file(&d)).unwrap();
        assert!(
            !text.contains("delivery ="),
            "Video is the default and is left out of the file: {text}"
        );
        assert_eq!(load(&d).0.delivery, Delivery::Video);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn open_shadows_is_saved_when_on_and_applies_only_to_video() {
        let d = dir("open-shadows");
        let mut p = RecordProfile::new();
        p.folder = d.join("rec");
        assert!(!p.video_settings().open_shadows);
        p.open_shadows = true;
        save(&d, &p).unwrap();
        let text = std::fs::read_to_string(file(&d)).unwrap();
        assert!(text.contains("open_shadows = true"));
        assert!(load(&d).0.open_shadows);
        let v = p.video_settings();
        assert!(v.open_shadows);
        assert_eq!(v.color, ColorSpace::BT709_LIMITED);
        p.delivery = Delivery::Screen;
        assert!(
            !p.video_settings().open_shadows,
            "Screen already matches the display, so the grade stays off"
        );
        save(&d, &p).unwrap();
        let text = std::fs::read_to_string(file(&d)).unwrap();
        assert!(
            text.contains("open_shadows = true"),
            "the switch is kept while Screen is selected: {text}"
        );
        p.open_shadows = false;
        p.delivery = Delivery::Video;
        save(&d, &p).unwrap();
        let text = std::fs::read_to_string(file(&d)).unwrap();
        assert!(
            !text.contains("open_shadows"),
            "off is the default and is left out of the file: {text}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn video_settings_follow_the_profile() {
        let mut p = RecordProfile::new();
        let v = p.video_settings();
        assert_eq!(
            v.rate_control,
            RateControl::Cbr {
                bitrate_bps: 12_000_000
            }
        );
        assert_eq!(
            v.keyframe_interval,
            KeyframeInterval::Duration(Duration::from_secs(2))
        );
        assert_eq!(v.preset, EncoderPreset::Balanced);
        assert_eq!((v.max_b_frames, v.profile.as_deref()), (0, None));
        assert_eq!(v.color, ColorSpace::BT709_LIMITED);
        assert!(!v.open_shadows);
        assert_eq!(p.delivery, Delivery::Video);

        p.cbr_padding = true;
        p.keyframe_seconds = 5;
        p.profile = Some("high".into());
        p.preset = Preset::Speed;
        p.b_frames = 2;
        let v = p.video_settings();
        assert_eq!(
            v.rate_control,
            RateControl::CbrPadded {
                bitrate_bps: 12_000_000
            }
        );
        assert_eq!(
            v.keyframe_interval,
            KeyframeInterval::Duration(Duration::from_secs(5))
        );
        assert_eq!(v.profile.as_deref(), Some("high"));
        assert_eq!(v.preset, EncoderPreset::Speed);
        assert_eq!(v.max_b_frames, 2);

        p.rate_control = RateMode::Vbr;
        let v = p.video_settings();
        assert_eq!(
            v.rate_control,
            RateControl::Vbr {
                target_bps: 12_000_000,
                max_bps: 18_000_000
            }
        );
        p.max_video_kbps = Some(40_000);
        assert_eq!(
            p.video_settings().rate_control,
            RateControl::Vbr {
                target_bps: 12_000_000,
                max_bps: 40_000_000
            }
        );

        p.rate_control = RateMode::ConstantQuality;
        p.quality = 18;
        assert_eq!(
            p.video_settings().rate_control,
            RateControl::ConstantQuality { level: 18 }
        );
        p.video_codec = VideoCodec::Hevc;
        assert_eq!(p.video_settings().codec, CodecId::Hevc);
    }

    #[test]
    fn problems_are_found() {
        let mut p = RecordProfile::new();
        p.folder = PathBuf::from("relative");
        assert!(p.problem().is_some());
        let mut p = RecordProfile::new();
        p.video_kbps = 100;
        assert!(p.problem().is_some());
        let mut p = RecordProfile::new();
        p.rate_control = RateMode::Vbr;
        p.max_video_kbps = Some(6_000);
        assert!(p.problem().unwrap().contains("below the bitrate"));
        p.rate_control = RateMode::Cbr;
        assert_eq!(p.problem(), None, "the peak only matters with VBR");
        let mut p = RecordProfile::new();
        p.quality = 52;
        assert!(p.problem().is_some());
        let mut p = RecordProfile::new();
        p.keyframe_seconds = 0;
        assert!(p.problem().is_some());
        let mut p = RecordProfile::new();
        p.b_frames = 5;
        assert!(p.problem().is_some());
    }

    #[test]
    fn file_names_carry_the_local_start_time_and_extension() {
        let mut p = RecordProfile::new();
        p.folder = PathBuf::from("/videos");
        p.format = Format::Mp4;
        let t = chrono::NaiveDate::from_ymd_opt(2026, 11, 3)
            .unwrap()
            .and_hms_opt(21, 30, 5)
            .unwrap();
        assert_eq!(
            p.file_for(t),
            PathBuf::from("/videos").join("RBS 2026-11-03 21-30-05.mp4")
        );
        assert_eq!(Format::Mkv.container(), ContainerId::Matroska);
        assert_eq!(Format::Mp4.container(), ContainerId::Mp4Fragmented);
        p.format = Format::Wav;
        p.bit_depth = 24;
        p.video_kbps = 1;
        assert_eq!(
            p.file_for(t),
            PathBuf::from("/videos").join("RBS 2026-11-03 21-30-05.wav")
        );
        p.folder = std::env::temp_dir();
        assert_eq!(
            p.problem(),
            None,
            "a WAV profile does not use the video bitrate"
        );
        let start = p.recording_start(Some("nvenc".into()), Some((1280, 720)));
        assert_eq!(start.container, ContainerId::Wav);
        assert_eq!(start.video, None);
        assert_eq!(start.bit_depth, Some(24));
        assert_eq!(start.encoder, None);
        assert_eq!(start.size, None);
        assert_eq!(start.audio_bitrate_bps, 0);
        let api = p.to_api();
        assert_eq!(api.container, rbs_api::types::Container::Wav);
        assert_eq!(api.bit_depth, 24);
        let back = RecordProfile::from_api(&api);
        assert_eq!(back.format, Format::Wav);
        assert_eq!(back.bit_depth, 24);
        p.bit_depth = 12;
        assert!(p.problem().is_some());
        p.bit_depth = 32;
        p.use_format(Format::Flac);
        assert_eq!(p.bit_depth, 16, "FLAC has no 32-bit float");
        assert_eq!(p.format, Format::Flac);
        p.bit_depth = 24;
        p.folder = std::env::temp_dir();
        assert_eq!(p.problem(), None);
        let start = p.recording_start(Some("nvenc".into()), Some((1280, 720)));
        assert_eq!(start.container, ContainerId::Flac);
        assert_eq!(start.video, None);
        assert_eq!(start.bit_depth, Some(24));
        assert_eq!(p.format.audio_codec(), CodecId::Flac);
        assert_eq!(
            p.file_for(t),
            std::env::temp_dir().join("RBS 2026-11-03 21-30-05.flac")
        );
        let api = p.to_api();
        assert_eq!(api.container, rbs_api::types::Container::Flac);
        let back = RecordProfile::from_api(&api);
        assert_eq!(back.format, Format::Flac);
        assert_eq!(back.bit_depth, 24);
        p.bit_depth = 32;
        assert!(p.problem().is_some());
    }

    #[test]
    fn the_media_choice_keeps_each_category() {
        let mut p = RecordProfile::new();
        p.folder = std::env::temp_dir();
        p.use_format(Format::Mp4);
        p.use_media(true);
        assert_eq!(p.format, Format::Wav);
        p.use_format(Format::Flac);
        p.use_media(false);
        assert_eq!(p.format, Format::Mp4);
        p.use_media(true);
        assert_eq!(p.format, Format::Flac);
        assert_eq!(p.format.category_index(), 1);
    }

    #[test]
    fn an_explicit_output_size_is_saved_and_an_absent_one_is_not() {
        let d = dir("output-size");
        let mut p = RecordProfile::new();
        p.folder = d.join("rec");
        save(&d, &p).unwrap();
        let text = std::fs::read_to_string(file(&d)).unwrap();
        assert!(
            !text.contains("output_size"),
            "same as canvas is left out of the file: {text}"
        );
        assert_eq!(load(&d).0.output_size, None);
        p.output_size = Some(OutputSize {
            width: 1280,
            height: 720,
        });
        save(&d, &p).unwrap();
        let text = std::fs::read_to_string(file(&d)).unwrap();
        assert!(text.contains("width = 1280"), "{text}");
        assert!(text.contains("height = 720"), "{text}");
        assert_eq!(load(&d).0.output_size, p.output_size);
        let api = p.to_api();
        assert_eq!(
            api.output_size.map(|s| (s.width, s.height)),
            Some((1280, 720))
        );
        assert_eq!(RecordProfile::from_api(&api).output_size, p.output_size);
        // One side only is not a size.
        let half = text.replace("height = 720\n", "");
        std::fs::write(file(&d), half).unwrap();
        let (got, why) = load(&d);
        assert_eq!(got.output_size, None);
        assert!(why.is_some(), "a half-specified size is refused");
        p.output_size = Some(OutputSize {
            width: 1921,
            height: 1080,
        });
        assert!(p.problem().unwrap().contains("even sides"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn output_size_presets_keep_the_canvas_aspect() {
        let canvas = (1920, 1080);
        let options = output_size_options(canvas, None);
        assert_eq!(options[0].size, None);
        assert_eq!(options[0].label, "Same as canvas");
        let sizes: Vec<_> = options.iter().filter_map(|o| o.size).collect();
        assert!(!sizes.iter().any(|s| s.pair() == canvas));
        assert!(
            sizes
                .iter()
                .all(|s| s.width.is_multiple_of(2) && s.height.is_multiple_of(2))
        );
        assert!(sizes.iter().all(|s| s.width * 9 == s.height * 16));
        let down = options
            .iter()
            .find(|o| o.label == "1280 x 720 (downscale)")
            .and_then(|o| o.size)
            .unwrap();
        assert_eq!(
            output_choice_copy(down, canvas),
            ("720p".to_owned(), "1280 × 720 · downscale".to_owned())
        );
        let custom = fit_output_size(1600, canvas).unwrap();
        assert_eq!(
            output_choice_copy(custom, canvas),
            ("1600 × 900".to_owned(), "downscale".to_owned())
        );
        assert!(options.iter().any(|o| o.label == "1280 x 720 (downscale)"));
        assert!(options.iter().any(|o| o.label == "2560 x 1440 (upscale)"));
        assert!(options.iter().any(|o| o.label == "3840 x 2160 (upscale)"));
        assert!(!options.iter().any(|o| o.label.contains("1920 x 1080")));

        // An odd nearest short side takes the closer even neighbour (562.5 → 562).
        assert_eq!(
            fit_output_size(1000, canvas).map(OutputSize::pair),
            Some((1000, 562))
        );
        // Exact odd integer: the two even neighbours tie, and the smaller one wins.
        assert_eq!(
            fit_output_size(22, (200, 100)).map(OutputSize::pair),
            Some((22, 10))
        );

        let vertical = (1080, 1920);
        let options = output_size_options(vertical, None);
        assert!(options.iter().any(|o| {
            o.size.map(OutputSize::pair) == Some((720, 1280)) && o.label.contains("downscale")
        }));
        assert!(options.iter().any(|o| {
            o.size.map(OutputSize::pair) == Some((1440, 2560)) && o.label.contains("upscale")
        }));
        assert!(
            !options
                .iter()
                .any(|o| o.size.map(OutputSize::pair) == Some(vertical))
        );
        assert!(options.iter().filter_map(|o| o.size).all(|s| {
            s.height > s.width && s.width.is_multiple_of(2) && s.height.is_multiple_of(2)
        }));

        // A long side that cannot fit inside 2..=7680 is not offered.
        assert_eq!(fit_output_size(1280, (7680, 2)), None);
        assert!(
            output_size_options((7680, 2), None)
                .iter()
                .all(|o| o.size.is_none())
        );

        let saved = fit_output_size(1600, canvas).unwrap();
        assert_eq!(saved.pair(), (1600, 900));
        let options = output_size_options(canvas, Some(saved));
        assert!(options.iter().any(|o| o.size == Some(saved)));
        assert!(output_size_matches(saved.pair(), canvas));
        assert!(!output_size_matches((1280, 800), canvas));
        // A stored size equal to the canvas is the default row, not a second one.
        let same = OutputSize {
            width: 1920,
            height: 1080,
        };
        assert!(
            !output_size_options(canvas, Some(same))
                .iter()
                .any(|o| o.size == Some(same))
        );
    }

    #[test]
    fn the_first_vertical_recording_omits_the_main_output_size() {
        let mut main = RecordProfile::new();
        main.output_size = Some(OutputSize {
            width: 1280,
            height: 720,
        });
        main.video_kbps = 8_000;
        let vertical = vertical_recording_from_main(&main);
        assert_eq!(vertical.output_size, None);
        assert_eq!(vertical.video_kbps, main.video_kbps);
        assert_eq!(vertical.folder, main.folder);
        assert_eq!(vertical.format, main.format);
    }
}
