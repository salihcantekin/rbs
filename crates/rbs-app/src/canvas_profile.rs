//! The canvas the app's studio runs (glossary: Canvas): its size and frame rate, as the
//! Video section of the Settings window sets them. Kept in `video.toml` in the config
//! directory. Toolkit-free and tested without a window.

use std::path::{Path, PathBuf};

use rbs_core::media::Size;
use rbs_core::timing::FrameRate;
use serde::{Deserialize, Serialize};

/// The file format version this build writes and reads.
const FORMAT: u32 = 1;

/// Canvas sizes offered, besides the main display's own.
pub const SIZES: [(u32, u32); 5] = [
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
    (1080, 1920),
];

/// Frame rates offered (whole frames per second).
pub const RATES: [u32; 5] = [24, 25, 30, 50, 60];

/// Largest side of a canvas: what the GPU path and encoders are sized for.
const MAX_SIDE: u32 = 7680;

/// The canvas size and rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanvasProfile {
    /// File format version (1).
    pub format_version: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Frames per second, one of [`RATES`].
    pub fps: u32,
}

impl CanvasProfile {
    /// 1920 x 1080 at 30 frames per second.
    pub fn new() -> Self {
        Self {
            format_version: FORMAT,
            width: 1920,
            height: 1080,
            fps: 30,
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
        let even = |v: u32| v >= 2 && v.is_multiple_of(2) && v <= MAX_SIDE;
        if !even(self.width) || !even(self.height) {
            return Some(format!(
                "Canvas {} x {} is not supported: sides must be even, 2 to {MAX_SIDE} pixels.",
                self.width, self.height
            ));
        }
        if !RATES.contains(&self.fps) {
            return Some(format!(
                "{} frames per second is not supported; use 24, 25, 30, 50 or 60.",
                self.fps
            ));
        }
        None
    }

    /// The size.
    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    /// The frame rate.
    pub fn frame_rate(&self) -> FrameRate {
        match self.fps {
            24 => FrameRate::FPS_24,
            25 => FrameRate::FPS_25,
            50 => FrameRate::FPS_50,
            60 => FrameRate::FPS_60,
            _ => FrameRate::FPS_30,
        }
    }
}

impl Default for CanvasProfile {
    fn default() -> Self {
        Self::new()
    }
}

/// A display's size as a canvas size: even sides, at most 7680 pixels, aspect kept.
pub fn canvas_for_display(width: u32, height: u32) -> Option<(u32, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    let scale = (f64::from(MAX_SIDE) / f64::from(width.max(height))).min(1.0);
    let even = |v: f64| ((v * scale / 2.0).round() as u32 * 2).max(2);
    Some((even(f64::from(width)), even(f64::from(height))))
}

/// The size of the preview surfaces for a canvas: its aspect ratio, the longer side
/// 1280 pixels (or the canvas's, if smaller), even sides.
pub fn preview_size(width: u32, height: u32) -> (u32, u32) {
    let long = 1280.min(width.max(height)).max(2);
    let even = |v: u64| ((v / 2) * 2).max(2) as u32;
    if width >= height {
        (
            even(u64::from(long)),
            even(u64::from(long) * u64::from(height) / u64::from(width.max(1))),
        )
    } else {
        (
            even(u64::from(long) * u64::from(width) / u64::from(height.max(1))),
            even(u64::from(long)),
        )
    }
}

fn file(config: &Path) -> PathBuf {
    config.join("video.toml")
}

/// The saved profile; the defaults when there is none. A file that cannot be read gives
/// the defaults and the reason.
pub fn load(config: &Path) -> (CanvasProfile, Option<String>) {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (CanvasProfile::new(), None),
        Err(e) => {
            return (
                CanvasProfile::new(),
                Some(format!("{}: {e}", path.display())),
            );
        }
    };
    match toml::from_str::<CanvasProfile>(&text) {
        Ok(p) => match p.problem() {
            None => (p, None),
            Some(why) => (
                CanvasProfile::new(),
                Some(format!("{}: {why}", path.display())),
            ),
        },
        Err(e) => (
            CanvasProfile::new(),
            Some(format!("{}: {e}", path.display())),
        ),
    }
}

/// Writes the profile (replacing the file in one step).
pub fn save(config: &Path, profile: &CanvasProfile) -> std::io::Result<()> {
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

    #[test]
    fn defaults_and_presets_are_valid() {
        assert_eq!(CanvasProfile::new().problem(), None);
        for (w, h) in SIZES {
            for fps in RATES {
                let p = CanvasProfile {
                    width: w,
                    height: h,
                    fps,
                    ..CanvasProfile::new()
                };
                assert_eq!(p.problem(), None, "{p:?}");
                assert_eq!(p.frame_rate().per_second().num(), fps);
            }
        }
    }

    #[test]
    fn odd_or_huge_sizes_and_other_rates_are_refused() {
        let base = CanvasProfile::new();
        assert!(
            CanvasProfile {
                width: 1921,
                ..base
            }
            .problem()
            .is_some()
        );
        assert!(CanvasProfile { height: 0, ..base }.problem().is_some());
        assert!(
            CanvasProfile {
                width: 8000,
                ..base
            }
            .problem()
            .is_some()
        );
        assert!(CanvasProfile { fps: 29, ..base }.problem().is_some());
    }

    #[test]
    fn a_saved_profile_loads_back_and_a_bad_one_gives_the_defaults() {
        let d = std::env::temp_dir().join(format!("rbs-app-video-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(load(&d), (CanvasProfile::new(), None));
        let p = CanvasProfile {
            width: 3440,
            height: 1440,
            fps: 60,
            ..CanvasProfile::new()
        };
        save(&d, &p).unwrap();
        assert_eq!(load(&d), (p, None));
        std::fs::write(file(&d), "width = 3").unwrap();
        let (got, why) = load(&d);
        assert_eq!(got, CanvasProfile::new());
        assert!(why.is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn displays_become_even_canvases_and_previews_keep_the_aspect() {
        assert_eq!(canvas_for_display(3440, 1440), Some((3440, 1440)));
        assert_eq!(canvas_for_display(1365, 767), Some((1366, 768)));
        assert_eq!(canvas_for_display(15360, 4320), Some((7680, 2160)));
        assert_eq!(canvas_for_display(0, 1080), None);
        assert_eq!(preview_size(1920, 1080), (1280, 720));
        assert_eq!(preview_size(3440, 1440), (1280, 534));
        assert_eq!(preview_size(1080, 1920), (720, 1280));
        assert_eq!(preview_size(640, 360), (640, 360));
    }
}
