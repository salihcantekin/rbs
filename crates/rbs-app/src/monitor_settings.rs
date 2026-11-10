//! The monitor output's device and volume, and when the host opens it (ADR-0097
//! decisions 4 and 7; spec `docs/specs/audio-graph.md`, "Monitor").
//!
//! - [`MonitorSettings`]: the device ("System default" or a named playback endpoint)
//!   and the monitor volume, in `monitor.toml`. Both belong to the computer that runs
//!   the engine, not to the scene collection or an output profile.
//! - [`MonitorControl`]: what the host does next with the monitor device, a pure
//!   function of whether the studio wants it, whether it plays, whether it failed and
//!   the time, so the back-off is tested without devices or sleeps.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rbs_core::time::Timestamp;
use serde::{Deserialize, Serialize};

const FORMAT: u32 = 1;

/// The first wait before opening a monitor device again after it failed.
pub const FIRST_RETRY: Duration = Duration::from_secs(1);
/// The longest wait between attempts; the wait doubles up to this.
pub const MAX_RETRY: Duration = Duration::from_secs(30);

/// The monitor's saved settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MonitorSettings {
    /// The file's format; 1.
    pub format_version: u32,
    /// The playback endpoint id (`rbs_sources::MonitorDevice::target`); absent:
    /// System default, which follows the OS default output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Linear gain of the monitor output, `0.0..=1.0`.
    pub volume: f32,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            format_version: FORMAT,
            device: None,
            volume: 1.0,
        }
    }
}

impl MonitorSettings {
    fn valid(&self) -> bool {
        self.volume.is_finite() && (0.0..=1.0).contains(&self.volume)
    }
}

fn file(config: &Path) -> PathBuf {
    config.join("monitor.toml")
}

/// The saved settings; the defaults when there are none. A file that cannot be read, a
/// newer format or a volume out of range gives the defaults and the reason (the file is
/// left as it is until the user changes a setting).
pub fn load(config: &Path) -> (MonitorSettings, Option<String>) {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (MonitorSettings::default(), None);
        }
        Err(e) => {
            return (
                MonitorSettings::default(),
                Some(format!("{}: {e}", path.display())),
            );
        }
    };
    match toml::from_str::<MonitorSettings>(&text) {
        Ok(s) if s.format_version != FORMAT => (
            MonitorSettings::default(),
            Some(format!(
                "{}: format version {} is not supported",
                path.display(),
                s.format_version
            )),
        ),
        Ok(s) if !s.valid() => (
            MonitorSettings {
                volume: 1.0,
                ..s.clone()
            },
            Some(format!(
                "{}: volume {} is out of range",
                path.display(),
                s.volume
            )),
        ),
        Ok(s) => (s, None),
        Err(e) => (
            MonitorSettings::default(),
            Some(format!("{}: {e}", path.display())),
        ),
    }
}

/// Writes the settings (replacing the file in one step).
pub fn save(config: &Path, settings: &MonitorSettings) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(settings).map_err(std::io::Error::other)?;
    let path = file(config);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// What the host does with the monitor device now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorStep {
    /// Nothing.
    Keep,
    /// Open the device and start the monitor; report a failure with
    /// [`MonitorControl::failed`].
    Open,
    /// Stop the monitor (nothing is monitored any more, or its device failed).
    Close,
}

/// When the host opens, closes and retries the monitor device (ADR-0097 decisions 4
/// and 7). No device is opened while nothing is monitored; a failed device is closed
/// and opened again after [`FIRST_RETRY`], doubling up to [`MAX_RETRY`]; the wait
/// starts over once the monitor plays without failing, or once nothing is monitored.
#[derive(Debug, Clone, Default)]
pub struct MonitorControl {
    retry_at: Option<Timestamp>,
    wait: Option<Duration>,
}

impl MonitorControl {
    /// The step for the studio's state at `now`, and how long until this should be
    /// asked again (`None`: only after a change).
    pub fn step(
        &mut self,
        wanted: bool,
        playing: bool,
        failed: bool,
        now: Timestamp,
    ) -> (MonitorStep, Option<Duration>) {
        if !wanted {
            self.retry_at = None;
            self.wait = None;
            let step = if playing {
                MonitorStep::Close
            } else {
                MonitorStep::Keep
            };
            return (step, None);
        }
        if playing {
            if failed {
                let left = self.failed(now);
                return (MonitorStep::Close, Some(left));
            }
            // It runs: the next failure starts the back-off over.
            self.wait = None;
            return (MonitorStep::Keep, None);
        }
        match self.retry_at {
            Some(at) if now < at => (MonitorStep::Keep, Some(at.saturating_since(now))),
            _ => {
                self.retry_at = None;
                (MonitorStep::Open, None)
            }
        }
    }

    /// The device failed to open or start at `now`: the next attempt waits. Returns the
    /// wait.
    pub fn failed(&mut self, now: Timestamp) -> Duration {
        let wait = self.wait.map_or(FIRST_RETRY, |w| (w * 2).min(MAX_RETRY));
        self.wait = Some(wait);
        self.retry_at = Some(now + wait);
        wait
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use super::*;

    fn at(ms: u64) -> Timestamp {
        Timestamp::from_nanos(ms * 1_000_000)
    }

    #[test]
    fn nothing_is_opened_until_a_source_is_monitored() {
        let mut c = MonitorControl::default();
        assert_eq!(
            c.step(false, false, false, at(0)),
            (MonitorStep::Keep, None)
        );
        assert_eq!(c.step(true, false, false, at(1)), (MonitorStep::Open, None));
        assert_eq!(c.step(true, true, false, at(2)), (MonitorStep::Keep, None));
        assert_eq!(
            c.step(false, true, false, at(3)),
            (MonitorStep::Close, None)
        );
    }

    #[test]
    fn a_failed_device_is_retried_with_a_growing_wait() {
        let mut c = MonitorControl::default();
        assert_eq!(c.step(true, false, false, at(0)).0, MonitorStep::Open);
        // Opening failed: wait a second.
        assert_eq!(c.failed(at(0)), FIRST_RETRY);
        assert_eq!(
            c.step(true, false, false, at(400)),
            (MonitorStep::Keep, Some(Duration::from_millis(600)))
        );
        assert_eq!(c.step(true, false, false, at(1000)).0, MonitorStep::Open);
        // It opened but its node failed: closed, and the wait doubles.
        assert_eq!(
            c.step(true, true, true, at(1500)),
            (MonitorStep::Close, Some(Duration::from_secs(2)))
        );
        assert_eq!(c.step(true, false, false, at(3499)).0, MonitorStep::Keep);
        assert_eq!(c.step(true, false, false, at(3500)).0, MonitorStep::Open);
        let mut wait = Duration::from_secs(2);
        for _ in 0..10 {
            wait = c.failed(at(0));
        }
        assert_eq!(wait, MAX_RETRY);
        // Playing without failing starts the back-off over.
        assert_eq!(c.step(true, true, false, at(90_000)).0, MonitorStep::Keep);
        assert_eq!(c.failed(at(90_000)), FIRST_RETRY);
        // Nothing monitored: no retry is pending any more.
        assert_eq!(
            c.step(false, false, false, at(90_001)),
            (MonitorStep::Keep, None)
        );
        assert_eq!(c.step(true, false, false, at(90_002)).0, MonitorStep::Open);
    }

    #[test]
    fn settings_round_trip_and_bad_files_give_defaults() {
        let dir = std::env::temp_dir().join(format!("rbs-monitor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load(&dir), (MonitorSettings::default(), None));
        let s = MonitorSettings {
            device: Some("play:{0.0.0.00000000}.{1234}".into()),
            volume: 0.25,
            ..MonitorSettings::default()
        };
        save(&dir, &s).unwrap();
        assert_eq!(load(&dir), (s, None));
        std::fs::write(file(&dir), "format_version = 1\nvolume = 3.0\n").unwrap();
        let (read, why) = load(&dir);
        assert_eq!(read.volume, 1.0);
        assert!(why.unwrap().contains("volume"));
        std::fs::write(file(&dir), "format_version = 2\nvolume = 0.5\n").unwrap();
        assert_eq!(load(&dir).0, MonitorSettings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
