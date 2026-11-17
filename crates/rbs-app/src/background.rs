//! Keeping RBS running after the main window closes (ADR-0056).
//!
//! - [`AppSettings`]: what closing the main window does, in `app.toml`.
//! - [`on_request`]: what a Close or a Quit does now, a pure function of the setting,
//!   the live state and whether a way back to a hidden window exists.
//! - [`dock_badge`] and [`tray_state`]: the live state without a window (U4), with a
//!   problem added to it, never in its place.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FORMAT: u32 = 1;

/// What closing the main window does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnClose {
    /// Closing the main window quits RBS.
    Quit,
    /// Closing the main window hides every RBS window; outputs and remote control go on.
    KeepRunning,
}

impl OnClose {
    /// The platform's default: the convention on macOS is to keep running; elsewhere
    /// closing quits, as before ADR-0056.
    pub const fn platform_default() -> Self {
        if cfg!(target_os = "macos") {
            Self::KeepRunning
        } else {
            Self::Quit
        }
    }

    /// Whether this platform can keep running without a window (Linux: not yet).
    pub const fn supported(self) -> bool {
        match self {
            Self::Quit => true,
            Self::KeepRunning => cfg!(any(target_os = "macos", windows)),
        }
    }
}

/// How much of the app's settings a user wants to see (ADR-0113 decision 2): which
/// section levels are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Experience {
    /// Simple sections only.
    Simple,
    /// Simple and advanced sections.
    Standard,
    /// Every section, expert ones too.
    Pro,
}

impl Experience {
    /// Whether advanced and expert sections are drawn (ADR-0113 decision 3).
    pub const fn levels(self) -> (bool, bool) {
        match self {
            Self::Simple => (false, false),
            Self::Standard => (true, false),
            Self::Pro => (true, true),
        }
    }

    /// The settings' order: Simple 0, Standard 1, Pro 2.
    pub const fn index(self) -> i32 {
        match self {
            Self::Simple => 0,
            Self::Standard => 1,
            Self::Pro => 2,
        }
    }

    /// The level at `index`; Standard for any other value.
    pub const fn from_index(index: i32) -> Self {
        match index {
            0 => Self::Simple,
            2 => Self::Pro,
            _ => Self::Standard,
        }
    }

    /// The command and API value (`simple`, `standard`, `pro`).
    pub const fn id(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Standard => "standard",
            Self::Pro => "pro",
        }
    }

    /// The level named `id`, if any.
    pub fn from_id(id: &str) -> Option<Self> {
        [Self::Simple, Self::Standard, Self::Pro]
            .into_iter()
            .find(|e| e.id() == id)
    }
}

/// The app's own settings, which belong to this computer and to no output profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    /// The file's format; 1.
    pub format_version: u32,
    /// What closing the main window does.
    pub on_close: OnClose,
    /// The first Close to the background said where RBS went (ADR-0056 decision 3).
    #[serde(default)]
    pub background_notice_shown: bool,
    /// A transition also puts the old program scene in preview (ADR-0098). Off by
    /// default: the preview keeps its scene. Read as false when absent, so the format
    /// stays 1.
    #[serde(default)]
    pub swap_after_transition: bool,
    /// Leave the RBS windows out of a screen capture (Windows
    /// `WDA_EXCLUDEFROMCAPTURE`, macOS `NSWindowSharingNone`). On when the key is
    /// absent: a screen recording does not include RBS. Linux ignores it.
    #[serde(default = "yes")]
    pub exclude_own_window: bool,
    /// Which section levels are drawn (ADR-0113 decision 2). Absent in files from
    /// older builds: [`AppSettings::settle`] fills it at the first start of this build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experience: Option<Experience>,
    /// The first-run page was shown (ADR-0113 decision 5). Absent: filled like
    /// `experience`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_run_done: Option<bool>,
    /// Seconds a paused or cued media file keeps its decoder before it is released
    /// (ADR-0108 decision 10). Absent: 30. Read through [`AppSettings::pause_release`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_release_s: Option<u32>,
}

/// The longest `pause_release_s` honoured: one hour.
const MAX_PAUSE_RELEASE_S: u32 = 3600;

impl AppSettings {
    /// The media pause release (`pause_release_s`, default 30 s), clamped to 1 s to
    /// one hour so a typo neither releases at once nor keeps decoders forever.
    pub fn pause_release(&self) -> std::time::Duration {
        self.pause_release_s
            .map_or(rbs_record::MEDIA_PAUSE_RELEASE, |s| {
                std::time::Duration::from_secs(u64::from(s.clamp(1, MAX_PAUSE_RELEASE_S)))
            })
    }

    /// Fills the fields an older file lacks (ADR-0113 decisions 2 and 5): an existing
    /// install reads as Pro with the first run done, a new one as Standard with the first
    /// run to show. Returns whether anything changed, so the caller saves at once and the
    /// next start does not take its own files for an older install's.
    pub fn settle(&mut self, existing_install: bool) -> bool {
        let mut changed = false;
        if self.experience.is_none() {
            self.experience = Some(if existing_install {
                Experience::Pro
            } else {
                Experience::Standard
            });
            changed = true;
        }
        if self.first_run_done.is_none() {
            self.first_run_done = Some(existing_install);
            changed = true;
        }
        changed
    }

    /// The level in use; Standard before [`AppSettings::settle`] ran.
    pub fn experience(&self) -> Experience {
        self.experience.unwrap_or(Experience::Standard)
    }
}

/// Whether the user data folder already holds a user file (ADR-0113 decision 2): a
/// workspace, an output profile, a scene collection, `appearance.toml` or `app.toml`.
/// Called before anything at this start writes one.
pub fn existing_install(config: &Path) -> bool {
    let has_file = |p: PathBuf| p.is_file();
    let has_entry = |p: PathBuf| {
        std::fs::read_dir(p)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
    };
    has_file(file(config))
        || has_file(config.join(rbs_user_data::APPEARANCE_FILE))
        // The binary's `config::workspace_dir`.
        || has_entry(config.join("workspaces"))
        || has_entry(config.join(rbs_user_data::PROFILES_DIR))
        || has_entry(config.join(rbs_user_data::SCENE_COLLECTIONS_DIR))
}

fn yes() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            format_version: FORMAT,
            on_close: OnClose::platform_default(),
            background_notice_shown: false,
            swap_after_transition: false,
            exclude_own_window: true,
            experience: None,
            first_run_done: None,
            pause_release_s: None,
        }
    }
}

fn file(config: &Path) -> PathBuf {
    config.join("app.toml")
}

/// The saved settings; the defaults when there are none. A file that cannot be read, a
/// newer format or a value this platform does not support gives the default for that
/// value (the file is left as it is until the user changes the setting).
pub fn load(config: &Path) -> (AppSettings, Option<String>) {
    let path = file(config);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (AppSettings::default(), None);
        }
        Err(e) => {
            return (
                AppSettings::default(),
                Some(format!("{}: {e}", path.display())),
            );
        }
    };
    match toml::from_str::<AppSettings>(&text) {
        Ok(s) if s.format_version != FORMAT => (
            AppSettings::default(),
            Some(format!(
                "{}: format version {} is not supported",
                path.display(),
                s.format_version
            )),
        ),
        Ok(mut s) => {
            if !s.on_close.supported() {
                s.on_close = OnClose::platform_default();
            }
            (s, None)
        }
        Err(e) => (
            AppSettings::default(),
            Some(format!("{}: {e}", path.display())),
        ),
    }
}

/// Writes the settings (replacing the file in one step).
pub fn save(config: &Path, settings: &AppSettings) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    let text = toml::to_string(settings).map_err(std::io::Error::other)?;
    let path = file(config);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// What the user asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// The main window's close button, Alt+F4, Cmd+W.
    Close,
    /// Cmd+Q, Dock > Quit, logout; the tray menu's Quit RBS.
    Quit,
}

/// What the app does for a [`Request`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Hide every RBS window and keep running.
    Hide {
        /// Say once where RBS went before hiding.
        notice: bool,
    },
    /// Show the main window and ask "End the stream and quit?".
    AskEndStream,
    /// Save, stop the studio and end the process.
    Quit,
}

/// What a Close or a Quit does now (ADR-0056 decision 3). `way_back`: a hidden window
/// can be brought back (Dock reopen on macOS, the tray icon on Windows); without one,
/// Close is Quit (decision 6).
pub fn on_request(
    settings: &AppSettings,
    request: Request,
    stream_live: bool,
    way_back: bool,
) -> Action {
    let keep = request == Request::Close
        && settings.on_close == OnClose::KeepRunning
        && settings.on_close.supported()
        && way_back;
    if keep {
        Action::Hide {
            notice: !settings.background_notice_shown,
        }
    } else if stream_live {
        Action::AskEndStream
    } else {
        Action::Quit
    }
}

/// The live state shown without a window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LiveState {
    /// A stream runs.
    pub streaming: bool,
    /// A recording runs.
    pub recording: bool,
    /// A problem went to the error bar and the window has not been shown since.
    pub problem: bool,
}

/// The Dock tile's badge on macOS (ADR-0056 decisions 7 and 8): "LIVE" while a stream
/// runs, "REC" while only a recording runs, with "!" added for a problem.
pub fn dock_badge(state: LiveState) -> Option<&'static str> {
    match (state.streaming, state.recording, state.problem) {
        (true, _, false) => Some("LIVE"),
        (true, _, true) => Some("LIVE !"),
        (false, true, false) => Some("REC"),
        (false, true, true) => Some("REC !"),
        (false, false, true) => Some("!"),
        (false, false, false) => None,
    }
}

/// The tray icon's variant on Windows: shapes, not colour alone (ADR-0056 decision 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayGlyph {
    /// The RBS icon: nothing live.
    Idle,
    /// With a recording dot.
    Recording,
    /// With a broadcast glyph.
    Live,
}

/// The tray icon's variant and whether it carries the warning mark.
pub fn tray_state(state: LiveState) -> (TrayGlyph, bool) {
    let glyph = if state.streaming {
        TrayGlyph::Live
    } else if state.recording {
        TrayGlyph::Recording
    } else {
        TrayGlyph::Idle
    };
    (glyph, state.problem)
}

/// Width and height of the tray icon's pixels.
pub const TRAY_ICON_SIZE: u32 = 32;

/// The tray icon's RGBA pixels (ADR-0056 decision 7): a neutral disc for RBS, a filled
/// dot while recording, a dot with a ring around it (broadcast) while live, and a
/// warning triangle for a problem. The marks differ in shape, not only in colour (U7).
pub fn tray_pixels(glyph: TrayGlyph, warning: bool) -> Vec<u8> {
    const NEUTRAL: [u8; 4] = [0x9c, 0xa3, 0xaf, 0xff];
    const OUTLINE: [u8; 4] = [0x37, 0x41, 0x51, 0xff];
    const LIVE: [u8; 4] = [0xe5, 0x48, 0x4d, 0xff];
    const WARN: [u8; 4] = [0xf5, 0xa5, 0x24, 0xff];
    const WHITE: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
    let n = TRAY_ICON_SIZE as usize;
    let mut px = vec![0u8; n * n * 4];
    let dist = |x: usize, y: usize, cx: f32, cy: f32| {
        let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
        (dx * dx + dy * dy).sqrt()
    };
    for y in 0..n {
        for x in 0..n {
            let mut c = [0u8; 4];
            // RBS: a disc with an outline.
            let d = dist(x, y, 14.0, 14.0);
            if d <= 13.0 {
                c = if d > 11.5 { OUTLINE } else { NEUTRAL };
            }
            // The live state, bottom right, on a white backing so it reads on any
            // taskbar.
            let b = dist(x, y, 24.0, 24.0);
            match glyph {
                TrayGlyph::Idle => {}
                TrayGlyph::Recording => {
                    if b <= 8.0 {
                        c = if b > 6.0 { WHITE } else { LIVE };
                    }
                }
                TrayGlyph::Live => {
                    if b <= 8.0 {
                        c = if b <= 3.0 || (5.0..=7.0).contains(&b) {
                            LIVE
                        } else {
                            WHITE
                        };
                    }
                }
            }
            // A problem: a triangle at the top right.
            if warning {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                // Apex (25, 1), base y = 13 from x = 18 to 32.
                if (1.0..=13.0).contains(&fy) && (fx - 25.0).abs() <= (fy - 1.0) * 7.0 / 12.0 {
                    c = WARN;
                }
            }
            let i = (y * n + x) * 4;
            px[i..i + 4].copy_from_slice(&c);
        }
    }
    px
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-app-bg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn keep(shown: bool) -> AppSettings {
        AppSettings {
            on_close: OnClose::KeepRunning,
            background_notice_shown: shown,
            ..AppSettings::default()
        }
    }

    fn quit() -> AppSettings {
        AppSettings {
            on_close: OnClose::Quit,
            ..AppSettings::default()
        }
    }

    #[test]
    fn quit_setting_makes_close_quit() {
        for live in [false, true] {
            for way_back in [false, true] {
                let want = if live {
                    Action::AskEndStream
                } else {
                    Action::Quit
                };
                assert_eq!(on_request(&quit(), Request::Close, live, way_back), want);
                assert_eq!(on_request(&quit(), Request::Quit, live, way_back), want);
            }
        }
    }

    #[test]
    fn keep_running_hides_without_asking() {
        if !OnClose::KeepRunning.supported() {
            // Linux: Close always quits.
            assert_eq!(
                on_request(&keep(true), Request::Close, false, true),
                Action::Quit
            );
            return;
        }
        // Nothing ends, so nothing is asked, also while live.
        for live in [false, true] {
            assert_eq!(
                on_request(&keep(true), Request::Close, live, true),
                Action::Hide { notice: false }
            );
        }
        // The first time says where RBS went.
        assert_eq!(
            on_request(&keep(false), Request::Close, false, true),
            Action::Hide { notice: true }
        );
    }

    #[test]
    fn quit_asks_while_live_whatever_the_setting() {
        for s in [keep(true), quit()] {
            assert_eq!(
                on_request(&s, Request::Quit, true, true),
                Action::AskEndStream
            );
            assert_eq!(on_request(&s, Request::Quit, false, true), Action::Quit);
        }
    }

    #[test]
    fn no_way_back_means_close_quits() {
        assert_eq!(
            on_request(&keep(true), Request::Close, false, false),
            Action::Quit
        );
        assert_eq!(
            on_request(&keep(true), Request::Close, true, false),
            Action::AskEndStream
        );
    }

    #[test]
    fn badge_keeps_the_live_state_with_a_problem() {
        let s = |streaming, recording, problem| LiveState {
            streaming,
            recording,
            problem,
        };
        assert_eq!(dock_badge(s(false, false, false)), None);
        assert_eq!(dock_badge(s(false, true, false)), Some("REC"));
        assert_eq!(dock_badge(s(true, false, false)), Some("LIVE"));
        assert_eq!(dock_badge(s(true, true, false)), Some("LIVE"));
        assert_eq!(dock_badge(s(true, true, true)), Some("LIVE !"));
        assert_eq!(dock_badge(s(false, true, true)), Some("REC !"));
        assert_eq!(dock_badge(s(false, false, true)), Some("!"));
        assert_eq!(tray_state(s(true, true, true)), (TrayGlyph::Live, true));
        assert_eq!(
            tray_state(s(false, true, false)),
            (TrayGlyph::Recording, false)
        );
        assert_eq!(tray_state(s(false, false, true)), (TrayGlyph::Idle, true));
    }

    #[test]
    fn tray_variants_differ_in_shape() {
        let n = TRAY_ICON_SIZE as usize;
        let len = n * n * 4;
        // Opaque pixels only, so colour alone cannot tell the variants apart.
        let shape = |px: &[u8]| -> Vec<bool> { px.chunks(4).map(|p| p[3] != 0).collect() };
        let colours =
            |px: &[u8]| -> Vec<[u8; 3]> { px.chunks(4).map(|p| [p[0], p[1], p[2]]).collect() };
        let idle = tray_pixels(TrayGlyph::Idle, false);
        let rec = tray_pixels(TrayGlyph::Recording, false);
        let live = tray_pixels(TrayGlyph::Live, false);
        let warn = tray_pixels(TrayGlyph::Live, true);
        for p in [&idle, &rec, &live, &warn] {
            assert_eq!(p.len(), len);
        }
        assert_ne!(shape(&idle), shape(&rec));
        assert_ne!(shape(&idle), shape(&warn));
        // Recording and live share an outline, so their inner shapes must differ:
        // count the live-coloured pixels.
        let red = |px: &[u8]| {
            colours(px)
                .iter()
                .filter(|c| **c == [0xe5, 0x48, 0x4d])
                .count()
        };
        assert_ne!(red(&rec), red(&live));
        // The warning keeps the live mark.
        assert!(red(&warn) > 0);
    }

    #[test]
    fn settings_round_trip_and_fallbacks() {
        let d = dir("round-trip");
        // Missing: the platform default, no note.
        assert_eq!(load(&d), (AppSettings::default(), None));
        let s = AppSettings {
            on_close: OnClose::Quit,
            background_notice_shown: true,
            swap_after_transition: true,
            ..AppSettings::default()
        };
        save(&d, &s).unwrap();
        assert_eq!(load(&d), (s, None));
        // A file without the swap key (older builds): off (ADR-0098).
        std::fs::write(file(&d), "format_version = 1\non_close = \"quit\"\n").unwrap();
        assert!(!load(&d).0.swap_after_transition);
        // Invalid: the default and the reason.
        std::fs::write(file(&d), "format_version = 1\non_close = \"sometimes\"\n").unwrap();
        let (got, note) = load(&d);
        assert_eq!(got, AppSettings::default());
        assert!(note.is_some());
        // Newer format: the default and the reason.
        std::fs::write(file(&d), "format_version = 2\non_close = \"quit\"\n").unwrap();
        let (got, note) = load(&d);
        assert_eq!(got, AppSettings::default());
        assert!(note.is_some());
        // Unsupported here (Linux): the platform default, without a note.
        std::fs::write(
            file(&d),
            "format_version = 1\non_close = \"keep_running\"\n",
        )
        .unwrap();
        let (got, note) = load(&d);
        assert_eq!(note, None);
        let want = if OnClose::KeepRunning.supported() {
            OnClose::KeepRunning
        } else {
            OnClose::platform_default()
        };
        assert_eq!(got.on_close, want);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn experience_maps_to_the_drawn_levels() {
        assert_eq!(Experience::Simple.levels(), (false, false));
        assert_eq!(Experience::Standard.levels(), (true, false));
        assert_eq!(Experience::Pro.levels(), (true, true));
        for e in [Experience::Simple, Experience::Standard, Experience::Pro] {
            assert_eq!(Experience::from_index(e.index()), e);
            assert_eq!(Experience::from_id(e.id()), Some(e));
        }
        assert_eq!(Experience::from_index(7), Experience::Standard);
        assert_eq!(Experience::from_id("expert"), None);
    }

    #[test]
    fn absent_fields_settle_by_install() {
        let mut new = AppSettings::default();
        assert!(new.settle(false));
        assert_eq!(new.experience, Some(Experience::Standard));
        assert_eq!(new.first_run_done, Some(false));
        assert!(!new.settle(true), "settled fields stay");
        let mut old = AppSettings::default();
        assert!(old.settle(true));
        assert_eq!(old.experience, Some(Experience::Pro));
        assert_eq!(old.first_run_done, Some(true));
    }

    #[test]
    fn the_pause_release_defaults_to_30_s_and_is_clamped() {
        use std::time::Duration;
        let mut s = AppSettings::default();
        assert_eq!(s.pause_release(), Duration::from_secs(30));
        s.pause_release_s = Some(0);
        assert_eq!(s.pause_release(), Duration::from_secs(1));
        s.pause_release_s = Some(u32::MAX);
        assert_eq!(s.pause_release(), Duration::from_secs(3600));
        s.pause_release_s = Some(90);
        assert_eq!(s.pause_release(), Duration::from_secs(90));
        let text = toml::to_string(&AppSettings::default()).unwrap();
        assert!(!text.contains("pause_release_s"), "absent unless set");
    }

    #[test]
    fn experience_round_trips_and_is_optional() {
        let d = dir("experience");
        // A file from an older build: both fields absent.
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(file(&d), "format_version = 1\non_close = \"quit\"\n").unwrap();
        let (got, note) = load(&d);
        assert_eq!(note, None);
        assert_eq!((got.experience, got.first_run_done), (None, None));
        let s = AppSettings {
            experience: Some(Experience::Simple),
            first_run_done: Some(true),
            ..AppSettings::default()
        };
        save(&d, &s).unwrap();
        assert_eq!(load(&d), (s, None));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_install_is_known_by_any_user_file() {
        let d = dir("existing");
        assert!(!existing_install(&d), "no folder");
        std::fs::create_dir_all(d.join("crash-reports")).unwrap();
        assert!(!existing_install(&d), "crash reports are not user files");
        std::fs::create_dir_all(d.join(rbs_user_data::SCENE_COLLECTIONS_DIR)).unwrap();
        assert!(!existing_install(&d), "an empty folder is not a file");
        std::fs::write(
            d.join(rbs_user_data::SCENE_COLLECTIONS_DIR)
                .join("default.toml"),
            "",
        )
        .unwrap();
        assert!(existing_install(&d));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(rbs_user_data::APPEARANCE_FILE), "").unwrap();
        assert!(existing_install(&d));
        let _ = std::fs::remove_dir_all(&d);
    }
}
