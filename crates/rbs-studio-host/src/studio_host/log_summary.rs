//! The engine's stats summary in the log (ADR-0158 decisions 2 and 3, spec
//! `diagnostics-log.md`, `engine` row): one Debug line every 10 s, every second at
//! Trace, with what the outputs did since the last line.
//!
//! The control thread writes it from the counters the outputs already keep; nothing
//! is counted for it on a hot path. It runs only while the `engine` area lets Debug
//! through and an output runs on the main canvas. Then the control loop already looks
//! every round (`round_cap`), so a level raised in Settings is seen within a round and
//! needs no wake of its own. Otherwise it keeps no timer and no counters (ADR-0013).

use std::fmt::Write as _;
use std::time::Duration;

use rbs_core::log::{self, Area, Level};
use rbs_core::time::Timestamp;

use super::{Host, OutputId, OutputStats, Stats};

/// Between two lines at Debug.
const DEBUG_INTERVAL: Duration = Duration::from_secs(10);
/// Between two lines at Trace.
const TRACE_INTERVAL: Duration = Duration::from_secs(1);

/// The window the next line covers; `None` while no summary runs.
#[derive(Debug, Default)]
pub(super) struct LogSummary {
    window: Option<(Timestamp, Stats)>,
}

impl LogSummary {
    /// Starts or ends a window at `now`. The first call after a start only takes the
    /// counters; later calls give the line for the window that ended and start the
    /// next. Returns the line, if one is due, and the time until the next call.
    fn step(
        &mut self,
        now: Timestamp,
        interval: Duration,
        stats: impl FnOnce() -> Stats,
    ) -> (Option<String>, Duration) {
        if let Some((start, _)) = self.window {
            let due = start + interval;
            if now < due {
                return (None, due.saturating_since(now));
            }
        }
        let current = stats();
        let line = self
            .window
            .replace((now, current))
            .map(|(start, before)| line(now.saturating_since(start), &before, &current));
        (line, interval)
    }
}

impl Host {
    /// Writes the summary line when it is due, and arms the round's wait for the next
    /// one; nothing while no summary runs.
    pub(super) fn drive_log_summary(
        &mut self,
        now: Timestamp,
        wait: &mut Duration,
        armed: &mut bool,
    ) {
        if let Some(left) = self.log_summary_due(now) {
            super::arm(wait, armed, left);
        }
    }

    fn log_summary_due(&mut self, now: Timestamp) -> Option<Duration> {
        let interval = if log::enabled(Area::Engine, Level::Trace) {
            TRACE_INTERVAL
        } else if log::enabled(Area::Engine, Level::Debug) {
            DEBUG_INTERVAL
        } else {
            self.outs[0].log_summary.window = None;
            return None;
        };
        let recording = self.outs[0].recording.as_ref().map(|(_, id)| *id);
        let stream = self.outs[0].streaming.as_ref().map(|(_, _, id)| *id);
        if recording.is_none() && stream.is_none() {
            self.outs[0].log_summary.window = None;
            return None;
        }
        let counters = |id: Option<OutputId>| id.and_then(|id| self.studio.output_counters(id));
        let (line, left) = self.outs[0].log_summary.step(now, interval, || {
            Stats::of(counters(recording).as_ref(), counters(stream).as_ref())
        });
        if let Some(line) = line {
            rbs_core::debug!(Area::Engine, "{line}");
        }
        Some(left)
    }
}

/// The line for a window of `took` from the counters `before` and `after` it. Counts
/// are differences; an output that started again counts from zero.
fn line(took: Duration, before: &Stats, after: &Stats) -> String {
    let mut out = format!("stats over {:.1} s:", took.as_secs_f64());
    let mut output = |name: &str, b: Option<OutputStats>, a: Option<OutputStats>| {
        let Some(a) = a else {
            return;
        };
        let b = b.unwrap_or_default();
        let _ = write!(
            out,
            " {name} {} frames, {} dropped, {} packets",
            a.frames.saturating_sub(b.frames),
            a.dropped.saturating_sub(b.dropped),
            a.packets.saturating_sub(b.packets),
        );
        if name == "stream" {
            let _ = write!(
                out,
                ", {}, {} reconnects, {} transport drops",
                if a.connected {
                    "connected"
                } else {
                    "not connected"
                },
                a.reconnect_attempts.saturating_sub(b.reconnect_attempts),
                a.transport_drops.saturating_sub(b.transport_drops),
            );
        }
        out.push(';');
    };
    output("recording", before.recording, after.recording);
    output("stream", before.stream, after.stream);
    let _ = write!(
        out,
        " late ticks: render {}, audio {}",
        after
            .late_render_ticks
            .saturating_sub(before.late_render_ticks),
        after
            .late_audio_ticks
            .saturating_sub(before.late_audio_ticks),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> Timestamp {
        Timestamp::from_nanos(secs * 1_000_000_000)
    }

    fn recording(frames: u64, dropped: u64, packets: u64) -> Stats {
        Stats {
            recording: Some(OutputStats {
                frames,
                dropped,
                packets,
                ..OutputStats::default()
            }),
            ..Stats::default()
        }
    }

    #[test]
    fn the_first_step_only_takes_the_counters() {
        let mut s = LogSummary::default();
        let (line, left) = s.step(at(5), DEBUG_INTERVAL, || recording(10, 0, 20));
        assert_eq!(line, None);
        assert_eq!(left, DEBUG_INTERVAL);
    }

    #[test]
    fn a_step_before_the_window_ends_waits_without_reading_counters() {
        let mut s = LogSummary::default();
        let _ = s.step(at(5), DEBUG_INTERVAL, Stats::default);
        let (line, left) = s.step(at(8), DEBUG_INTERVAL, || panic!("read too early"));
        assert_eq!(line, None);
        assert_eq!(left, Duration::from_secs(7));
    }

    #[test]
    fn a_step_after_the_window_gives_the_differences() {
        let mut s = LogSummary::default();
        let _ = s.step(at(0), DEBUG_INTERVAL, || recording(100, 1, 200));
        let (line, left) = s.step(at(10), DEBUG_INTERVAL, || recording(700, 3, 1400));
        assert_eq!(
            line.as_deref(),
            Some(
                "stats over 10.0 s: recording 600 frames, 2 dropped, 1200 packets; \
                 late ticks: render 0, audio 0"
            )
        );
        assert_eq!(left, DEBUG_INTERVAL);
    }

    #[test]
    fn a_stream_line_has_its_link_state_and_an_output_that_started_counts_from_zero() {
        let before = recording(50, 0, 50);
        let after = Stats {
            stream: Some(OutputStats {
                frames: 30,
                dropped: 1,
                packets: 60,
                connected: true,
                reconnect_attempts: 2,
                transport_drops: 4,
            }),
            late_render_ticks: 3,
            late_audio_ticks: 1,
            ..recording(80, 0, 90)
        };
        assert_eq!(
            line(Duration::from_secs(1), &before, &after),
            "stats over 1.0 s: recording 30 frames, 0 dropped, 40 packets; stream 30 \
             frames, 1 dropped, 60 packets, connected, 2 reconnects, 4 transport drops; \
             late ticks: render 3, audio 1"
        );
    }

    #[test]
    fn counters_that_went_down_count_as_zero() {
        let line = line(
            Duration::from_secs(10),
            &recording(500, 5, 900),
            &recording(20, 0, 40),
        );
        assert!(
            line.contains("recording 0 frames, 0 dropped, 0 packets"),
            "{line}"
        );
    }
}
