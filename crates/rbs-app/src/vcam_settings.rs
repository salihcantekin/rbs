//! Settings › Virtual camera (ADR-0066 decisions 11 and 12, ADR-0070; spec
//! `docs/specs/virtual-camera.md`, "Desktop app").
//!
//! - [`VcamSettings`]: the camera's size and rate and whether the user added it, in
//!   `vcam.toml`. The camera belongs to the computer, not to an output profile.
//! - [`Page`]: what the section shows, a pure function of the camera's state, the work
//!   in progress and the sending counters, so it is tested without a window.

use std::path::{Path, PathBuf};

use rbs_core::timing::FrameRate;
use rbs_vcam::{InstallState, VcamStats};
use serde::{Deserialize, Serialize};

const FORMAT: u32 = 1;

/// The sizes the camera offers, largest first (ADR-0066 decision 11).
pub const SIZES: [(u32, u32); 3] = [(1920, 1080), (1280, 720), (640, 360)];
/// The rates the camera offers.
pub const RATES: [u32; 4] = [15, 24, 30, 60];

/// The camera's settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcamSettings {
    /// The file's format; 1.
    pub format_version: u32,
    /// The user added the camera (the toggle); the camera's real state is probed.
    pub enabled: bool,
    /// Width in pixels, one of [`SIZES`].
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Frames per second, one of [`RATES`].
    pub rate: u32,
}

impl Default for VcamSettings {
    fn default() -> Self {
        Self {
            format_version: FORMAT,
            enabled: false,
            width: 1280,
            height: 720,
            rate: 30,
        }
    }
}

impl VcamSettings {
    /// The index of the size in [`SIZES`].
    pub fn size_index(&self) -> usize {
        SIZES
            .iter()
            .position(|s| *s == (self.width, self.height))
            .unwrap_or(1)
    }

    /// The index of the rate in [`RATES`].
    pub fn rate_index(&self) -> usize {
        RATES.iter().position(|r| *r == self.rate).unwrap_or(2)
    }

    /// The rate as a frame rate.
    pub fn frame_rate(&self) -> FrameRate {
        FrameRate::new(self.rate, 1).unwrap_or(FrameRate::FPS_30)
    }

    fn valid(&self) -> bool {
        SIZES.contains(&(self.width, self.height)) && RATES.contains(&self.rate)
    }
}

fn file(config: &Path) -> PathBuf {
    config.join("vcam.toml")
}

/// The saved settings; the defaults when there are none. A file that cannot be read, a
/// newer format or a size or rate the camera does not offer gives the defaults and the
/// reason (the file is left as it is until the user changes a setting).
pub fn load(config: &Path) -> (VcamSettings, Option<String>) {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (VcamSettings::default(), None);
        }
        Err(e) => {
            return (
                VcamSettings::default(),
                Some(format!("{}: {e}", path.display())),
            );
        }
    };
    match toml::from_str::<VcamSettings>(&text) {
        Ok(s) if s.format_version != FORMAT => (
            VcamSettings::default(),
            Some(format!(
                "{}: format version {} is not supported",
                path.display(),
                s.format_version
            )),
        ),
        Ok(s) if !s.valid() => (
            VcamSettings {
                enabled: s.enabled,
                ..VcamSettings::default()
            },
            Some(format!(
                "{}: {} x {} at {} fps is not offered",
                path.display(),
                s.width,
                s.height,
                s.rate
            )),
        ),
        Ok(s) => (s, None),
        Err(e) => (
            VcamSettings::default(),
            Some(format!("{}: {e}", path.display())),
        ),
    }
}

/// Writes the settings (replacing the file in one step).
pub fn save(config: &Path, settings: &VcamSettings) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(settings).map_err(std::io::Error::other)?;
    let path = file(config);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// Where the camera's symbolic link is kept: the file `rbs-cli vcam` uses too, so a
/// camera either of them added is the same camera for both.
pub fn link_file(config: &Path) -> PathBuf {
    config.join("vcam-link.txt")
}

/// Work on the camera that runs off the UI thread (it may wait for a prompt).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    /// Reading the camera's state.
    Checking,
    /// Adding it: Windows may ask for administrator approval.
    Adding,
    /// Removing it.
    Removing,
    /// Removing it and RBS's part from Windows: a second approval.
    RemovingAll,
    /// Changing the format it announces.
    Formatting,
}

/// What a finished [`Work`] gives back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Done {
    /// The camera's state now.
    State(InstallState),
    /// The format was written: `true` applied, `false` applies when no app reads.
    Format(bool),
    /// It failed, in words for the user.
    Failed(String),
}

/// The camera's picture while RBS sends to it, from the studio.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sending {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Frames per second.
    pub rate: u32,
    /// The session's counters, when the adapter shares them.
    pub counters: Option<VcamStats>,
}

/// The state of Settings › Virtual camera.
#[derive(Clone, Debug)]
pub struct Page {
    /// The saved settings.
    pub settings: VcamSettings,
    /// Why the camera cannot exist on this platform at all (no adapter); `None` when
    /// there is one.
    pub no_adapter: Option<&'static str>,
    /// The camera's state as last read; `None` until read.
    pub state: Option<InstallState>,
    /// Work in progress.
    pub work: Option<Work>,
    /// The warning before adding is shown (ADR-0066 decision 11).
    pub confirming: bool,
    /// The last failure, until the next action.
    pub note: Option<String>,
    /// A format change waits until no app reads the camera.
    pub pending: bool,
    /// RBS sends to the camera.
    pub sending: Option<Sending>,
    /// Start was asked for; the studio has not answered yet.
    pub starting: bool,
    /// The counters at the last status, for "no app has the camera open".
    last: Option<VcamStats>,
    /// An app took frames since the last status.
    reading: bool,
}

/// The section as the window shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// The camera can be added on this computer.
    pub available: bool,
    /// The toggle: the camera is added (or being added).
    pub added: bool,
    /// The toggle can be used now.
    pub can_toggle: bool,
    /// The state in words.
    pub state: String,
    /// 0 neutral, 1 good, 2 problem (`SettingsStatus.tone`).
    pub tone: i32,
    /// Size and Frame rate can be changed now.
    pub can_format: bool,
    /// Why they cannot, or what a change does.
    pub format_help: String,
    /// Start or Stop can be used now.
    pub can_send: bool,
    /// The sending line.
    pub sending_text: String,
    /// "Remove completely" can be used now.
    pub can_remove_all: bool,
}

impl Page {
    /// A page for `settings`; `no_adapter` says why this platform has no camera.
    pub fn new(settings: VcamSettings, no_adapter: Option<&'static str>) -> Self {
        Self {
            settings,
            no_adapter,
            state: None,
            work: None,
            confirming: false,
            note: None,
            pending: false,
            sending: None,
            starting: false,
            last: None,
            reading: false,
        }
    }

    /// Takes a finished [`Work`].
    pub fn done(&mut self, work: Work, done: Done) {
        if self.work == Some(work) {
            self.work = None;
        }
        match done {
            Done::State(s) => {
                if s != InstallState::Installed {
                    self.pending = false;
                }
                self.state = Some(s);
            }
            Done::Format(applied) => self.pending = !applied,
            Done::Failed(why) => self.note = Some(why),
        }
    }

    /// The studio's word on sending: `None` stopped.
    pub fn sending(&mut self, s: Option<Sending>) {
        self.starting = false;
        let now = s.and_then(|s| s.counters);
        self.reading = match (self.last, now) {
            (Some(a), Some(b)) => b.sent() > a.sent(),
            (None, Some(b)) => b.sent() > 0,
            _ => false,
        };
        self.last = now;
        self.sending = s;
    }

    fn installed(&self) -> bool {
        self.state == Some(InstallState::Installed)
    }

    /// What the section shows.
    pub fn view(&self) -> View {
        let available = self.no_adapter.is_none()
            && !matches!(self.state, Some(InstallState::Unsupported { .. }));
        let added = self.installed() || self.work == Some(Work::Adding) || self.confirming;
        let busy = self.work.is_some();
        let (state, tone) = match (&self.work, &self.state, self.no_adapter) {
            (_, _, Some(why)) => (format!("Not available on this computer: {why}"), 0),
            (Some(Work::Adding), _, _) => (
                "Adding the camera… Windows may ask for administrator approval.".into(),
                0,
            ),
            (Some(Work::Removing), _, _) => ("Removing the camera…".into(), 0),
            (Some(Work::RemovingAll), _, _) => (
                "Removing the camera from Windows… Windows asks for administrator approval.".into(),
                0,
            ),
            (_, None, _) => ("Checking the camera…".into(), 0),
            (_, Some(InstallState::Unsupported { reason }), _) => {
                (format!("Not available on this computer: {reason}"), 0)
            }
            (_, Some(InstallState::NotInstalled), _) => ("Not added".into(), 0),
            (_, Some(InstallState::NeedsUpdate { reason }), _) => {
                (format!("Update needed: {reason}"), 2)
            }
            (_, Some(InstallState::Installed), _) => (
                "Added: apps on this computer can list RBS Virtual Camera.".into(),
                1,
            ),
            (_, Some(_), _) => ("Not added".into(), 0),
        };
        let (state, tone) = match &self.note {
            Some(why) => (format!("{state} {why}"), 2),
            None => (state, tone),
        };
        let can_format = available && !busy && self.sending.is_none() && !self.starting;
        let format_help = if !available {
            String::new()
        } else if self.sending.is_some() {
            "Stop sending to change the picture.".into()
        } else if self.pending {
            "Applies when the app using the camera closes.".into()
        } else {
            "What apps get: RBS scales the program to this size.".into()
        };
        let sending_text = match &self.sending {
            None if self.starting => "Starting…".into(),
            None if !self.installed() => "Add the camera first.".into(),
            None => "Not sending: apps show a still picture.".into(),
            Some(s) => {
                let what = format!("{} x {} at {} fps", s.width, s.height, s.rate);
                match s.counters {
                    Some(c) if self.reading => {
                        format!("Sending {what}: {} frames delivered.", c.sent())
                    }
                    Some(_) => format!("Sending {what}: no app has the camera open."),
                    None => format!("Sending {what}."),
                }
            }
        };
        View {
            available,
            added,
            can_toggle: available && !busy && self.sending.is_none() && !self.starting,
            state,
            tone,
            can_format,
            format_help,
            can_send: available
                && !busy
                && !self.starting
                && (self.sending.is_some() || self.installed()),
            sending_text,
            can_remove_all: available && !busy && self.sending.is_none() && !self.starting,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use rbs_vcam::VcamCounters;

    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("rbs-vcam-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn settings_round_trip_and_fallbacks() {
        let d = dir("round-trip");
        assert_eq!(load(&d), (VcamSettings::default(), None));
        let s = VcamSettings {
            enabled: true,
            width: 1920,
            height: 1080,
            rate: 60,
            ..VcamSettings::default()
        };
        save(&d, &s).unwrap();
        assert_eq!(load(&d), (s, None));
        assert_eq!((s.size_index(), s.rate_index()), (0, 3));
        // A size the camera does not offer: the defaults, but still added.
        std::fs::write(
            file(&d),
            "format_version = 1\nenabled = true\nwidth = 1000\nheight = 700\nrate = 30\n",
        )
        .unwrap();
        let (got, note) = load(&d);
        assert!(got.enabled && note.is_some());
        assert_eq!((got.width, got.height), (1280, 720));
        // Newer format.
        std::fs::write(
            file(&d),
            "format_version = 2\nenabled = true\nwidth = 1280\nheight = 720\nrate = 30\n",
        )
        .unwrap();
        let (got, note) = load(&d);
        assert_eq!(got, VcamSettings::default());
        assert!(note.is_some());
        assert_eq!(link_file(&d), d.join("vcam-link.txt"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_adapter_disables_everything() {
        let p = Page::new(VcamSettings::default(), Some("Not available on macOS yet"));
        let v = p.view();
        assert!(!v.available && !v.can_toggle && !v.can_send && !v.can_format);
        assert_eq!(
            v.state,
            "Not available on this computer: Not available on macOS yet"
        );
    }

    #[test]
    fn add_then_send_then_stop() {
        let mut p = Page::new(VcamSettings::default(), None);
        assert_eq!(p.view().state, "Checking the camera…");
        p.work = Some(Work::Checking);
        p.done(Work::Checking, Done::State(InstallState::NotInstalled));
        let v = p.view();
        assert!(v.can_toggle && !v.added && !v.can_send);
        assert_eq!((v.state.as_str(), v.tone), ("Not added", 0));
        // The warning shows the toggle on; adding keeps it on.
        p.confirming = true;
        assert!(p.view().added);
        p.confirming = false;
        p.work = Some(Work::Adding);
        assert!(p.view().added && !p.view().can_toggle);
        p.done(Work::Adding, Done::State(InstallState::Installed));
        let v = p.view();
        assert_eq!(v.tone, 1);
        assert!(v.can_send && v.can_format);
        // Sending, nobody reading, then an app reads.
        p.starting = true;
        assert!(!p.view().can_send);
        let counters = VcamCounters::default();
        let mut s = Sending {
            width: 1280,
            height: 720,
            rate: 30,
            counters: Some(counters.snapshot()),
        };
        p.sending(Some(s));
        let v = p.view();
        assert_eq!(
            v.sending_text,
            "Sending 1280 x 720 at 30 fps: no app has the camera open."
        );
        assert!(!v.can_format && !v.can_toggle && v.can_send);
        for _ in 0..5 {
            counters.count_sent();
        }
        s.counters = Some(counters.snapshot());
        p.sending(Some(s));
        assert_eq!(
            p.view().sending_text,
            "Sending 1280 x 720 at 30 fps: 5 frames delivered."
        );
        // No new frame taken: nobody reads any more.
        p.sending(Some(s));
        assert!(
            p.view()
                .sending_text
                .ends_with("no app has the camera open.")
        );
        p.sending(None);
        assert_eq!(
            p.view().sending_text,
            "Not sending: apps show a still picture."
        );
    }

    #[test]
    fn pending_format_and_failures() {
        let mut p = Page::new(VcamSettings::default(), None);
        p.done(Work::Checking, Done::State(InstallState::Installed));
        p.work = Some(Work::Formatting);
        assert!(!p.view().can_format);
        p.done(Work::Formatting, Done::Format(false));
        assert_eq!(
            p.view().format_help,
            "Applies when the app using the camera closes."
        );
        p.done(
            Work::Adding,
            Done::Failed("Administrator approval was not given.".into()),
        );
        let v = p.view();
        assert_eq!(v.tone, 2);
        assert!(v.state.ends_with("Administrator approval was not given."));
        // Removed: nothing is pending any more.
        p.done(Work::Removing, Done::State(InstallState::NotInstalled));
        assert!(!p.pending);
        p.done(
            Work::Checking,
            Done::State(InstallState::Unsupported {
                reason: "Needs Windows 11".into(),
            }),
        );
        assert!(!p.view().available);
    }
}
