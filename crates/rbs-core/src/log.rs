//! The diagnostics log facade (ADR-0158, spec `diagnostics-log.md`).
//!
//! Code reports what it does with [`error!`](crate::error), [`warn!`](crate::warn),
//! [`info!`](crate::info), [`debug!`](crate::debug) and [`trace!`](crate::trace), each
//! naming its [`Area`] first. A composition root installs one [`Sink`] (the file writer
//! in `rbs-log`) and sets each area's [`Filter`]. Until then every area is
//! [`Filter::Off`] and a call costs one relaxed atomic load: its arguments are not
//! evaluated.
//!
//! Never call these macros on a hot path (`docs/architecture/hot-paths.md`). Count
//! events there and let a thread that is not hot write a summary.

use std::fmt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

/// How serious a line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Level {
    /// Something the user asked for failed, or something running stopped.
    Error = 1,
    /// It goes on, degraded or on a fallback.
    Warn = 2,
    /// Life-cycle steps and user actions; a few lines a minute.
    Info = 3,
    /// Decisions and details.
    Debug = 4,
    /// Every command and message; still never per frame.
    Trace = 5,
}

impl Level {
    /// The upper-case name written in a log line, padded to five characters.
    pub const fn label(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

/// The most detailed level an area writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(u8)]
pub enum Filter {
    /// Nothing.
    #[default]
    Off = 0,
    /// Errors only.
    Error = 1,
    /// Warnings and errors.
    Warn = 2,
    /// Normal.
    Info = 3,
    /// Detailed.
    Debug = 4,
    /// Everything.
    Trace = 5,
}

impl Filter {
    /// Every filter, from `Off` to `Trace`.
    pub const ALL: [Filter; 6] = [
        Filter::Off,
        Filter::Error,
        Filter::Warn,
        Filter::Info,
        Filter::Debug,
        Filter::Trace,
    ];

    /// The lower-case name used in settings and `RBS_LOG`.
    pub const fn name(self) -> &'static str {
        match self {
            Filter::Off => "off",
            Filter::Error => "error",
            Filter::Warn => "warn",
            Filter::Info => "info",
            Filter::Debug => "debug",
            Filter::Trace => "trace",
        }
    }

    /// The filter named `name` (see [`Filter::name`]), ignoring ASCII case.
    pub fn from_name(name: &str) -> Option<Filter> {
        Filter::ALL
            .into_iter()
            .find(|f| f.name().eq_ignore_ascii_case(name))
    }

    const fn from_u8(v: u8) -> Filter {
        match v {
            1 => Filter::Error,
            2 => Filter::Warn,
            3 => Filter::Info,
            4 => Filter::Debug,
            5 => Filter::Trace,
            _ => Filter::Off,
        }
    }
}

/// The subsystem a line comes from. Each has its own filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum Area {
    /// Start, quit, settings, profiles.
    App,
    /// Windows and the UI thread.
    Ui,
    /// The studio host and its control thread.
    Studio,
    /// The media graph and its partitions.
    Engine,
    /// GPU adapter and device.
    Gpu,
    /// Cameras, screens and windows.
    Capture,
    /// Audio devices and the audio graph.
    Audio,
    /// Encoders.
    Encode,
    /// Recordings, replay buffer, virtual camera.
    Record,
    /// Streams and their connections.
    Stream,
    /// Browser sources and their helper.
    Browser,
    /// Control API, MCP, remote guests.
    Remote,
    /// Downloaded add-ons.
    AddOns,
    /// Plugins.
    Plugin,
    /// Profiles, collections, import and export.
    UserData,
    /// Libraries reporting through the `log` crate.
    Lib,
}

/// The number of areas. New areas go before `Lib`, which stays last.
const AREAS: usize = 16;
const _: () = assert!(Area::Lib as usize + 1 == AREAS);

impl Area {
    /// Every area once, in declaration order.
    pub const ALL: [Area; AREAS] = [
        Area::App,
        Area::Ui,
        Area::Studio,
        Area::Engine,
        Area::Gpu,
        Area::Capture,
        Area::Audio,
        Area::Encode,
        Area::Record,
        Area::Stream,
        Area::Browser,
        Area::Remote,
        Area::AddOns,
        Area::Plugin,
        Area::UserData,
        Area::Lib,
    ];

    /// The lower-case name written in a log line and used in `RBS_LOG`.
    pub const fn name(self) -> &'static str {
        match self {
            Area::App => "app",
            Area::Ui => "ui",
            Area::Studio => "studio",
            Area::Engine => "engine",
            Area::Gpu => "gpu",
            Area::Capture => "capture",
            Area::Audio => "audio",
            Area::Encode => "encode",
            Area::Record => "record",
            Area::Stream => "stream",
            Area::Browser => "browser",
            Area::Remote => "remote",
            Area::AddOns => "addons",
            Area::Plugin => "plugin",
            Area::UserData => "userdata",
            Area::Lib => "lib",
        }
    }

    /// The area named `name` (see [`Area::name`]), ignoring ASCII case.
    pub fn from_name(name: &str) -> Option<Area> {
        Area::ALL
            .into_iter()
            .find(|a| a.name().eq_ignore_ascii_case(name))
    }
}

/// One line before it is formatted.
#[derive(Debug, Clone, Copy)]
pub struct Record<'a> {
    /// How serious it is.
    pub level: Level,
    /// Where it comes from.
    pub area: Area,
    /// The message.
    pub args: fmt::Arguments<'a>,
    /// The source file of the call.
    pub file: &'static str,
    /// The source line of the call.
    pub line: u32,
}

/// Where lines go. Implementations must not block the caller: the UI thread and the
/// control thread call it.
pub trait Sink: Send + Sync {
    /// Takes one line whose level its area lets through.
    fn write(&self, record: &Record<'_>);
}

/// [`install`] was already called; the first sink stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadyInstalled;

impl fmt::Display for AlreadyInstalled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a log sink is already installed")
    }
}

impl std::error::Error for AlreadyInstalled {}

/// Per-area filters. One global set serves the macros; tests may make their own.
#[derive(Debug)]
pub struct Filters([AtomicU8; AREAS]);

impl Filters {
    /// Every area off.
    pub const fn new() -> Filters {
        Filters([const { AtomicU8::new(0) }; AREAS])
    }

    /// Sets `area`'s filter.
    pub fn set(&self, area: Area, filter: Filter) {
        self.0[area as usize].store(filter as u8, Ordering::Relaxed);
    }

    /// `area`'s filter.
    pub fn get(&self, area: Area) -> Filter {
        Filter::from_u8(self.0[area as usize].load(Ordering::Relaxed))
    }

    /// Whether `area` writes lines of `level`: one relaxed load.
    #[inline(always)]
    pub fn enabled(&self, area: Area, level: Level) -> bool {
        self.0[area as usize].load(Ordering::Relaxed) >= level as u8
    }
}

impl Default for Filters {
    fn default() -> Self {
        Filters::new()
    }
}

static FILTERS: Filters = Filters::new();
static SINK: OnceLock<&'static dyn Sink> = OnceLock::new();

/// Installs the process's sink. Succeeds once. Filters stay as they are (all off unless
/// set), so set them after installing.
pub fn install(sink: &'static dyn Sink) -> Result<(), AlreadyInstalled> {
    SINK.set(sink).map_err(|_| AlreadyInstalled)
}

/// Sets `area`'s filter. Takes effect at the next call. Low level, like [`set_all`].
pub fn set_filter(area: Area, filter: Filter) {
    FILTERS.set(area, filter);
}

/// Sets every area's filter, `Lib` included. Low level, for composition roots and
/// tests: apps set levels through `rbs-log`, which keeps `Lib` and the `log` crate's
/// own filter in step (ADR-0158 decision 7).
pub fn set_all(filter: Filter) {
    for area in Area::ALL {
        FILTERS.set(area, filter);
    }
}

/// `area`'s filter.
pub fn filter(area: Area) -> Filter {
    FILTERS.get(area)
}

/// Whether a call in `area` at `level` reaches the sink (when one is installed).
#[inline(always)]
pub fn enabled(area: Area, level: Level) -> bool {
    FILTERS.enabled(area, level)
}

/// Hands `record` to the sink, if one is installed. Macros call it after
/// [`enabled`]; call it directly only to forward a line already filtered. Kept out of
/// line so a call site whose level is off is only a load and a branch.
#[cold]
#[inline(never)]
pub fn write(record: &Record<'_>) {
    if let Some(sink) = SINK.get() {
        sink.write(record);
    }
}

/// Writes a line when `$area` lets `$level` through. Prefer the level macros.
#[macro_export]
macro_rules! log_at {
    ($level:expr, $area:expr, $($arg:tt)+) => {{
        let level: $crate::log::Level = $level;
        let area: $crate::log::Area = $area;
        if $crate::log::enabled(area, level) {
            $crate::log::write(&$crate::log::Record {
                level,
                area,
                args: ::std::format_args!($($arg)+),
                file: ::std::file!(),
                line: ::std::line!(),
            });
        }
    }};
}

/// An error line: something the user asked for failed, or something running stopped.
#[macro_export]
macro_rules! error {
    ($area:expr, $($arg:tt)+) => { $crate::log_at!($crate::log::Level::Error, $area, $($arg)+) };
}

/// A warning line: it goes on, degraded or on a fallback.
#[macro_export]
macro_rules! warn {
    ($area:expr, $($arg:tt)+) => { $crate::log_at!($crate::log::Level::Warn, $area, $($arg)+) };
}

/// An info line: a life-cycle step or a user action.
#[macro_export]
macro_rules! info {
    ($area:expr, $($arg:tt)+) => { $crate::log_at!($crate::log::Level::Info, $area, $($arg)+) };
}

/// A debug line: a decision or a detail.
#[macro_export]
macro_rules! debug {
    ($area:expr, $($arg:tt)+) => { $crate::log_at!($crate::log::Level::Debug, $area, $($arg)+) };
}

/// A trace line: every command or message, never per frame.
#[macro_export]
macro_rules! trace {
    ($area:expr, $($arg:tt)+) => { $crate::log_at!($crate::log::Level::Trace, $area, $($arg)+) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_start_off() {
        let f = Filters::new();
        for area in Area::ALL {
            assert_eq!(f.get(area), Filter::Off);
            assert!(!f.enabled(area, Level::Error));
        }
    }

    #[test]
    fn a_filter_lets_its_level_and_more_serious_through() {
        let f = Filters::new();
        f.set(Area::Stream, Filter::Info);
        assert!(f.enabled(Area::Stream, Level::Error));
        assert!(f.enabled(Area::Stream, Level::Info));
        assert!(!f.enabled(Area::Stream, Level::Debug));
        assert!(!f.enabled(Area::Record, Level::Error));
    }

    #[test]
    fn names_round_trip() {
        for (i, area) in Area::ALL.into_iter().enumerate() {
            assert_eq!(area as usize, i);
            assert_eq!(Area::from_name(area.name()), Some(area));
        }
        for filter in Filter::ALL {
            assert_eq!(Filter::from_name(filter.name()), Some(filter));
            assert_eq!(Filter::from_u8(filter as u8), filter);
        }
        assert_eq!(Area::from_name("STREAM"), Some(Area::Stream));
        assert_eq!(Area::from_name("nope"), None);
    }

    #[test]
    fn levels_order_by_detail() {
        assert!(Level::Error < Level::Trace);
        assert_eq!(Level::Warn.label().len(), 5);
    }
}
