//! The loudness meter's saved settings (ADR-0092 decisions 3, 5 and 7; spec
//! `docs/specs/user-data.md`, "Loudness settings"): whether the Mixer shows loudness,
//! the target and the true-peak limit, in `loudness.toml`. They are the user's settings
//! of this app on this computer, not part of a scene collection or an output profile.
//!
//! The target presets are data here, so Settings, the control API and the tests share
//! one table.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FORMAT: u32 = 1;

/// The lowest and highest target the settings take, LUFS.
pub const TARGET_RANGE: (f32, f32) = (-40.0, -5.0);
/// The lowest and highest true-peak limit the settings take, dBTP.
pub const LIMIT_RANGE: (f32, f32) = (-10.0, 0.0);
/// The true-peak limit by default, dBTP: what streaming sites and EBU R 128 ask for.
pub const DEFAULT_LIMIT: f32 = -1.0;

/// One target preset: its name in Settings and its loudness (LUFS).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Preset {
    /// The name Settings shows, with the value.
    pub label: &'static str,
    /// The target, LUFS.
    pub lufs: f32,
}

/// The target presets, the default first. Settings adds "Custom" after them.
pub const PRESETS: [Preset; 4] = [
    Preset {
        label: "Streaming (−14 LUFS)",
        lufs: -14.0,
    },
    Preset {
        label: "Podcast (−16 LUFS)",
        lufs: -16.0,
    },
    Preset {
        label: "Broadcast EBU R 128 (−23 LUFS)",
        lufs: -23.0,
    },
    Preset {
        label: "Broadcast ATSC A/85 (−24 LUFS)",
        lufs: -24.0,
    },
];

/// The preset a target is, if it is one.
pub fn preset_of(lufs: f32) -> Option<usize> {
    PRESETS.iter().position(|p| (p.lufs - lufs).abs() < 0.05)
}

/// The loudness settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoudnessSettings {
    /// The file's format; 1.
    pub format_version: u32,
    /// The Mixer shows loudness, so the meter runs (`loudness.set_enabled`).
    #[serde(default)]
    pub shown: bool,
    /// The target, LUFS: the mark on the loudness bar.
    pub target: f32,
    /// The true-peak limit, dBTP: the dot lights once the true peak passed it.
    pub true_peak_limit: f32,
}

impl Default for LoudnessSettings {
    fn default() -> Self {
        Self {
            format_version: FORMAT,
            shown: false,
            target: PRESETS[0].lufs,
            true_peak_limit: DEFAULT_LIMIT,
        }
    }
}

/// Why a target or limit cannot be used, or `None` when it can.
pub fn problem(target: f32, true_peak_limit: f32) -> Option<String> {
    let inside = |v: f32, (lo, hi): (f32, f32)| v.is_finite() && (lo..=hi).contains(&v);
    if !inside(target, TARGET_RANGE) {
        return Some(format!(
            "the target must be from {} to {} LUFS",
            TARGET_RANGE.0, TARGET_RANGE.1
        ));
    }
    if !inside(true_peak_limit, LIMIT_RANGE) {
        return Some(format!(
            "the true-peak limit must be from {} to {} dBTP",
            LIMIT_RANGE.0, LIMIT_RANGE.1
        ));
    }
    None
}

/// `v` rounded to a tenth, as the settings keep and show it.
pub fn tenth(v: f32) -> f32 {
    // `+ 0.0` turns a rounded −0.0 into 0.0, so equal settings compare equal.
    (v * 10.0).round() / 10.0 + 0.0
}

fn file(config: &Path) -> PathBuf {
    config.join("loudness.toml")
}

/// The saved settings; the defaults when there are none. A file that cannot be read or
/// a newer format gives the defaults and the reason; a value out of range gives the
/// defaults with the saved `shown`. The file is left as it is until the user changes a
/// setting, and that save writes format 1 (a newer build's extra keys are dropped).
pub fn load(config: &Path) -> (LoudnessSettings, Option<String>) {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (LoudnessSettings::default(), None);
        }
        Err(e) => {
            return (
                LoudnessSettings::default(),
                Some(format!("{}: {e}", path.display())),
            );
        }
    };
    match toml::from_str::<LoudnessSettings>(&text) {
        Ok(s) if s.format_version != FORMAT => (
            LoudnessSettings::default(),
            Some(format!(
                "{}: format version {} is not supported",
                path.display(),
                s.format_version
            )),
        ),
        Ok(s) => match problem(s.target, s.true_peak_limit) {
            Some(why) => (
                LoudnessSettings {
                    shown: s.shown,
                    ..LoudnessSettings::default()
                },
                Some(format!("{}: {why}", path.display())),
            ),
            None => (s, None),
        },
        Err(e) => (
            LoudnessSettings::default(),
            Some(format!("{}: {e}", path.display())),
        ),
    }
}

/// Writes the settings (replacing the file in one step).
pub fn save(config: &Path, settings: &LoudnessSettings) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(settings).map_err(std::io::Error::other)?;
    let path = file(config);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use super::*;

    #[test]
    fn presets_and_limits() {
        assert_eq!(preset_of(-14.0), Some(0));
        assert_eq!(preset_of(-24.0), Some(3));
        assert_eq!(preset_of(-18.0), None);
        assert_eq!(problem(-14.0, -1.0), None);
        assert!(problem(-50.0, -1.0).unwrap().contains("target"));
        assert!(problem(-14.0, 1.0).unwrap().contains("limit"));
        assert!(problem(f32::NAN, -1.0).is_some());
        // A value that rounds to zero is +0.0, so equal settings compare equal.
        assert_eq!(tenth(-0.04).to_bits(), 0.0f32.to_bits());
        assert_eq!(tenth(-14.04), -14.0);
        for p in PRESETS {
            assert_eq!(problem(p.lufs, DEFAULT_LIMIT), None);
        }
        assert_eq!(tenth(-14.04), -14.0);
    }

    #[test]
    fn settings_round_trip_and_bad_files_give_defaults() {
        let dir = std::env::temp_dir().join(format!("rbs-loudness-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load(&dir), (LoudnessSettings::default(), None));
        let s = LoudnessSettings {
            shown: true,
            target: -16.0,
            true_peak_limit: -2.0,
            ..LoudnessSettings::default()
        };
        save(&dir, &s).unwrap();
        assert_eq!(load(&dir), (s, None));
        std::fs::write(
            file(&dir),
            "format_version = 1\nshown = true\ntarget = 3.0\ntrue_peak_limit = -1.0\n",
        )
        .unwrap();
        let (read, why) = load(&dir);
        assert_eq!(read.target, -14.0);
        assert!(read.shown);
        assert!(why.unwrap().contains("target"));
        std::fs::write(
            file(&dir),
            "format_version = 2\ntarget = -14.0\ntrue_peak_limit = -1.0\n",
        )
        .unwrap();
        assert_eq!(load(&dir).0, LoudnessSettings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
