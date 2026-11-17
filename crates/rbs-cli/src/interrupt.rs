//! Ctrl+C asks a running `record` or `stream` to stop through the normal finish
//! path, so the file gets its trailer. The handler only stores a flag. A second
//! request while a stop is already asked lets the process end.

use std::sync::atomic::{AtomicBool, Ordering};

/// The user asked to stop. The recording loop loads this between service steps.
static ASKED: AtomicBool = AtomicBool::new(false);
/// The console handler is installed at most once per process.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Records a stop request. True the first time, so the process keeps running and
/// finishes the file. False when a stop was already asked.
pub fn ask_stop() -> bool {
    // One flag, no other data: relaxed is enough for the loop to observe it.
    !ASKED.swap(true, Ordering::Relaxed)
}

/// Whether [`ask_stop`] has been called.
pub fn stop_asked() -> bool {
    ASKED.load(Ordering::Relaxed)
}

/// Installs the console handler. A later call does nothing. Failure leaves Enter
/// as the way to stop; the recording still runs.
pub fn install() {
    if INSTALLED.swap(true, Ordering::Relaxed) {
        return;
    }
    // SAFETY: the handler is static, touches only `ASKED`, and catches a panic so
    // it cannot unwind into the operating system.
    unsafe { platform::install() }
}

#[cfg(windows)]
mod platform {
    use std::panic::catch_unwind;

    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, SetConsoleCtrlHandler,
    };
    use windows_sys::core::BOOL;

    use super::ask_stop;

    /// Handled: the process keeps running.
    const HANDLED: BOOL = 1;

    unsafe extern "system" fn on_console(event: u32) -> BOOL {
        if event != CTRL_C_EVENT && event != CTRL_BREAK_EVENT && event != CTRL_CLOSE_EVENT {
            return 0;
        }
        // A panic here must not cross into the console host (ADR-0055).
        let first = catch_unwind(ask_stop).unwrap_or(false);
        if first { HANDLED } else { 0 }
    }

    pub unsafe fn install() {
        // SAFETY: `on_console` outlives the process, and adding a handler does not
        // free an existing one.
        unsafe {
            SetConsoleCtrlHandler(Some(on_console), HANDLED);
        }
    }
}

#[cfg(unix)]
mod platform {
    use std::panic::catch_unwind;

    use super::ask_stop;

    unsafe extern "C" fn on_sigint(_: libc::c_int) {
        let first = catch_unwind(ask_stop).unwrap_or(false);
        if !first {
            // SAFETY: `signal` and `raise` are async-signal-safe. Restoring the
            // default action and raising SIGINT ends the process on a second press.
            unsafe {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::raise(libc::SIGINT);
            }
        }
    }

    pub unsafe fn install() {
        // SAFETY: `on_sigint` outlives the process. `signal` installs it for SIGINT
        // and does not reset the handler after delivery on the platforms RBS runs.
        unsafe {
            libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    pub unsafe fn install() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_interrupt_asks_to_stop_and_the_second_does_not() {
        // `ASKED` is process-global. This is the only test that calls `ask_stop`.
        assert!(ask_stop());
        assert!(stop_asked());
        assert!(!ask_stop());
    }
}
