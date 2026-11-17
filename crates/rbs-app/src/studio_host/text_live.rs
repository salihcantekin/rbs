//! Live text inputs (ADR-0129 decision 11): `text.timer` (control API 1.84), the
//! timers kept in `text-timers.toml`, and how long the main canvas's main recording
//! and stream have run, for `recording_time`, `stream_time` and `{scene}`.

use rbs_api::Response;
use rbs_api::error::{ApiError, ErrorKind};
use rbs_api::request::{TextTimer, TimerAction};
use rbs_api::types as api;
use rbs_core::time::Timestamp;
use rbs_record::{TextTimer as Timer, TimerRefused};
use rbs_scene::model::SourceId;
use rbs_sources::text::TextKind;
use rbs_sources::text_live::RunTime;

use super::{Host, OutputCanvas, slot};
use crate::text_timers;

/// How long the main recording and stream have run, on the studio's master clock.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct TextElapsed {
    recording: Option<RunTime>,
    stream: Option<RunTime>,
}

impl TextElapsed {
    /// The same values on a new studio's clock: the running part up to `old` is
    /// folded in and runs on from `new`.
    pub(super) fn moved(self, old: Timestamp, new: Timestamp) -> Self {
        let mv = |r: Option<RunTime>| {
            r.map(|r| RunTime {
                value: r.at(old),
                since: r.since.map(|_| new),
            })
        };
        Self {
            recording: mv(self.recording),
            stream: mv(self.stream),
        }
    }
}

/// One step of an elapsed time: `None` while off, running unless `paused`.
fn step(run: Option<RunTime>, on: bool, paused: bool, now: Timestamp) -> Option<RunTime> {
    if !on {
        return None;
    }
    let run = run.unwrap_or(RunTime {
        value: std::time::Duration::ZERO,
        since: Some(now),
    });
    Some(match (paused, run.since) {
        (true, Some(_)) => run.paused(now),
        (false, None) => RunTime {
            value: run.value,
            since: Some(now),
        },
        _ => run,
    })
}

fn timer_state(t: &Timer) -> api::TextTimerState {
    api::TextTimerState {
        source: t.source.get(),
        kind: match t.kind {
            TextKind::Countdown => "countdown",
            _ => "stopwatch",
        }
        .into(),
        running: t.running,
        value_ms: u64::try_from(t.value.as_millis()).unwrap_or(u64::MAX),
    }
}

impl Host {
    /// Hands the studio what live texts show, before each service: the program scene
    /// (the one a held switch is going to, ADR-0129 decision 11), and how long the
    /// main recording and stream have run. Restores the saved timers of the collection
    /// once.
    pub(super) fn feed_text_live(&mut self) {
        if !self.text_timers_restored {
            self.text_timers_restored = true;
            self.restore_text_timers();
        }
        let now = self.clock.now();
        let main = &self.outs[slot(OutputCanvas::Main)];
        let recording = main.recording.is_some() && !main.ending;
        let paused = matches!(
            main.record_state,
            api::RecordState::Recording { paused: true, .. }
        );
        let elapsed = TextElapsed {
            recording: step(self.text_elapsed.recording, recording, paused, now),
            stream: step(
                self.text_elapsed.stream,
                main.streaming.is_some(),
                false,
                now,
            ),
        };
        self.text_elapsed = elapsed;
        let scene = self.held_switch.unwrap_or(self.scene);
        let name = self.collection.scene(scene).map_or("", |s| s.name.as_str());
        self.studio
            .set_text_outputs(name, elapsed.recording, elapsed.stream);
    }

    /// The collection's saved timers, stopped at their values.
    fn restore_text_timers(&mut self) {
        let Some(collection) = self.autosave.as_ref().map(|a| a.id().to_owned()) else {
            return;
        };
        let timers = text_timers::restore(&self.text_timer_file, &collection, |s| {
            self.studio.text_timer_kind(s)
        });
        self.studio.set_text_timers(&timers);
    }

    /// Writes the timers of the collection in use to `text-timers.toml` when they
    /// changed. A file that cannot be written is left for the next change.
    pub(super) fn save_text_timers(&mut self) {
        let (Some(dir), Some(collection)) = (
            self.config_dir.clone(),
            self.autosave.as_ref().map(|a| a.id().to_owned()),
        ) else {
            return;
        };
        let timers = self.studio.text_timers();
        let exists = |s: SourceId| self.collection.source(s).is_some();
        if text_timers::merge(&mut self.text_timer_file, &collection, &timers, exists)
            && let Err(e) = text_timers::save(&dir, &self.text_timer_file)
        {
            eprintln!("rbs: text timers were not saved: {e}");
        }
    }

    /// `text.timer` (control API 1.84).
    pub(super) fn text_timer(&mut self, p: TextTimer) -> Result<Response, ApiError> {
        let action = match p.action {
            TimerAction::Start => rbs_record::TimerAction::Start,
            TimerAction::Pause => rbs_record::TimerAction::Pause,
            TimerAction::Reset => rbs_record::TimerAction::Reset,
        };
        let timer = self
            .studio
            .text_timer(SourceId::from_raw(p.source), action)
            .map_err(|e| match e {
                TimerRefused::NotFound => ApiError::new(
                    ErrorKind::NotFound,
                    format!("There is no source {}.", p.source),
                ),
                TimerRefused::NotATimer => ApiError::new(
                    ErrorKind::InvalidParams,
                    "the source is not a countdown by duration or a stopwatch",
                ),
            })?;
        self.save_text_timers();
        Ok(Response::TextTimerState(timer_state(&timer)))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use std::time::Duration;

    fn t(ms: u64) -> Timestamp {
        Timestamp::from_nanos(ms * 1_000_000)
    }

    #[test]
    fn elapsed_times_start_pause_resume_and_stop() {
        let on = step(None, true, false, t(1000)).unwrap();
        assert_eq!(on.at(t(3000)), Duration::from_secs(2));
        let paused = step(Some(on), true, true, t(4000)).unwrap();
        assert_eq!(
            (paused.since, paused.at(t(9000))),
            (None, Duration::from_secs(3))
        );
        let resumed = step(Some(paused), true, false, t(10_000)).unwrap();
        assert_eq!(resumed.at(t(11_000)), Duration::from_secs(4));
        assert_eq!(step(Some(resumed), false, false, t(12_000)), None);
        // A restart folds the running part in and runs on from the new clock.
        let e = TextElapsed {
            recording: Some(resumed),
            stream: None,
        }
        .moved(t(11_000), t(5));
        assert_eq!(e.recording.unwrap().at(t(1005)), Duration::from_secs(5));
    }
}
