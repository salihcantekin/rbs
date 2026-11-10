//! Settings › Recording › Replay buffer (ADR-0088 decision 3; spec
//! `docs/specs/recording.md`, "Replay buffer").
//!
//! [`ReplaySetup`] is what the user chose: the length of the past, the memory limit,
//! whether the buffer carries sound, whether it arms when RBS starts, and where clips
//! go. It is saved in `replay.toml`. Every value is a setting; none is hard-coded
//! behaviour. How the buffer encodes follows the saved recording profile
//! ("Match the recording", the default and the only mode in this version).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

const FORMAT: u32 = 1;

/// Shortest length of the past the buffer keeps.
pub const MIN_LENGTH_SECS: u32 = 10;
/// Longest length of the past the buffer keeps (30 minutes).
pub const MAX_LENGTH_SECS: u32 = 30 * 60;
/// Smallest memory limit in MiB.
pub const MIN_MEMORY_MIB: u32 = 64;
/// Largest memory limit in MiB (16 GiB).
pub const MAX_MEMORY_MIB: u32 = 16 * 1024;

/// What the user chose for the replay buffer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaySetup {
    /// The file's format; 1.
    pub format_version: u32,
    /// Arm the buffer when RBS starts (off by default).
    pub arm_on_start: bool,
    /// Wanted length of the past in seconds, [`MIN_LENGTH_SECS`] to [`MAX_LENGTH_SECS`].
    pub length_secs: u32,
    /// Most memory the buffer may use, in MiB.
    pub memory_mib: u32,
    /// Keep the sound too.
    pub audio: bool,
    /// Where clips go; `None` is the recording profile's folder.
    pub folder: Option<PathBuf>,
}

impl Default for ReplaySetup {
    fn default() -> Self {
        Self {
            format_version: FORMAT,
            arm_on_start: false,
            length_secs: 120,
            memory_mib: 1024,
            audio: true,
            folder: None,
        }
    }
}

impl ReplaySetup {
    /// The length as a duration.
    pub fn length(&self) -> Duration {
        Duration::from_secs(u64::from(self.length_secs))
    }

    /// The memory limit in bytes.
    pub fn memory_limit(&self) -> usize {
        usize::try_from(u64::from(self.memory_mib) << 20).unwrap_or(usize::MAX)
    }

    /// Why the settings cannot be used, or `None`.
    pub fn problem(&self) -> Option<String> {
        if !(MIN_LENGTH_SECS..=MAX_LENGTH_SECS).contains(&self.length_secs) {
            return Some(format!(
                "The length is {} s; it must be {MIN_LENGTH_SECS} to {MAX_LENGTH_SECS} seconds.",
                self.length_secs
            ));
        }
        if !(MIN_MEMORY_MIB..=MAX_MEMORY_MIB).contains(&self.memory_mib) {
            return Some(format!(
                "The memory limit is {} MiB; it must be {MIN_MEMORY_MIB} to {MAX_MEMORY_MIB}.",
                self.memory_mib
            ));
        }
        if self.folder.as_ref().is_some_and(|f| !f.is_absolute()) {
            return Some("The clip folder must be a full path.".into());
        }
        None
    }

    /// The estimate shown beside the settings: memory for the wanted length at a
    /// combined video and audio bit rate, capped by the memory limit.
    pub fn estimate_mib(&self, total_kbps: u32) -> u64 {
        let bytes = u64::from(total_kbps) * 1000 / 8 * u64::from(self.length_secs);
        bytes.div_ceil(1 << 20)
    }
}

fn file(config: &Path) -> PathBuf {
    config.join("replay.toml")
}

/// The saved settings; the defaults when there are none. A file that cannot be read, a
/// newer format or a value out of range gives the defaults and the reason (the file is
/// left as it is until the user changes a setting).
pub fn load(config: &Path) -> (ReplaySetup, Option<String>) {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (ReplaySetup::default(), None);
        }
        Err(e) => {
            return (
                ReplaySetup::default(),
                Some(format!("{}: {e}", path.display())),
            );
        }
    };
    match toml::from_str::<ReplaySetup>(&text) {
        Ok(s) if s.format_version != FORMAT => (
            ReplaySetup::default(),
            Some(format!(
                "{}: format version {} is not supported",
                path.display(),
                s.format_version
            )),
        ),
        Ok(s) => match s.problem() {
            Some(why) => (
                ReplaySetup::default(),
                Some(format!("{}: {why}", path.display())),
            ),
            None => (s, None),
        },
        Err(e) => (
            ReplaySetup::default(),
            Some(format!("{}: {e}", path.display())),
        ),
    }
}

/// Writes the settings (replacing the file in one step).
pub fn save(config: &Path, setup: &ReplaySetup) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(setup).map_err(std::io::Error::other)?;
    let path = file(config);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_match_the_adr() {
        let s = ReplaySetup::default();
        assert_eq!(s.problem(), None);
        assert_eq!((s.length_secs, s.memory_mib), (120, 1024));
        assert!(!s.arm_on_start && s.audio && s.folder.is_none());
    }

    #[test]
    fn out_of_range_values_are_refused() {
        let mut s = ReplaySetup::default();
        s.length_secs = 5;
        assert!(s.problem().is_some());
        s.length_secs = MAX_LENGTH_SECS + 1;
        assert!(s.problem().is_some());
        s.length_secs = 60;
        s.memory_mib = 1;
        assert!(s.problem().is_some());
        s.memory_mib = 512;
        s.folder = Some(PathBuf::from("relative/clips"));
        assert!(s.problem().is_some());
    }

    #[test]
    fn the_estimate_follows_the_bit_rate_and_length() {
        let mut s = ReplaySetup::default();
        s.length_secs = 300;
        // 20 Mbit/s video + 0.2 Mbit/s audio for five minutes: about 720 MiB.
        let mib = s.estimate_mib(20_200);
        assert!((700..=760).contains(&mib), "{mib}");
    }

    #[test]
    fn save_then_load_round_trips_and_bad_files_give_defaults() {
        let dir = std::env::temp_dir().join(format!("rbs-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (s, why) = load(&dir);
        assert_eq!((s, why), (ReplaySetup::default(), None));
        let mut s = ReplaySetup::default();
        s.length_secs = 300;
        s.arm_on_start = true;
        save(&dir, &s).unwrap();
        assert_eq!(load(&dir), (s, None));
        std::fs::write(file(&dir), "format_version = 9").unwrap();
        let (d, why) = load(&dir);
        assert_eq!(d, ReplaySetup::default());
        assert!(why.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
