//! The installed sink and the `log` crate bridge, process-wide (ADR-0158). One test:
//! the sink can be installed once per process.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::time::Duration;

use rbs_core::log::{self, Area, Filter};
use rbs_log::{Config, apply_level, bridge_log_crate, set_filter, start};

#[test]
fn macros_and_library_records_reach_the_file() {
    let dir = std::env::temp_dir().join(format!("rbs-log-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut config = Config::new(dir.clone(), "rbs");
    config.home = None;
    let logger = start(config).unwrap();
    log::install(logger.sink()).unwrap();
    apply_level(Filter::Info);
    set_filter(Area::Lib, Filter::Warn);
    bridge_log_crate();
    assert_eq!(log::filter(Area::Lib), Filter::Warn);
    assert_eq!(::log::max_level(), ::log::LevelFilter::Warn);

    rbs_core::info!(Area::App, "started");
    rbs_core::debug!(Area::App, "hidden");
    ::log::warn!(target: "wgpu_core", "surface lost");
    ::log::info!(target: "wgpu_core", "hidden too");

    // Trace for Lib is capped at Debug.
    set_filter(Area::Lib, Filter::Trace);
    assert_eq!(log::filter(Area::Lib), Filter::Debug);
    assert_eq!(::log::max_level(), ::log::LevelFilter::Debug);

    assert!(logger.flush(Duration::from_secs(20)));
    let text = std::fs::read_to_string(logger.path().unwrap()).unwrap();
    let msgs: Vec<&str> = text
        .lines()
        .map(|l| l.split_once("] ").map_or(l, |(_, m)| m))
        .collect();
    assert_eq!(msgs, ["started", "wgpu_core: surface lost"]);
    let _ = std::fs::remove_dir_all(&dir);
}
