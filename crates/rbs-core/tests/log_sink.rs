//! The process-wide log sink and filters (ADR-0158). One test, because the sink can
//! be installed once per process.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use rbs_core::log::{self, Area, Filter, Level, Record, Sink};

/// One captured line: level, area, message, file, line.
type Seen = (Level, Area, String, &'static str, u32);

struct Capture(Mutex<Vec<Seen>>);

impl Sink for Capture {
    fn write(&self, r: &Record<'_>) {
        if let Ok(mut lines) = self.0.lock() {
            lines.push((r.level, r.area, r.args.to_string(), r.file, r.line));
        }
    }
}

static CAPTURE: Capture = Capture(Mutex::new(Vec::new()));

fn counted(n: &AtomicUsize) -> usize {
    n.fetch_add(1, Ordering::Relaxed)
}

#[test]
fn macros_reach_the_installed_sink_only_when_their_level_is_on() {
    let evaluated = AtomicUsize::new(0);

    // Before install every area is off and arguments are not evaluated.
    rbs_core::error!(Area::App, "{}", counted(&evaluated));
    assert_eq!(evaluated.load(Ordering::Relaxed), 0);
    assert_eq!(log::filter(Area::App), Filter::Off);

    // A filter set before install is kept.
    log::set_filter(Area::Ui, Filter::Warn);
    log::install(&CAPTURE).expect("first install");
    assert_eq!(log::filter(Area::Ui), Filter::Warn);
    assert_eq!(log::install(&CAPTURE), Err(log::AlreadyInstalled));

    // Installed, but still off: nothing.
    rbs_core::error!(Area::App, "{}", counted(&evaluated));
    assert_eq!(evaluated.load(Ordering::Relaxed), 0);

    log::set_filter(Area::Record, Filter::Info);
    rbs_core::debug!(Area::Record, "{}", counted(&evaluated));
    assert_eq!(evaluated.load(Ordering::Relaxed), 0);
    let line = line!() + 1;
    rbs_core::info!(Area::Record, "started {}", 7);
    rbs_core::warn!(Area::Record, "slow");
    rbs_core::trace!(Area::Record, "never");
    rbs_core::error!(Area::Stream, "other area is off");

    log::set_all(Filter::Trace);
    rbs_core::trace!(Area::Lib, "now on");

    let lines = CAPTURE.0.lock().expect("lines").clone();
    assert_eq!(
        lines,
        vec![
            (
                Level::Info,
                Area::Record,
                "started 7".to_owned(),
                file!(),
                line
            ),
            (
                Level::Warn,
                Area::Record,
                "slow".to_owned(),
                file!(),
                line + 1
            ),
            (
                Level::Trace,
                Area::Lib,
                "now on".to_owned(),
                file!(),
                line + 6
            ),
        ]
    );
}
