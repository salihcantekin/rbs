//! Settings › Guests and `guest.toml` (ADR-0139 phase 1d): how a share is made. They
//! are the `check_phrase`, `lifetime_s` and `local_only` parameters of `guest.share`.
//!
//! - [`GuestSettings`]: the three values, with the bounds the API accepts.
//! - [`load`] and [`save`]: `guest.toml` in the settings folder.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FORMAT: u32 = 1;

/// How long an invite lasts, in minutes, as the page offers it (the API takes 1 to 30).
pub const LIFETIMES: [u32; 5] = [1, 5, 10, 20, 30];

/// The settings of a share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestSettings {
    /// The file's format; 1.
    pub format_version: u32,
    /// Compare the check phrase before the program is sent (ADR-0139 decision 6).
    pub check_phrase: bool,
    /// How long an invite lasts, one of [`LIFETIMES`].
    pub lifetime_min: u32,
    /// Private addresses only, no STUN query (ADR-0139 decision 7).
    pub local_only: bool,
}

impl Default for GuestSettings {
    fn default() -> Self {
        Self {
            format_version: FORMAT,
            check_phrase: true,
            lifetime_min: 10,
            local_only: false,
        }
    }
}

impl GuestSettings {
    /// The index of the lifetime in [`LIFETIMES`].
    pub fn lifetime_index(&self) -> usize {
        LIFETIMES
            .iter()
            .position(|m| *m == self.lifetime_min)
            .unwrap_or(2)
    }

    /// The invite's lifetime in seconds, for `guest.share`.
    pub fn lifetime_s(&self) -> u32 {
        self.lifetime_min * 60
    }
}

/// The labels of [`LIFETIMES`], for the drop-down.
pub fn lifetime_labels() -> Vec<String> {
    LIFETIMES
        .iter()
        .map(|m| {
            if *m == 1 {
                "1 minute".to_string()
            } else {
                format!("{m} minutes")
            }
        })
        .collect()
}

fn file(config: &Path) -> PathBuf {
    config.join("guest.toml")
}

/// The saved settings; the defaults when there are none. A file that cannot be read,
/// a newer format or a lifetime the page does not offer gives the defaults and the
/// reason (the file is left as it is until the user changes a setting).
pub fn load(config: &Path) -> (GuestSettings, Option<String>) {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (GuestSettings::default(), None);
        }
        Err(e) => {
            return (
                GuestSettings::default(),
                Some(format!("{}: {e}", path.display())),
            );
        }
    };
    match toml::from_str::<GuestSettings>(&text) {
        Ok(s) if s.format_version != FORMAT => (
            GuestSettings::default(),
            Some(format!(
                "{}: format version {} is not supported",
                path.display(),
                s.format_version
            )),
        ),
        Ok(s) if !LIFETIMES.contains(&s.lifetime_min) => (
            GuestSettings {
                lifetime_min: GuestSettings::default().lifetime_min,
                ..s
            },
            Some(format!(
                "{}: an invite of {} minutes is not offered",
                path.display(),
                s.lifetime_min
            )),
        ),
        Ok(s) => (s, None),
        Err(e) => (
            GuestSettings::default(),
            Some(format!("{}: {e}", path.display())),
        ),
    }
}

/// Writes the settings (replacing the file in one step).
pub fn save(config: &Path, settings: &GuestSettings) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(settings).map_err(std::io::Error::other)?;
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
        let d =
            std::env::temp_dir().join(format!("rbs-guest-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_defaults_check_the_phrase_for_ten_minutes_with_stun() {
        let s = GuestSettings::default();
        assert!(s.check_phrase && !s.local_only);
        assert_eq!(
            (s.lifetime_min, s.lifetime_s(), s.lifetime_index()),
            (10, 600, 2)
        );
    }

    #[test]
    fn settings_round_trip() {
        let d = dir("round-trip");
        assert_eq!(load(&d), (GuestSettings::default(), None));
        let s = GuestSettings {
            check_phrase: false,
            lifetime_min: 30,
            local_only: true,
            ..GuestSettings::default()
        };
        save(&d, &s).unwrap();
        assert_eq!(load(&d), (s, None));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_bad_lifetime_a_newer_format_and_a_broken_file_give_defaults_and_a_reason() {
        let d = dir("bad");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("guest.toml"),
            "format_version = 1\ncheck_phrase = false\nlifetime_min = 7\nlocal_only = false\n",
        )
        .unwrap();
        let (s, note) = load(&d);
        assert_eq!((s.lifetime_min, s.check_phrase), (10, false));
        assert!(note.is_some());
        std::fs::write(
            d.join("guest.toml"),
            "format_version = 2\ncheck_phrase = true\nlifetime_min = 10\nlocal_only = false\n",
        )
        .unwrap();
        assert!(load(&d).1.unwrap().contains("format version 2"));
        std::fs::write(d.join("guest.toml"), "not toml [").unwrap();
        assert_eq!(load(&d).0, GuestSettings::default());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn labels_follow_the_lifetimes() {
        let l = lifetime_labels();
        assert_eq!(l.len(), LIFETIMES.len());
        assert_eq!((l[0].as_str(), l[2].as_str()), ("1 minute", "10 minutes"));
    }
}
