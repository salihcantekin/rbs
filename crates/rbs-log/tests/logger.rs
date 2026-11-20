//! The running logger: lines reach the file through the `rbs-log` thread (ADR-0158).
//! Tests wait for `flush`, never for a duration (ADR-0032).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rbs_core::log::{Area, Level, Record};
use rbs_log::{Config, start};

fn fixed() -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(1_795_188_455_123)
}

/// Generous hang guard; the flush answers as soon as the lines are written.
const GUARD: Duration = Duration::from_secs(20);

#[test]
fn lines_reach_the_file_and_the_ring() {
    let dir = std::env::temp_dir().join(format!("rbs-log-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut config = Config::new(dir.clone(), "rbs");
    config.now = fixed;
    config.pid = 7;
    config.home = None;
    config.header = vec!["RBS test".to_owned()];
    let logger = start(config).unwrap();
    assert!(
        logger
            .path()
            .unwrap()
            .ends_with("rbs-20261120T152735Z-7.log")
    );

    let sink = logger.sink();
    for _ in 0..3 {
        sink.write(&Record {
            level: Level::Warn,
            area: Area::Stream,
            args: format_args!("reconnecting to {}", "srt://h:1?passphrase=x"),
            file: file!(),
            line: line!(),
        });
    }
    assert!(logger.flush(GUARD));

    let text = std::fs::read_to_string(logger.path().unwrap()).unwrap();
    let thread = std::thread::current().name().unwrap_or("?").to_owned();
    let expected = format!(
        "RBS test\n2026-11-20T15:27:35.123Z WARN  stream [{thread}] reconnecting to srt://h:1?<redacted>\n(repeated 2 times)\n"
    );
    assert_eq!(text, expected);
    assert_eq!(logger.recent().len(), 3);

    // A second run in the same second and process gets its own file.
    let mut again = Config::new(dir.clone(), "rbs");
    again.now = fixed;
    again.pid = 7;
    assert!(start(again).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
