//! The replay buffer in the shell (ADR-0088): the `ReplayBuffer` global's callbacks on
//! the main window, floating windows and the Settings window, the saved settings
//! (`replay.toml`), and the texts the buffer's state becomes. The buffer itself runs in
//! the studio host ([`rbs_app::studio_host::Command::ArmReplay`] and friends).

use std::path::{Path, PathBuf};

use rbs_app::replay_settings::{
    self, MAX_LENGTH_SECS, MAX_MEMORY_MIB, MIN_LENGTH_SECS, MIN_MEMORY_MIB, ReplaySetup,
};
use rbs_app::studio_host::replay::ReplayState;
use rbs_app::studio_host::{Command, Update};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{App, Ctx, ReplayBuffer, UiError, entry};

/// The memory limits the Settings list offers, in MiB; a saved value that is not one of
/// them is listed too.
const MEMORY_STEPS_MIB: [u32; 7] = [256, 512, 1024, 2048, 4096, 8192, 16384];

/// The page's state.
pub struct Replay {
    /// What the user chose, as saved.
    setup: ReplaySetup,
    /// What the buffer reports.
    state: Option<ReplayState>,
    /// The file of the last clip, for Show in folder.
    saved: Option<PathBuf>,
    /// Why the last save failed.
    saved_error: Option<String>,
    /// Why the settings could not be saved.
    note: Option<String>,
    /// The slider while it is dragged; the saved length otherwise.
    dragging: Option<u32>,
    /// Bumped when Rust changes the folder text.
    folder_serial: i32,
}

impl Replay {
    /// The saved settings; also returns a note when the file could not be used.
    pub fn load(config: &Path) -> (Self, Option<String>) {
        let (setup, note) = replay_settings::load(config);
        (
            Self {
                setup,
                state: None,
                saved: None,
                saved_error: None,
                note: None,
                dragging: None,
                folder_serial: 0,
            },
            note,
        )
    }

    /// The settings to tell the studio at start.
    pub fn setup(&self) -> ReplaySetup {
        self.setup.clone()
    }

    /// The buffer is armed (Live controls and `replay.toggle`).
    pub fn armed(&self) -> bool {
        self.state.as_ref().is_some_and(|s| s.armed)
    }

    /// A clip can be saved now.
    pub fn can_save(&self) -> bool {
        self.state.as_ref().is_some_and(|s| s.armed && !s.saving)
    }

    /// The state changed.
    pub fn state(&mut self, state: ReplayState) {
        self.state = Some(state);
    }

    /// A save ended: the file, or why not.
    pub fn saved(&mut self, result: Result<PathBuf, String>) {
        match result {
            Ok(path) => {
                self.saved = Some(path);
                self.saved_error = None;
            }
            Err(why) => self.saved_error = Some(why),
        }
    }

    fn length_text(&self) -> String {
        duration_text(self.dragging.unwrap_or(self.setup.length_secs))
    }

    /// The buffer in words, and its tone (0 neutral, 1 good, 2 problem).
    fn status(&self) -> (String, i32) {
        match &self.state {
            Some(s) if s.armed => {
                let mut text = if s.held_ms < 1000 {
                    "Armed. Starting to keep the past.".to_owned()
                } else {
                    format!("Holds the last {}", clock_text(s.held_ms / 1000))
                };
                if s.memory_limited {
                    text.push_str(", limit: memory");
                }
                (text, 1)
            }
            Some(_) => ("Not armed. Nothing is kept.".to_owned(), 0),
            None => ("The studio is starting.".to_owned(), 0),
        }
    }

    fn estimate(&self, kbps: u32) -> String {
        let mib = self.setup.estimate_mib(kbps);
        format!(
            "About {} for {} at your recording's bit rate; the limit caps it.",
            size_text(mib),
            duration_text(self.setup.length_secs)
        )
    }
}

/// Registered with the studio at start: the saved settings, and arming when asked.
pub fn started(app: &mut App) {
    let setup = app.replay.setup();
    let arm = setup.arm_on_start;
    app.studio.send(Command::SetReplay(setup));
    if arm {
        app.studio.send(Command::ArmReplay);
    }
}

/// An update from the host about the buffer.
pub fn update(app: &mut App, u: &Update) {
    match u {
        Update::Replay(state) => app.replay.state(state.clone()),
        Update::ReplaySaved(result) => {
            app.replay.saved(result.clone());
            // A notice for the moment, the Live controls keep the file.
            app.error = Some(UiError::Notice(match result {
                Ok(path) => format!("Clip saved: {}", file_name(path)),
                Err(why) => why.clone(),
            }));
        }
        _ => {}
    }
}

/// Arm (`true`) or disarm: Live controls, Settings and `replay.toggle`.
pub fn toggle(app: &mut App, on: bool) {
    if on == app.replay.armed() {
        return;
    }
    app.studio.send(if on {
        Command::ArmReplay
    } else {
        Command::DisarmReplay
    });
}

/// Save the whole past as a clip.
pub fn save(app: &mut App) {
    if app.replay.can_save() {
        app.studio.send(Command::SaveReplay { seconds: None });
    }
}

fn changed(app: &mut App) {
    app.replay.note = replay_settings::save(&app.config, &app.replay.setup)
        .err()
        .map(|e| format!("The settings were not saved: {e}"));
    app.studio.send(Command::SetReplay(app.replay.setup()));
}

/// Sets the callbacks on a window's global.
pub fn install(g: &ReplayBuffer<'_>, ctx: &Ctx) {
    let c = ctx.clone();
    g.on_toggle(move |on| entry::guard(|| c.update(move |app| toggle(app, on))));
    let c = ctx.clone();
    g.on_save(move || entry::guard(|| c.update(save)));
    let c = ctx.clone();
    g.on_set_arm_on_start(move |on| {
        entry::guard(|| {
            c.update(move |app| {
                app.replay.setup.arm_on_start = on;
                changed(app);
            })
        })
    });
    let c = ctx.clone();
    g.on_set_audio(move |on| {
        entry::guard(|| {
            c.update(move |app| {
                app.replay.setup.audio = on;
                changed(app);
            })
        })
    });
    let c = ctx.clone();
    g.on_length_moved(move |v| {
        entry::guard(|| c.update(move |app| app.replay.dragging = Some(secs_at(v))))
    });
    let c = ctx.clone();
    g.on_length_released(move || {
        entry::guard(|| {
            c.update(|app| {
                if let Some(secs) = app.replay.dragging.take() {
                    app.replay.setup.length_secs = secs;
                    changed(app);
                }
            })
        })
    });
    let c = ctx.clone();
    g.on_set_memory(move |i| {
        entry::guard(|| {
            c.update(move |app| {
                let steps = memory_steps(app.replay.setup.memory_mib);
                if let Some(mib) = usize::try_from(i).ok().and_then(|i| steps.get(i)) {
                    app.replay.setup.memory_mib = *mib;
                    changed(app);
                }
            })
        })
    });
    let c = ctx.clone();
    g.on_set_folder(move |text| {
        entry::guard(|| {
            c.update(move |app| {
                let t = text.trim();
                let previous = app.replay.setup.folder.clone();
                app.replay.setup.folder = (!t.is_empty()).then(|| PathBuf::from(t));
                if app.replay.setup.problem().is_some() {
                    // Shown as the problem; the old folder stays in use.
                    app.replay.note = app.replay.setup.problem();
                    app.replay.setup.folder = previous;
                    app.replay.folder_serial += 1;
                } else {
                    changed(app);
                }
            })
        })
    });
    let c = ctx.clone();
    g.on_show_saved(move || {
        entry::guard(|| {
            c.update(|app| {
                if let Some(path) = &app.replay.saved {
                    show_in_folder(path);
                }
            })
        })
    });
}

/// Shows the page's state in the main window, floating windows and Settings.
pub fn push(app: &App) {
    let r = &app.replay;
    let (status, tone) = r.status();
    let kbps = app
        .studio
        .profile
        .video_kbps
        .saturating_add(app.studio.profile.audio_kbps);
    let estimate = r.estimate(kbps);
    let running = app.studio.state == 1;
    let steps = memory_steps(r.setup.memory_mib);
    let memory = steps
        .iter()
        .position(|m| *m == r.setup.memory_mib)
        .unwrap_or(0);
    let folder = r
        .setup
        .folder
        .as_ref()
        .map(|f| f.display().to_string())
        .unwrap_or_default();
    let problem = r
        .note
        .clone()
        .or_else(|| r.setup.problem())
        .unwrap_or_default();
    let saved_name = r.saved.as_deref().map(file_name).unwrap_or_default();
    let set = |g: ReplayBuffer<'_>| {
        g.set_can_arm(running);
        g.set_armed(r.armed());
        g.set_arm_on_start(r.setup.arm_on_start);
        g.set_audio(r.setup.audio);
        g.set_length(position_of(r.dragging.unwrap_or(r.setup.length_secs)));
        g.set_length_text(r.length_text().into());
        g.set_memories(ModelRc::new(VecModel::from(
            steps
                .iter()
                .map(|m| SharedString::from(size_text(u64::from(*m))))
                .collect::<Vec<_>>(),
        )));
        g.set_memory(i32::try_from(memory).unwrap_or(0));
        g.set_folder(folder.as_str().into());
        g.set_folder_serial(r.folder_serial);
        g.set_status(status.as_str().into());
        g.set_tone(tone);
        g.set_estimate(estimate.as_str().into());
        g.set_problem(problem.as_str().into());
        g.set_saving(r.state.as_ref().is_some_and(|s| s.saving));
        g.set_can_save(running && r.can_save());
        g.set_saved_name(saved_name.as_str().into());
        g.set_saved_error(r.saved_error.clone().unwrap_or_default().as_str().into());
    };
    set(app.main.global::<ReplayBuffer>());
    for f in &app.floats {
        set(f.window.global::<ReplayBuffer>());
    }
    if let Some(w) = &app.settings {
        set(w.global::<ReplayBuffer>());
    }
}

/// The offered memory limits, with the saved one when it is not a step.
fn memory_steps(saved: u32) -> Vec<u32> {
    let mut steps = MEMORY_STEPS_MIB.to_vec();
    if !steps.contains(&saved) && (MIN_MEMORY_MIB..=MAX_MEMORY_MIB).contains(&saved) {
        steps.push(saved);
        steps.sort_unstable();
    }
    steps
}

/// The slider's position (0 to 1, logarithmic) for a length.
fn position_of(secs: u32) -> f32 {
    let (lo, hi) = (MIN_LENGTH_SECS as f32, MAX_LENGTH_SECS as f32);
    ((secs.clamp(MIN_LENGTH_SECS, MAX_LENGTH_SECS) as f32 / lo).ln() / (hi / lo).ln())
        .clamp(0.0, 1.0)
}

/// The length at a slider position, rounded to a tidy step.
fn secs_at(position: f32) -> u32 {
    let (lo, hi) = (MIN_LENGTH_SECS as f32, MAX_LENGTH_SECS as f32);
    let raw = lo * (hi / lo).powf(position.clamp(0.0, 1.0));
    let step = if raw < 60.0 {
        5.0
    } else if raw < 600.0 {
        15.0
    } else {
        60.0
    };
    ((raw / step).round() * step).clamp(lo, hi) as u32
}

/// "45 s", "2 min", "2 min 30 s".
fn duration_text(secs: u32) -> String {
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

/// "3:12" or "1:02:03".
fn clock_text(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// "512 MiB", "2 GiB".
fn size_text(mib: u64) -> String {
    if mib >= 1024 && mib.is_multiple_of(1024) {
        format!("{} GiB", mib / 1024)
    } else if mib >= 1024 {
        format!("{:.1} GiB", mib as f64 / 1024.0)
    } else {
        format!("{mib} MiB")
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Opens the folder of `path` in the system's file manager with the file selected where
/// the system can.
fn show_in_folder(path: &Path) {
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(windows)]
    let spawned = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
    #[cfg(not(any(target_os = "macos", windows)))]
    let spawned = std::process::Command::new("xdg-open")
        .arg(path.parent().unwrap_or(path))
        .spawn();
    if let Ok(mut child) = spawned {
        // Reaped off the UI thread; it ends at once.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slider_round_trips_lengths_and_stays_in_range() {
        assert_eq!(secs_at(0.0), MIN_LENGTH_SECS);
        assert_eq!(secs_at(1.0), MAX_LENGTH_SECS);
        for secs in [10, 30, 120, 300, 1800] {
            let back = secs_at(position_of(secs));
            let off = back.abs_diff(secs);
            assert!(off <= secs / 8 + 5, "{secs} came back as {back}");
        }
        assert_eq!(position_of(5), 0.0);
        assert_eq!(position_of(99_999), 1.0);
    }

    #[test]
    fn times_and_sizes_read_naturally() {
        assert_eq!(duration_text(45), "45 s");
        assert_eq!(duration_text(120), "2 min");
        assert_eq!(duration_text(150), "2 min 30 s");
        assert_eq!(clock_text(192), "3:12");
        assert_eq!(clock_text(3723), "1:02:03");
        assert_eq!(size_text(512), "512 MiB");
        assert_eq!(size_text(2048), "2 GiB");
    }

    #[test]
    fn a_saved_memory_limit_that_is_not_a_step_is_listed() {
        assert_eq!(memory_steps(1024), MEMORY_STEPS_MIB.to_vec());
        let steps = memory_steps(1500);
        assert!(steps.contains(&1500) && steps.windows(2).all(|w| w[0] < w[1]));
    }
}
