//! The recording's output settings in the app (glossary: output profile): folder,
//! format, encoder and bitrates. Kept in `recording.toml` in the config directory, so
//! they survive a restart. Toolkit-free and tested without a window.

use std::path::{Path, PathBuf};

use rbs_core::media::ContainerId;
use serde::{Deserialize, Serialize};

/// The file format version this build writes and reads.
const FORMAT: u32 = 1;

/// Audio bitrates the AAC encoders take, in kbit/s (Media Foundation AAC accepts only
/// these).
pub const AUDIO_KBPS: [u32; 4] = [96, 128, 160, 192];

/// Video bitrates offered, in kbit/s.
pub const VIDEO_KBPS: [u32; 8] = [2_500, 4_000, 6_000, 8_000, 12_000, 20_000, 40_000, 80_000];

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
    /// Video bitrate in kbit/s.
    pub video_kbps: u32,
    /// Audio bitrate in kbit/s, one of [`AUDIO_KBPS`].
    pub audio_kbps: u32,
}

impl RecordProfile {
    /// MKV in the user's videos folder, the best encoder, 12 Mbit/s video and
    /// 160 kbit/s audio.
    pub fn new() -> Self {
        Self {
            format_version: FORMAT,
            folder: default_folder(),
            format: Format::Mkv,
            encoder: None,
            video_kbps: 12_000,
            audio_kbps: 160,
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
        None
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

impl Default for RecordProfile {
    fn default() -> Self {
        Self::new()
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
    fn problems_are_found() {
        let mut p = RecordProfile::new();
        p.folder = PathBuf::from("relative");
        assert!(p.problem().is_some());
        let mut p = RecordProfile::new();
        p.video_kbps = 100;
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
