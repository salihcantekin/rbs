//! The local wall clock of live text sources (ADR-0129 decision 11): the system's
//! time zone through `chrono`, so clocks, `{time}` and countdowns to a time of day
//! follow DST and time-zone changes.

use std::time::{Duration, SystemTime};

use chrono::{
    DateTime, Datelike, Local, LocalResult, NaiveDateTime, NaiveTime, TimeZone, Timelike,
};
use rbs_sources::text_live::LocalTime;
use rbs_sources::text_worker::TextClock;

/// The system's local time.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalTextClock;

/// The instant `naive` names in the local zone: the first of two, or the first valid
/// instant after a DST gap.
fn resolve(naive: NaiveDateTime) -> Option<DateTime<Local>> {
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => Some(t),
        LocalResult::None => {
            // Gaps are whole minutes long; at most a day is searched.
            (1..=24 * 60).find_map(|m| {
                match Local.from_local_datetime(&(naive + chrono::Duration::minutes(m))) {
                    LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => Some(t),
                    LocalResult::None => None,
                }
            })
        }
    }
}

impl TextClock for LocalTextClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn local(&self) -> LocalTime {
        let now = Local::now();
        LocalTime {
            year: now.year(),
            month: now.month() as u8,
            day: now.day() as u8,
            hour: now.hour() as u8,
            minute: now.minute() as u8,
            second: now.second().min(59) as u8,
            weekday: now.weekday().num_days_from_monday() as u8,
            // A leap second's nanoseconds go past one second.
            nanos: now.nanosecond() % 1_000_000_000,
        }
    }

    fn until_local(&self, seconds: u32) -> Duration {
        let now = Local::now();
        let Some(at) = NaiveTime::from_num_seconds_from_midnight_opt(seconds, 0) else {
            return Duration::ZERO;
        };
        let today = now.date_naive();
        (0..3)
            .filter_map(|d| today.checked_add_days(chrono::Days::new(d)))
            .filter_map(|date| resolve(date.and_time(at)))
            .find(|t| *t >= now)
            .and_then(|t| (t - now).to_std().ok())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_clock_reads_a_valid_time_and_counts_within_a_day() {
        let c = LocalTextClock;
        let t = c.local();
        assert!((1..=12).contains(&t.month) && (1..=31).contains(&t.day));
        assert!(t.hour < 24 && t.minute < 60 && t.second < 60 && t.weekday < 7);
        for s in [0, 12 * 3600, 86_399] {
            assert!(c.until_local(s) <= Duration::from_secs(25 * 3600));
        }
    }
}
