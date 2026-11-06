//! The recording's output settings in the app (glossary: output profile): folder,
//! format, encoder, and every encoder setting that changes the recording's quality
//! (rate control and bitrates, quality level, keyframe interval, profile, preset,
//! B-frames, CBR padding). Kept in the `recording` section of the output profile in
//! use (ADR-0051, `user_session`); `recording.toml`, which older versions kept, is read
//! once to make the first profile. Toolkit-free and tested without a window.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rbs_codec::{EncoderPreset, KeyframeInterval, RateControl};
use rbs_core::media::ContainerId;
use rbs_record::VideoSettings;
use serde::{Deserialize, Serialize};

/// The file format version this build writes and reads.
const FORMAT: u32 = 1;

/// Audio bitrates the AAC encoders take, in kbit/s (Media Foundation AAC accepts only
/// these).
pub const AUDIO_KBPS: [u32; 4] = [96, 128, 160, 192];

/// Video bitrates offered, in kbit/s.
pub const VIDEO_KBPS: [u32; 8] = [2_500, 4_000, 6_000, 8_000, 12_000, 20_000, 40_000, 80_000];

/// Constant-quality levels offered (0 best ..= 51).
pub const QUALITY_LEVELS: [u8; 10] = [14, 16, 18, 20, 22, 23, 25, 28, 32, 36];

/// Keyframe intervals offered, in seconds.
pub const KEYFRAME_SECONDS: [u32; 6] = [1, 2, 3, 4, 5, 10];

/// Most B-frames a profile may ask for.
pub const MAX_B_FRAMES: u32 = 4;

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

/// The recording's container, as the user picks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// Matroska (`.mkv`): survives a crash.
    Mkv,
    /// Fragmented MP4 (`.mp4`).
    Mp4,
}

impl Format {
    /// Both, in the order the UI lists them.
    pub const ALL: [Format; 2] = [Format::Mkv, Format::Mp4];

    /// The container.
    pub fn container(self) -> ContainerId {
        match self {
            Format::Mkv => ContainerId::Matroska,
            Format::Mp4 => ContainerId::Mp4Fragmented,
        }
    }

    /// The file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Mkv => "mkv",
            Format::Mp4 => "mp4",
        }
    }

    /// The name in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Format::Mkv => "MKV",
            Format::Mp4 => "MP4",
        }
    }
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
}

impl Encoding {
    /// The best encoder, CBR at `video_kbps` without padding, a keyframe every 2 s, the
    /// encoder's profile, the balanced preset, no B-frames and 160 kbit/s audio.
    pub fn new(video_kbps: u32) -> Self {
        Self {
            encoder: None,
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

/// What the next recording uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordProfile {
    /// File format version (1).
    pub format_version: u32,
    /// Where recordings are written.
    pub folder: PathBuf,
    /// The container.
    pub format: Format,
    /// The video encoder's adapter id, or `None` for the best one this system has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder: Option<String>,
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
    /// Audio bitrate in kbit/s, one of [`AUDIO_KBPS`].
    pub audio_kbps: u32,
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
            encoder: None,
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
        };
        p.set_encoding(Encoding::new(12_000));
        p
    }

    /// The encoder settings.
    pub fn encoding(&self) -> Encoding {
        Encoding {
            encoder: self.encoder.clone(),
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
        }
    }

    /// Replaces the encoder settings.
    pub fn set_encoding(&mut self, e: Encoding) {
        let Encoding {
            encoder,
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
        } = e;
        self.encoder = encoder;
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
    }

    /// The VBR peak in kbit/s: the saved one, or 1.5 times the target.
    pub fn peak_kbps(&self) -> u32 {
        self.encoding().peak_kbps()
    }

    /// The video encoding these settings ask for (see [`Encoding::video_settings`]).
    pub fn video_settings(&self) -> VideoSettings {
        self.encoding().video_settings()
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
        self.encoding().problem()
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
            },
            encoder: self.encoder.clone(),
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
        Self {
            format_version: FORMAT,
            folder: PathBuf::from(&p.folder),
            format: match p.container {
                api::Container::Mkv => Format::Mkv,
                api::Container::Mp4 => Format::Mp4,
            },
            encoder: p.encoder.clone(),
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
        }
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
        }
    }

    /// Settings from the control API's encoding. The caller checks
    /// [`Encoding::problem`].
    pub fn from_api(e: &rbs_api::types::Encoding) -> Self {
        use rbs_api::types as api;
        Self {
            encoder: e.encoder.clone(),
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
        Ok(p) => match p.problem() {
            None => (p, None),
            Some(why) => (
                RecordProfile::new(),
                Some(format!("{}: {why}", path.display())),
            ),
        },
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
            p.format = format;
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
        p.format = Format::Mp4;
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
        want.format = Format::Mp4;
        want.video_kbps = 8_000;
        want.audio_kbps = 128;
        assert_eq!(p, want);
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
    }
}
