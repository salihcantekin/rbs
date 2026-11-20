//! The CLI's diagnostics log (ADR-0158 decision 7, spec `diagnostics-log.md`): leveled
//! lines go to standard error, redacted and with repeats collapsed, from the `rbs-log`
//! thread. The CLI's answers to its user stay `outln!` and `errln!`.

use std::time::Duration;

use rbs_core::log::{self, Area, Filter};
use rbs_log::Logger;

/// Ends the log when dropped: the last lines get a short moment to be written.
#[must_use = "the log ends when this is dropped"]
pub struct Run(Option<Logger>);

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(logger) = &self.0 {
            let _ = logger.flush(Duration::from_millis(200));
        }
    }
}

/// Starts the log: Warn for every area, then `RBS_LOG`, then `--log-level` (`level`),
/// later items winning. An unreadable `level` is the error, for a usage message.
pub fn start(level: Option<&str>) -> Result<Run, String> {
    let level = level
        .map(|l| rbs_log::parse_filters(l).map_err(|e| format!("--log-level: {e}")))
        .transpose()?;
    let logger = match rbs_log::start_stderr(1024, rbs_crash::home_dir()) {
        Ok(l) => l,
        Err(e) => {
            rbs_crash::errln!("warning: no diagnostics log: {e}");
            return Ok(Run(None));
        }
    };
    if log::install(logger.sink()).is_err() {
        return Ok(Run(None));
    }
    rbs_log::apply_level(Filter::Warn);
    rbs_log::set_filter(Area::Lib, Filter::Warn);
    if let Some(text) = std::env::var("RBS_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        match rbs_log::parse_filters(&text) {
            Ok(items) => apply(items),
            Err(e) => rbs_core::warn!(Area::App, "RBS_LOG: {e}"),
        }
    }
    if let Some(items) = level {
        apply(items);
    }
    rbs_log::bridge_log_crate();
    Ok(Run(Some(logger)))
}

fn apply(items: Vec<(Option<Area>, Filter)>) {
    for (area, filter) in items {
        match area {
            Some(area) => rbs_log::set_filter(area, filter),
            None => rbs_log::apply_level(filter),
        }
    }
}
