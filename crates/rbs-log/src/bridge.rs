//! `log` crate records (wgpu, winit, rustls) into [`Area::Lib`] (ADR-0158 decision 7).

use std::sync::atomic::{AtomicU32, Ordering};

use rbs_core::log::{self, Area, Level, Record};

/// Lines one call site writes in full before only every [`EVERY`]th passes.
const FIRST: u32 = 10;
/// After [`FIRST`], one line in this many passes.
const EVERY: u32 = 1000;
/// Call-site slots; sites that share a slot share a count.
const SLOTS: usize = 256;

struct Bridge {
    seen: [AtomicU32; SLOTS],
}

static BRIDGE: Bridge = Bridge {
    seen: [const { AtomicU32::new(0) }; SLOTS],
};

fn level(l: ::log::Level) -> Level {
    match l {
        ::log::Level::Error => Level::Error,
        ::log::Level::Warn => Level::Warn,
        ::log::Level::Info => Level::Info,
        ::log::Level::Debug => Level::Debug,
        ::log::Level::Trace => Level::Trace,
    }
}

/// Whether the `n`th line (from 0) of one call site passes.
fn passes(n: u32) -> bool {
    n < FIRST || (n - FIRST) % EVERY == EVERY - 1
}

fn slot(file: &str, line: u32, level: Level) -> usize {
    // FNV-1a over the file name, line and level: cheap and stable.
    let mut h: u32 = 0x811c_9dc5;
    for b in file.bytes().chain(line.to_le_bytes()).chain([level as u8]) {
        h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    h as usize % SLOTS
}

impl ::log::Log for Bridge {
    fn enabled(&self, metadata: &::log::Metadata<'_>) -> bool {
        log::enabled(Area::Lib, level(metadata.level()))
    }

    fn log(&self, record: &::log::Record<'_>) {
        let lvl = level(record.level());
        if !log::enabled(Area::Lib, lvl) {
            return;
        }
        let file = record.file_static().unwrap_or("?");
        let line = record.line().unwrap_or(0);
        // A library that logs on every frame is cut down before anything is formatted.
        // Errors are never thinned. Records without a static file share one slot.
        if lvl != Level::Error {
            let n = self.seen[slot(file, line, lvl)].fetch_add(1, Ordering::Relaxed);
            if !passes(n) {
                return;
            }
        }
        log::write(&Record {
            level: lvl,
            area: Area::Lib,
            args: format_args!("{}: {}", record.target(), record.args()),
            file,
            line,
        });
    }

    fn flush(&self) {}
}

/// Sends `log` crate records to [`Area::Lib`]. Call once, after installing the sink;
/// a second call, or a logger another crate set, is left alone.
pub fn bridge_log_crate() {
    if ::log::set_logger(&BRIDGE).is_ok() {
        crate::set_filter(Area::Lib, log::filter(Area::Lib));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_noisy_call_site_is_thinned() {
        let passed = (0..2_010).filter(|&n| passes(n)).count();
        assert_eq!(passed, 10 + 2);
        assert!(slot("a.rs", 1, Level::Warn) < SLOTS);
        assert_ne!(slot("a.rs", 1, Level::Warn), slot("a.rs", 1, Level::Info));
    }
}
