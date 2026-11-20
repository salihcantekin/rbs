//! The app's diagnostics log (ADR-0158, spec `diagnostics-log.md`): started first in
//! `main`, before any other line is logged.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use rbs_core::log::{self, Area, Filter};
use rbs_log::{Config, Logger};
use serde::{Deserialize, Serialize};

use crate::background::AppSettings;

static LOGGER: OnceLock<Logger> = OnceLock::new();
/// `RBS_LOG` as read at start; while set it decides the levels, not Settings.
static ENV_LEVELS: OnceLock<Option<String>> = OnceLock::new();

/// How long Save logs waits for the queued lines to reach the file.
const SAVE_WAIT: Duration = Duration::from_secs(2);

/// Settings › Advanced › Log detail (ADR-0158 decision 7), saved in `app.toml`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogDetail {
    /// Errors only.
    Errors,
    /// Warnings and errors.
    Warnings,
    /// Info and above: the default.
    #[default]
    Normal,
    /// Debug and above.
    Detailed,
    /// Every line.
    Everything,
}

impl LogDetail {
    /// In the order the page shows them.
    pub const ALL: [LogDetail; 5] = [
        LogDetail::Errors,
        LogDetail::Warnings,
        LogDetail::Normal,
        LogDetail::Detailed,
        LogDetail::Everything,
    ];

    /// Its place in [`LogDetail::ALL`].
    pub fn index(self) -> i32 {
        match self {
            LogDetail::Errors => 0,
            LogDetail::Warnings => 1,
            LogDetail::Normal => 2,
            LogDetail::Detailed => 3,
            LogDetail::Everything => 4,
        }
    }

    /// The level at `i` in [`LogDetail::ALL`]; Normal for another number.
    pub fn from_index(i: i32) -> LogDetail {
        usize::try_from(i)
            .ok()
            .and_then(|i| LogDetail::ALL.get(i).copied())
            .unwrap_or_default()
    }

    /// The filter it sets.
    pub fn filter(self) -> Filter {
        match self {
            LogDetail::Errors => Filter::Error,
            LogDetail::Warnings => Filter::Warn,
            LogDetail::Normal => Filter::Info,
            LogDetail::Detailed => Filter::Debug,
            LogDetail::Everything => Filter::Trace,
        }
    }

    /// Detailed or Everything: a level that goes back to Normal at the next start.
    pub fn is_detailed(self) -> bool {
        matches!(self, LogDetail::Detailed | LogDetail::Everything)
    }
}

/// The areas the Pro list shows, with their names on the page, in its order.
pub const AREA_NAMES: [(Area, &str); 16] = [
    (Area::App, "App"),
    (Area::Ui, "Windows"),
    (Area::Studio, "Studio"),
    (Area::Engine, "Engine"),
    (Area::Gpu, "GPU"),
    (Area::Capture, "Capture"),
    (Area::Audio, "Audio"),
    (Area::Encode, "Encoding"),
    (Area::Record, "Recording"),
    (Area::Stream, "Stream"),
    (Area::Browser, "Browser"),
    (Area::Remote, "Remote control"),
    (Area::AddOns, "Add-ons"),
    (Area::Plugin, "Plugins"),
    (Area::UserData, "Profiles and collections"),
    (Area::Lib, "Libraries"),
];

/// `RBS_LOG` when it decides the levels (it was set when the app started).
pub fn env_levels() -> Option<&'static str> {
    ENV_LEVELS.get().and_then(Option::as_deref)
}

/// At start: a Detailed or Everything level goes back to Normal unless the user keeps
/// it (decision 7), and so do the per-area levels. True when `s` changed (save it).
pub fn settle(s: &mut AppSettings) -> bool {
    if s.log_keep {
        return false;
    }
    let detailed = s.log_detail.is_some_and(LogDetail::is_detailed);
    let areas = s.log_areas.values().any(|d| d.is_detailed());
    if detailed {
        s.log_detail = None;
    }
    if areas {
        s.log_areas.retain(|_, d| !d.is_detailed());
    }
    detailed || areas
}

/// Applies the Settings levels at once, unless `RBS_LOG` decides: Log detail for every
/// area but libraries (Warn), then each per-area level.
pub fn apply(s: &AppSettings) {
    if env_levels().is_some() || LOGGER.get().is_none() {
        return;
    }
    rbs_log::apply_level(s.log_detail.unwrap_or_default().filter());
    rbs_log::set_filter(Area::Lib, Filter::Warn);
    for (name, detail) in &s.log_areas {
        if let Some(area) = Area::from_name(name) {
            rbs_log::set_filter(area, detail.filter());
        }
    }
}

/// The logs folder, when this system has one.
pub fn folder() -> Option<PathBuf> {
    rbs_log::default_dir()
}

/// Writes the Save logs zip to `out`: the last three runs (the app's and the browser
/// helper's), up to ten crash reports from `config`, a system summary and `settings`
/// (the app settings, which hold no keys or paths). Waits for the queued lines first,
/// so never call it on the UI thread. Returns how many files the zip holds.
pub fn save_logs(out: &Path, config: &Path, settings: String) -> std::io::Result<usize> {
    let Some(logs) = folder() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "this system has no logs folder",
        ));
    };
    if let Some(logger) = LOGGER.get() {
        let _ = logger.flush(SAVE_WAIT);
    }
    let mut system = header().join("\n");
    system.push('\n');
    let bundle = rbs_log::Bundle {
        logs,
        prefixes: vec!["rbs".to_owned(), "rbs-browser".to_owned()],
        runs: 3,
        crash_reports: Some(config.join("crash-reports")),
        extra: vec![
            ("system.txt".to_owned(), system),
            ("settings.txt".to_owned(), settings),
        ],
    };
    rbs_log::write_zip(&bundle, out, SystemTime::now())
}

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
    let _ = ENV_LEVELS.set(rbs_log.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_detail_indexes_round_trip_and_map_to_filters() {
        for (i, d) in LogDetail::ALL.iter().enumerate() {
            assert_eq!(LogDetail::from_index(d.index()), *d);
            assert_eq!(d.index() as usize, i);
        }
        assert_eq!(LogDetail::from_index(9), LogDetail::Normal);
        assert_eq!(LogDetail::from_index(-1), LogDetail::Normal);
        assert_eq!(LogDetail::Detailed.filter(), Filter::Debug);
        assert!(LogDetail::Everything.is_detailed() && !LogDetail::Normal.is_detailed());
    }

    #[test]
    fn the_page_lists_every_area_once() {
        let names: Vec<Area> = AREA_NAMES.iter().map(|(a, _)| *a).collect();
        assert_eq!(names, Area::ALL);
    }
}
