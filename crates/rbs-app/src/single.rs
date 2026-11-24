//! One RBS per settings folder (ADR-0181): the binary's side of `rbs_app::instance`.
//!
//! [`claim`] runs first in `main`. The lock it takes lives until the process ends.
//! Windows: the show event is made right after the lock, in every mode, and dropped
//! when Quit starts. macOS: `instance.state` says `running` once the reopen handler
//! exists and `closing` once Quit is decided. Linux keeps the read-only second
//! instance of ADR-0051 (decision 5), so nothing here runs there.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rbs_app::instance::{self, Claim, InstanceLock, Waited};

/// The lock and the folder it belongs to, held for the life of the process.
static HELD: OnceLock<(InstanceLock, PathBuf)> = OnceLock::new();

/// What `main` does next.
pub enum Start {
    /// Start RBS; the text, if any, is logged as a warning once the log runs.
    Run(Option<String>),
    /// End now with this exit code; the running RBS was asked to show itself, or did
    /// not answer and the user was told.
    End(u8),
}

/// Takes the folder's lock, or asks the running RBS to come forward (decisions 1, 3, 4).
pub fn claim(folder: &Path) -> Start {
    if !cfg!(any(target_os = "macos", windows)) {
        return Start::Run(None);
    }
    match instance::claim(folder) {
        Claim::Owner(lock) => own(lock, folder),
        Claim::Unavailable(e) => Start::Run(Some(format!(
            "single instance: the lock in the settings folder cannot be used ({e}); a second launch may start a second RBS"
        ))),
        Claim::Taken => {
            match instance::wait_for(folder, instance::WAIT, instance::RETRY, || signal(folder)) {
                Waited::Answered => Start::End(0),
                Waited::Owner(lock) => own(lock, folder),
                Waited::Unavailable(e) => Start::Run(Some(format!(
                    "single instance: waiting for the running RBS failed ({e})"
                ))),
                Waited::TimedOut => {
                    tell_no_answer();
                    Start::End(1)
                }
            }
        }
    }
}

fn own(lock: InstanceLock, folder: &Path) -> Start {
    // A leftover state from a run that ended without cleaning up is not this run's.
    let _ = instance::write_state(&lock, folder, &instance::State::Starting);
    let _ = HELD.set((lock, folder.to_path_buf()));
    #[cfg(windows)]
    win::make_event(folder);
    Start::Run(None)
}

/// Tries once to make the running RBS show its main window; true when the signal was
/// delivered. A closing or starting RBS is not signalled.
fn signal(folder: &Path) -> bool {
    #[cfg(windows)]
    {
        rbs_app::win::signal_running(&instance::folder_key(folder))
    }
    #[cfg(target_os = "macos")]
    {
        match instance::read_state(folder) {
            instance::State::Running { pid, exe } => rbs_app::native::reopen_running(pid, &exe),
            _ => false,
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = folder;
        false
    }
}

/// The message box of decision 4; English, like the app's other texts made in Rust.
fn tell_no_answer() {
    #[cfg(any(target_os = "macos", windows))]
    {
        let _ = rfd::MessageDialog::new()
            .set_title("RBS")
            .set_description(instance::NO_ANSWER)
            .set_level(rfd::MessageLevel::Warning)
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    }
}

/// Writes this RBS's state (macOS reads it; elsewhere nothing is written).
fn set_state(running: bool) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let Some((lock, folder)) = HELD.get() else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let pid = std::process::id();
    let state = if running {
        instance::State::Running { pid, exe }
    } else {
        instance::State::Closing { pid, exe }
    };
    if let Err(e) = instance::write_state(lock, folder, &state) {
        rbs_core::warn!(
            rbs_core::log::Area::App,
            "single instance: state not written: {e}"
        );
    }
}

/// Sets up the Dock and reopen way back (ADR-0056 decision 6); once it exists, a
/// second launch may ask this RBS to show itself (`running`). A second launch that
/// arrives before the main window exists needs nothing: RBS never starts hidden.
#[cfg(target_os = "macos")]
pub fn route_show(show: fn()) -> bool {
    let routed = rbs_app::native::route_show(show);
    if routed {
        set_state(true);
    }
    routed
}

/// [`claim`], first thing in `main`: before the log, the crash marker and any user
/// data, so a launch that only brings the running RBS forward leaves no trace. Ends
/// the process when this launch only brought the running RBS
/// forward or told the user it did not answer.
pub fn claim_or_exit(folder: &Path) -> Option<String> {
    match claim(folder) {
        Start::Run(note) => note,
        Start::End(code) => std::process::exit(i32::from(code)),
    }
}

/// Logs why the lock could not be used, once the log runs.
pub fn log_note(note: Option<String>) {
    if let Some(note) = note {
        rbs_core::warn!(rbs_core::log::Area::App, "{note}");
    }
}

/// Quit was decided: a second launch waits instead of asking (decision 2). Windows
/// drops the show event here (ADR-0156 decision 4).
pub fn closing() {
    set_state(false);
    #[cfg(windows)]
    win::drop_event();
}

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::path::Path;

    thread_local! {
        static EVENT: RefCell<Option<rbs_app::win::ShowEvent>> = const { RefCell::new(None) };
    }

    /// Made on the main thread, which becomes the UI thread.
    pub fn make_event(folder: &Path) {
        let key = rbs_app::instance::folder_key(folder);
        let event = rbs_app::win::ShowEvent::create(crate::show_from_system, &key);
        if event.is_none() {
            rbs_core::warn!(
                rbs_core::log::Area::App,
                "single instance: no show event; a second launch waits, then says RBS did not respond"
            );
        }
        EVENT.with(|e| *e.borrow_mut() = event);
    }

    pub fn drop_event() {
        EVENT.with(|e| e.borrow_mut().take());
    }
}
