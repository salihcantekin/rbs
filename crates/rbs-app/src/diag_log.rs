//! The app's diagnostics log (ADR-0158, spec `diagnostics-log.md`): started first in
//! `main`, before any other line is logged.

use std::sync::OnceLock;
use std::time::Duration;

use rbs_core::log::{self, Area, Filter};
use rbs_log::{Config, Logger};

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// How long the end of the run waits for the last lines to reach the file.
const LAST_LINES: Duration = Duration::from_millis(500);

/// Ends the log when dropped: the last lines get a short moment to reach the file.
/// `main` keeps it until the event loop has ended.
#[derive(Debug)]
#[must_use = "the log ends when this is dropped"]
pub struct Run(());

impl Drop for Run {
    fn drop(&mut self) {
        finish();
    }
}

/// Starts the log file in the system's logs folder and installs it. Levels: Normal
/// (Info) for every area and Warn for libraries, then `RBS_LOG`. Without a folder or
/// file the app runs on, logging to standard error what went wrong.
pub fn start() -> Run {
    start_file();
    Run(())
}

fn start_file() {
    let rbs_log = std::env::var("RBS_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let Some(dir) = rbs_log::default_dir() else {
        rbs_crash::errln!("rbs: no logs folder on this system; logging is off");
        return;
    };
    let mut config = Config::new(dir, "rbs");
    config.header = header();
    // A developer at a terminal also sees the lines there.
    config.echo = rbs_log.is_some() || std::io::IsTerminal::is_terminal(&std::io::stderr());
    let logger = match rbs_log::start(config) {
        Ok(l) => LOGGER.get_or_init(|| l),
        Err(e) => {
            rbs_crash::errln!("rbs: no log file: {e}");
            return;
        }
    };
    if log::install(logger.sink()).is_err() {
        return;
    }
    rbs_log::apply_level(Filter::Info);
    rbs_log::set_filter(Area::Lib, Filter::Warn);
    if let Some(text) = rbs_log {
        match rbs_log::parse_filters(&text) {
            Ok(items) => {
                for (area, filter) in items {
                    match area {
                        Some(area) => rbs_log::set_filter(area, filter),
                        None => rbs_log::apply_level(filter),
                    }
                }
            }
            Err(e) => rbs_core::warn!(Area::App, "RBS_LOG: {e}; levels stay Normal"),
        }
    }
    rbs_log::bridge_log_crate();
    if let Some(path) = logger.path() {
        rbs_core::info!(Area::App, "log file {}", path.display());
    }
}

/// The first lines of the file: what a bug report needs to know about this build.
fn header() -> Vec<String> {
    let levels = std::env::var("RBS_LOG").unwrap_or_else(|_| "normal".to_owned());
    vec![
        format!("RBS {}", env!("CARGO_PKG_VERSION")),
        format!(
            "System: {} {}, {} logical CPUs",
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
        ),
        format!("Levels: {levels}"),
    ]
}

/// Waits a short moment for the last lines to reach the file. Call it when the event
/// loop has ended, never on the UI thread while it runs.
fn finish() {
    if let Some(logger) = LOGGER.get() {
        rbs_core::info!(Area::App, "the app ends");
        let _ = logger.flush(LAST_LINES);
    }
}

/// The last lines written, for a crash report; empty when logging is off.
pub fn recent() -> Vec<String> {
    LOGGER.get().map(Logger::recent).unwrap_or_default()
}
