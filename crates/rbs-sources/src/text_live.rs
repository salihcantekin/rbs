//! Live text (ADR-0129 decision 11, spec `scene-sources.md`, "Live kinds"): clock and
//! timer patterns, `{...}` fields, and what a live text source shows at a given time.
//! Pure functions: the text worker calls them on the control thread with its injected
//! clocks, so tests control every input.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rbs_core::time::Timestamp;
use rbs_scene::model::SourceId;

use crate::text::{TextKind, TextSettings};

/// The clock pattern when `format` is not set.
pub const CLOCK_PATTERN: &str = "HH:mm:ss";
/// The pattern of `{date}`.
pub const DATE_PATTERN: &str = "yyyy-MM-dd";
/// A countdown's `end_text` when not set.
pub const COUNTDOWN_END: &str = "00:00";

const SECOND: Duration = Duration::from_secs(1);

/// A local date and time, as a [`crate::text_worker::TextClock`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    /// The year, for example 2026.
    pub year: i32,
    /// 1 to 12.
    pub month: u8,
    /// 1 to 31.
    pub day: u8,
    /// 0 to 23.
    pub hour: u8,
    /// 0 to 59.
    pub minute: u8,
    /// 0 to 59 (a leap second reads as 59).
    pub second: u8,
    /// 0 = Monday ... 6 = Sunday.
    pub weekday: u8,
    /// Nanoseconds into the second.
    pub nanos: u32,
}

impl LocalTime {
    /// `time` in UTC (the fallback when no time zone is known).
    pub fn utc(time: SystemTime) -> Self {
        let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
        let secs = since.as_secs();
        let days = (secs / 86_400) as i64;
        let rem = secs % 86_400;
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: (rem / 3600) as u8,
            minute: (rem / 60 % 60) as u8,
            second: (rem % 60) as u8,
            // 1970-01-01 was a Thursday (3 when Monday is 0).
            weekday: ((days + 3).rem_euclid(7)) as u8,
            nanos: since.subsec_nanos(),
        }
    }

    /// Seconds into the day.
    fn seconds_of_day(&self) -> u32 {
        u32::from(self.hour) * 3600 + u32::from(self.minute) * 60 + u32::from(self.second)
    }
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i32, u8, u8) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    (year, month, day)
}

/// Time until the next occurrence of `target` seconds into a UTC day (the fallback
/// of [`crate::text_worker::TextClock::until_local`]). Zero when it is now.
pub fn until_utc(now: SystemTime, target: u32) -> Duration {
    let local = LocalTime::utc(now);
    let into = Duration::new(u64::from(local.seconds_of_day()), local.nanos);
    let target = Duration::from_secs(u64::from(target));
    if target >= into {
        target - into
    } else {
        Duration::from_secs(86_400) - (into - target)
    }
}

/// The language of weekday names: the app's UI language.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Language {
    /// English.
    #[default]
    English,
    /// Turkish.
    Turkish,
}

impl Language {
    /// The weekday name, 0 = Monday.
    pub fn weekday(self, day: u8) -> &'static str {
        const EN: [&str; 7] = [
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
            "Sunday",
        ];
        const TR: [&str; 7] = [
            "Pazartesi",
            "Salı",
            "Çarşamba",
            "Perşembe",
            "Cuma",
            "Cumartesi",
            "Pazar",
        ];
        let i = usize::from(day % 7);
        match self {
            Language::English => EN[i],
            Language::Turkish => TR[i],
        }
    }
}

/// Parses a countdown `target`: `HH:mm` or `HH:mm:ss`, as seconds into the day.
pub fn parse_target(target: &str) -> Option<u32> {
    let parts: Vec<&str> = target.trim().split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let num = |s: &str, max: u32| -> Option<u32> {
        if s.is_empty() || s.len() > 2 || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse::<u32>().ok().filter(|v| *v <= max)
    };
    let h = num(parts[0], 23)?;
    let m = num(parts[1], 59)?;
    let s = match parts.get(2) {
        Some(p) => num(p, 59)?,
        None => 0,
    };
    Some(h * 3600 + m * 60 + s)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Literal(String),
    Hour2,
    Hour,
    Minute,
    Second,
    Day,
    Month,
    Year,
    Weekday,
}

/// Splits a pattern: `HH`, `H`, `mm`, `ss`, `dd`, `MM`, `yyyy`, `ddd`, text in single or
/// double quotes, and any other character as itself.
fn tokens(pattern: &str) -> Vec<Token> {
    const NAMED: [(&str, Token); 8] = [
        ("yyyy", Token::Year),
        ("ddd", Token::Weekday),
        ("HH", Token::Hour2),
        ("mm", Token::Minute),
        ("ss", Token::Second),
        ("dd", Token::Day),
        ("MM", Token::Month),
        ("H", Token::Hour),
    ];
    let mut out = Vec::new();
    let mut literal = String::new();
    let mut rest = pattern;
    'outer: while let Some(c) = rest.chars().next() {
        if c == '\'' || c == '"' {
            let body = &rest[1..];
            let end = body.find(c).unwrap_or(body.len());
            literal.push_str(&body[..end]);
            rest = body.get(end + 1..).unwrap_or("");
            continue;
        }
        for (name, token) in &NAMED {
            if let Some(after) = rest.strip_prefix(name) {
                if !literal.is_empty() {
                    out.push(Token::Literal(std::mem::take(&mut literal)));
                }
                out.push(token.clone());
                rest = after;
                continue 'outer;
            }
        }
        literal.push(c);
        rest = &rest[c.len_utf8()..];
    }
    if !literal.is_empty() {
        out.push(Token::Literal(literal));
    }
    out
}

/// `time` with `pattern` (clock kinds and `{time}`, `{date}`).
pub fn format_clock(time: &LocalTime, pattern: &str, language: Language) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for t in tokens(pattern) {
        let _ = match t {
            Token::Literal(s) => write!(out, "{s}"),
            Token::Hour2 => write!(out, "{:02}", time.hour),
            Token::Hour => write!(out, "{}", time.hour),
            Token::Minute => write!(out, "{:02}", time.minute),
            Token::Second => write!(out, "{:02}", time.second),
            Token::Day => write!(out, "{:02}", time.day),
            Token::Month => write!(out, "{:02}", time.month),
            Token::Year => write!(out, "{:04}", time.year),
            Token::Weekday => write!(out, "{}", language.weekday(time.weekday)),
        };
    }
    out
}

/// The timer pattern when `format` is not set: `mm:ss`, `H:mm:ss` from one hour.
pub fn default_timer_pattern(seconds: u64) -> &'static str {
    if seconds >= 3600 { "H:mm:ss" } else { "mm:ss" }
}

/// `seconds` with `pattern` (timer kinds). The largest unit in the pattern takes the
/// whole: `mm:ss` shows 75:00 for 75 minutes. `dd` is days; `MM`, `yyyy` and `ddd`
/// show nothing.
pub fn format_elapsed(seconds: u64, pattern: &str) -> String {
    use std::fmt::Write;
    let tokens = tokens(pattern);
    let has = |want: &[Token]| tokens.iter().any(|t| want.contains(t));
    let mut rem = seconds;
    let mut take = |present: bool, unit: u64| {
        if present {
            let v = rem / unit;
            rem %= unit;
            v
        } else {
            0
        }
    };
    let days = take(has(&[Token::Day]), 86_400);
    let hours = take(has(&[Token::Hour, Token::Hour2]), 3600);
    let minutes = take(has(&[Token::Minute]), 60);
    let secs = rem;
    let mut out = String::new();
    for t in &tokens {
        let _ = match t {
            Token::Literal(s) => write!(out, "{s}"),
            Token::Day => write!(out, "{days:02}"),
            Token::Hour2 => write!(out, "{hours:02}"),
            Token::Hour => write!(out, "{hours}"),
            Token::Minute => write!(out, "{minutes:02}"),
            Token::Second => write!(out, "{secs:02}"),
            Token::Month | Token::Year | Token::Weekday => Ok(()),
        };
    }
    out
}

/// A value that runs on the master clock: an accumulated part plus the moment it last
/// started running (a timer, or how long a recording or stream has run).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunTime {
    /// Accumulated before `since`.
    pub value: Duration,
    /// Running since this master-clock time; `None` while stopped or paused.
    pub since: Option<Timestamp>,
}

impl RunTime {
    /// The value at `now`.
    pub fn at(&self, now: Timestamp) -> Duration {
        match self.since {
            Some(since) => self.value + now.saturating_since(since),
            None => self.value,
        }
    }

    /// The value at `now` with the running part folded in, stopped.
    pub fn paused(&self, now: Timestamp) -> Self {
        Self {
            value: self.at(now),
            since: None,
        }
    }
}

/// The live inputs the text worker draws from, other than the clocks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LiveInputs {
    /// Each countdown's and stopwatch's run state; a missing one is zero and stopped.
    pub timers: BTreeMap<SourceId, RunTime>,
    /// The main canvas's main recording; `None` while stopped.
    pub recording: Option<RunTime>,
    /// The main canvas's main stream; `None` while stopped.
    pub stream: Option<RunTime>,
    /// The main canvas's program scene name.
    pub scene: String,
    /// The language of weekday names.
    pub language: Language,
}

/// What a live text shows now and when that can change next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    /// The text to draw.
    pub text: String,
    /// How long until it can change, if it changes by itself.
    pub next: Option<Duration>,
}

/// The clocks a live text reads.
pub struct LiveClocks<'a> {
    /// The master clock's now.
    pub now: Timestamp,
    /// Local wall time now.
    pub local: LocalTime,
    /// Time until the next occurrence of a local time of day (seconds into the day).
    pub until_local: &'a dyn Fn(u32) -> Duration,
}

fn soonest(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Until the next whole second of a value counting up.
fn up_next(value: Duration) -> Duration {
    SECOND - Duration::from_nanos(u64::from(value.subsec_nanos()))
}

/// Until the shown (rounded up) value of a countdown changes.
fn down_next(remaining: Duration) -> Duration {
    match remaining.subsec_nanos() {
        0 => SECOND,
        n => Duration::from_nanos(u64::from(n)),
    }
}

fn wall_next(clocks: &LiveClocks<'_>) -> Duration {
    up_next(Duration::from_nanos(u64::from(clocks.local.nanos)))
}

/// An elapsed value shown with `pattern` (or the default timer pattern), rounded down.
fn elapsed_text(value: Duration, pattern: Option<&str>) -> String {
    let secs = value.as_secs();
    format_elapsed(secs, pattern.unwrap_or(default_timer_pattern(secs)))
}

/// A running or stopped elapsed time (`recording_time`, `stream_time`, a stopwatch).
fn elapsed(run: Option<&RunTime>, pattern: Option<&str>, stopped: &str, now: Timestamp) -> Shown {
    match run {
        Some(run) => {
            let value = run.at(now);
            Shown {
                text: elapsed_text(value, pattern),
                next: run.since.map(|_| up_next(value)),
            }
        }
        None => Shown {
            text: stopped.to_owned(),
            next: None,
        },
    }
}

/// What the text source `source` with `settings` shows, or `None` for plain text
/// (no live kind and `fields` off). A file source shows its file whatever `fields` is.
pub fn shown(
    source: SourceId,
    settings: &TextSettings,
    inputs: &LiveInputs,
    clocks: &LiveClocks<'_>,
) -> Option<Shown> {
    let pattern = settings.format.as_deref();
    let shown = match settings.kind {
        TextKind::Text => {
            if !settings.fields || settings.file.is_some() {
                return None;
            }
            expand_fields(&settings.block.text, inputs, clocks)
        }
        TextKind::Clock => Shown {
            text: format_clock(
                &clocks.local,
                pattern.unwrap_or(CLOCK_PATTERN),
                inputs.language,
            ),
            next: Some(wall_next(clocks)),
        },
        TextKind::Countdown => {
            let end = settings.end_text.as_deref().unwrap_or(COUNTDOWN_END);
            let (remaining, next_of) = match settings.target.as_deref().and_then(parse_target) {
                Some(target) => {
                    let left = (clocks.until_local)(target);
                    (left, Some(wall_next(clocks)))
                }
                None => {
                    let run = inputs.timers.get(&source).copied().unwrap_or_default();
                    let length = Duration::from_millis(settings.duration_ms.unwrap_or(0));
                    let left = length.saturating_sub(run.at(clocks.now));
                    (left, run.since.map(|_| down_next(left)))
                }
            };
            if remaining.is_zero() {
                Shown {
                    text: end.to_owned(),
                    next: settings.target.is_some().then(|| wall_next(clocks)),
                }
            } else {
                let secs = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
                Shown {
                    text: format_elapsed(secs, pattern.unwrap_or(default_timer_pattern(secs))),
                    next: next_of,
                }
            }
        }
        TextKind::Stopwatch => {
            let run = inputs.timers.get(&source).copied().unwrap_or_default();
            elapsed(Some(&run), pattern, "", clocks.now)
        }
        TextKind::RecordingTime => elapsed(
            inputs.recording.as_ref(),
            pattern,
            settings.end_text.as_deref().unwrap_or(""),
            clocks.now,
        ),
        TextKind::StreamTime => elapsed(
            inputs.stream.as_ref(),
            pattern,
            settings.end_text.as_deref().unwrap_or(""),
            clocks.now,
        ),
    };
    Some(shown)
}

/// Replaces `{time}`, `{date}`, `{time:FORMAT}`, `{scene}`, `{recording_time}` and
/// `{stream_time}` in `text`. Any other brace text stays as it is.
pub fn expand_fields(text: &str, inputs: &LiveInputs, clocks: &LiveClocks<'_>) -> Shown {
    let mut out = String::with_capacity(text.len());
    let mut next = None;
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after
            .find(['}', '{'])
            .filter(|i| after.as_bytes()[*i] == b'}')
        else {
            out.push('{');
            rest = after;
            continue;
        };
        let name = &after[..close];
        let wall = |p: &str| {
            (
                format_clock(&clocks.local, p, inputs.language),
                Some(wall_next(clocks)),
            )
        };
        let value = match name {
            "time" => Some(wall(CLOCK_PATTERN)),
            "date" => Some(wall(DATE_PATTERN)),
            "scene" => Some((inputs.scene.clone(), None)),
            "recording_time" => {
                let s = elapsed(inputs.recording.as_ref(), None, "", clocks.now);
                Some((s.text, s.next))
            }
            "stream_time" => {
                let s = elapsed(inputs.stream.as_ref(), None, "", clocks.now);
                Some((s.text, s.next))
            }
            _ => name.strip_prefix("time:").map(wall),
        };
        match value {
            Some((v, n)) => {
                out.push_str(&v);
                next = soonest(next, n);
            }
            None => {
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Shown { text: out, next }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::text::read_text_lenient;
    use rbs_scene::model::{SettingValue, Settings};

    #[allow(clippy::too_many_arguments)] // One field each, as the struct reads.
    fn at(y: i32, mo: u8, d: u8, h: u8, mi: u8, s: u8, wd: u8, nanos: u32) -> LocalTime {
        LocalTime {
            year: y,
            month: mo,
            day: d,
            hour: h,
            minute: mi,
            second: s,
            weekday: wd,
            nanos,
        }
    }

    fn text(pairs: &[(&str, SettingValue)]) -> TextSettings {
        let mut s = Settings::new();
        s.insert("width".into(), SettingValue::Int(100));
        s.insert("height".into(), SettingValue::Int(50));
        for (k, v) in pairs {
            s.insert((*k).into(), v.clone());
        }
        let (t, adjusted) = read_text_lenient(&s);
        assert!(adjusted.is_empty(), "{adjusted:?}");
        t
    }

    fn kind(k: &str) -> (&'static str, SettingValue) {
        ("kind", SettingValue::Text(k.into()))
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    const S: SourceId = SourceId::from_raw(7);

    fn show(settings: &TextSettings, inputs: &LiveInputs, now_ms: u64, local: LocalTime) -> Shown {
        let until = |t: u32| {
            let into = Duration::new(u64::from(local.seconds_of_day()), local.nanos);
            let t = Duration::from_secs(u64::from(t));
            if t >= into {
                t - into
            } else {
                Duration::from_secs(86_400) - (into - t)
            }
        };
        let clocks = LiveClocks {
            now: Timestamp::from_nanos(now_ms * 1_000_000),
            local,
            until_local: &until,
        };
        shown(S, settings, inputs, &clocks).unwrap()
    }

    #[test]
    fn utc_dates_and_weekdays_are_right() {
        let t = UNIX_EPOCH + Duration::new(1_795_398_245, 500); // 2026-11-23 01:44:05 UTC, Monday
        assert_eq!(LocalTime::utc(t), at(2026, 11, 23, 1, 44, 5, 0, 500));
        assert_eq!(LocalTime::utc(UNIX_EPOCH), at(1970, 1, 1, 0, 0, 0, 3, 0));
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(
            (LocalTime::utc(leap).month, LocalTime::utc(leap).day),
            (2, 29)
        );
        assert_eq!(
            until_utc(t, 2 * 3600),
            Duration::new(16 * 60 - 6, 999_999_500)
        );
        assert_eq!(
            until_utc(t, 3600),
            Duration::new(86_400 - 44 * 60 - 6, 999_999_500)
        );
    }

    #[test]
    fn clock_patterns_format_every_token_and_quoted_text() {
        let t = at(2026, 3, 7, 9, 5, 2, 5, 0);
        let en = Language::English;
        assert_eq!(format_clock(&t, CLOCK_PATTERN, en), "09:05:02");
        assert_eq!(format_clock(&t, "H:mm", en), "9:05");
        assert_eq!(
            format_clock(&t, "dd.MM.yyyy ddd", en),
            "07.03.2026 Saturday"
        );
        assert_eq!(format_clock(&t, "ddd", Language::Turkish), "Cumartesi");
        assert_eq!(format_clock(&t, "'Time:' HH\"h\"", en), "Time: 09h");
        assert_eq!(format_clock(&t, "'unterminated HH", en), "unterminated HH");
        assert_eq!(format_clock(&t, "x HH y", en), "x 09 y");
    }

    #[test]
    fn timer_patterns_give_the_largest_unit_the_whole() {
        assert_eq!(format_elapsed(75 * 60 + 3, "mm:ss"), "75:03");
        assert_eq!(format_elapsed(75 * 60 + 3, "H:mm:ss"), "1:15:03");
        assert_eq!(format_elapsed(26 * 3600, "dd HH:mm"), "01 02:00");
        assert_eq!(format_elapsed(90, "ss"), "90");
        assert_eq!(format_elapsed(5, "yyyy MM ddd ss"), "   05");
        assert_eq!(default_timer_pattern(3599), "mm:ss");
        assert_eq!(default_timer_pattern(3600), "H:mm:ss");
    }

    #[test]
    fn targets_parse_as_seconds_into_the_day() {
        assert_eq!(parse_target("18:30"), Some(18 * 3600 + 30 * 60));
        assert_eq!(parse_target("07:05:09"), Some(7 * 3600 + 5 * 60 + 9));
        for bad in ["24:00", "12", "12:60", "1:2:3:4", "aa:bb", "", "123:00"] {
            assert_eq!(parse_target(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_clock_shows_local_time_and_wakes_on_the_next_wall_second() {
        let s = text(&[kind("clock")]);
        let shown = show(
            &s,
            &LiveInputs::default(),
            0,
            at(2026, 1, 1, 23, 59, 58, 3, 250_000_000),
        );
        assert_eq!(shown.text, "23:59:58");
        assert_eq!(shown.next, Some(ms(750)));
    }

    #[test]
    fn a_duration_countdown_rounds_up_and_ends_with_its_end_text() {
        let s = text(&[
            kind("countdown"),
            ("duration_ms", SettingValue::Int(90_000)),
        ]);
        let noon = at(2026, 1, 1, 12, 0, 0, 3, 0);
        let mut inputs = LiveInputs::default();
        // Not started: the whole length, no wake-up.
        let idle = show(&s, &inputs, 5_000, noon);
        assert_eq!((idle.text.as_str(), idle.next), ("01:30", None));
        // Started at 1 s, now 1.4 s: 89.6 s left shows 01:30, changes in 0.6 s.
        inputs.timers.insert(
            S,
            RunTime {
                value: Duration::ZERO,
                since: Some(Timestamp::from_nanos(1_000_000_000)),
            },
        );
        let running = show(&s, &inputs, 1_400, noon);
        assert_eq!(
            (running.text.as_str(), running.next),
            ("01:30", Some(ms(600)))
        );
        let end = show(&s, &inputs, 95_000, noon);
        assert_eq!((end.text.as_str(), end.next), (COUNTDOWN_END, None));
        let custom = text(&[
            kind("countdown"),
            ("duration_ms", SettingValue::Int(1)),
            ("end_text", SettingValue::Text("Live!".into())),
        ]);
        assert_eq!(show(&custom, &inputs, 95_000, noon).text, "Live!");
        // From one hour the hour is added.
        let long = text(&[
            kind("countdown"),
            ("duration_ms", SettingValue::Int(3_600_000)),
        ]);
        assert_eq!(show(&long, &LiveInputs::default(), 0, noon).text, "1:00:00");
    }

    #[test]
    fn a_countdown_to_a_time_of_day_counts_to_the_next_occurrence() {
        let s = text(&[
            kind("countdown"),
            ("target", SettingValue::Text("18:00".into())),
        ]);
        let before = show(
            &s,
            &LiveInputs::default(),
            0,
            at(2026, 1, 1, 17, 58, 30, 3, 500_000_000),
        );
        assert_eq!(
            (before.text.as_str(), before.next),
            ("01:30", Some(ms(500)))
        );
        let exact = show(
            &s,
            &LiveInputs::default(),
            0,
            at(2026, 1, 1, 18, 0, 0, 3, 0),
        );
        assert_eq!(exact.text, COUNTDOWN_END);
        // Passed today: tomorrow.
        let after = show(
            &s,
            &LiveInputs::default(),
            0,
            at(2026, 1, 1, 18, 0, 1, 3, 0),
        );
        assert_eq!(after.text, "23:59:59");
    }

    #[test]
    fn a_stopwatch_and_elapsed_times_count_up_and_stop() {
        let sw = text(&[
            kind("stopwatch"),
            ("format", SettingValue::Text("ss".into())),
        ]);
        let noon = at(2026, 1, 1, 12, 0, 0, 3, 0);
        let mut inputs = LiveInputs::default();
        assert_eq!(show(&sw, &inputs, 9_000, noon).text, "00");
        inputs.timers.insert(
            S,
            RunTime {
                value: ms(2_000),
                since: Some(Timestamp::from_nanos(10_000_000_000)),
            },
        );
        let r = show(&sw, &inputs, 13_250, noon);
        assert_eq!((r.text.as_str(), r.next), ("05", Some(ms(750))));

        let rec = text(&[kind("recording_time")]);
        assert_eq!(
            show(&rec, &inputs, 0, noon),
            Shown {
                text: String::new(),
                next: None
            }
        );
        let stopped = text(&[
            kind("recording_time"),
            ("end_text", SettingValue::Text("off".into())),
        ]);
        assert_eq!(show(&stopped, &inputs, 0, noon).text, "off");
        inputs.recording = Some(RunTime {
            value: ms(3_600_000),
            since: None,
        });
        // Paused: the value stands still.
        assert_eq!(
            show(&rec, &inputs, 50_000, noon),
            Shown {
                text: "1:00:00".into(),
                next: None
            }
        );
        inputs.stream = Some(RunTime {
            value: Duration::ZERO,
            since: Some(Timestamp::ZERO),
        });
        let st = show(&text(&[kind("stream_time")]), &inputs, 61_500, noon);
        assert_eq!((st.text.as_str(), st.next), ("01:01", Some(ms(500))));
    }

    #[test]
    fn fields_expand_only_when_on_and_leave_other_braces() {
        let noon = at(2026, 1, 2, 12, 0, 5, 4, 100_000_000);
        let inputs = LiveInputs {
            scene: "Intro".into(),
            ..LiveInputs::default()
        };
        let plain = text(&[("text", SettingValue::Text("{time}".into()))]);
        let until = |_: u32| Duration::ZERO;
        let clocks = LiveClocks {
            now: Timestamp::ZERO,
            local: noon,
            until_local: &until,
        };
        assert_eq!(shown(S, &plain, &inputs, &clocks), None);
        let on = text(&[
            (
                "text",
                SettingValue::Text(
                    "{scene} {date} {time} {time:H'h'} {x} {rec{time} {recording_time}|".into(),
                ),
            ),
            ("fields", SettingValue::Bool(true)),
        ]);
        let r = show(&on, &inputs, 0, noon);
        assert_eq!(r.text, "Intro 2026-01-02 12:00:05 12h {x} {rec12:00:05 |");
        assert_eq!(r.next, Some(ms(900)));
        let scene_only = text(&[
            ("text", SettingValue::Text("on {scene}".into())),
            ("fields", SettingValue::Bool(true)),
        ]);
        assert_eq!(show(&scene_only, &inputs, 0, noon).next, None);
    }
}
