//! Slide show sources in the host (ADR-0171): adding one for a folder and its
//! `media.*` commands (decision 9).

use super::*;

impl Host {
    /// A new slide show for `folder`, filling the canvas, its output size the canvas's
    /// (decision 5): one undo step.
    pub(super) fn add_slideshow_folder(
        &mut self,
        folder: &std::path::Path,
        name: Option<&str>,
    ) -> Result<SourceId, Refusal> {
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("The canvas is gone.".to_owned()))?;
        let (w, h) = (canvas.size.width, canvas.size.height);
        let layout = Layout::new([0.0, 0.0], [w as f32, h as f32]);
        let mut settings = Settings::new();
        settings.insert(
            rbs_sources::slideshow::FOLDER.into(),
            SettingValue::Text(folder.display().to_string()),
        );
        settings.insert(keys::WIDTH.into(), SettingValue::Int(i64::from(w)));
        settings.insert(keys::HEIGHT.into(), SettingValue::Int(i64::from(h)));
        let base = folder
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Slide show");
        self.add_placed(
            SourceKind::Slideshow,
            settings,
            name.unwrap_or(base),
            &layout,
        )
    }

    /// `media.*` on a slide show (ADR-0171 decision 9): play and pause run the timer,
    /// restart goes to the first picture, next and previous move one, and seek is
    /// refused. `source` is known to be a slide show.
    pub(super) fn slideshow_command(
        &mut self,
        raw: u32,
        op: MediaOp,
    ) -> Result<rbs_api::request::MediaStateInfo, Refusal> {
        use rbs_api::request::{MediaPlayState as Wire, MediaStateInfo};
        use rbs_sources::slideshow::SlideCommand;
        let id = SourceId::from_raw(raw);
        let done = match op {
            MediaOp::Seek(_) => {
                return Err(Refusal::Invalid(format!(
                    "Source {raw} is a slide show; it has no time to seek to."
                )));
            }
            MediaOp::Play => self.studio.set_slideshow_paused(id, false),
            MediaOp::Pause => self.studio.set_slideshow_paused(id, true),
            MediaOp::Restart => self.studio.slideshow_command(id, SlideCommand::Restart),
            MediaOp::Next => self.studio.slideshow_command(id, SlideCommand::Next),
            MediaOp::Previous => self.studio.slideshow_command(id, SlideCommand::Previous),
            MediaOp::State => true,
        };
        let millis = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        let Some(s) = self.studio.slideshow_state(id).filter(|_| done) else {
            if matches!(op, MediaOp::State) {
                return Ok(MediaStateInfo {
                    source: raw,
                    state: Wire::Standby,
                    cursor_ms: 0,
                    duration_ms: None,
                    index: None,
                    count: None,
                });
            }
            return Err(Refusal::Conflict(format!(
                "Slide show {raw} is not in a scene that is shown."
            )));
        };
        let state = if s.folder_error {
            Wire::Unavailable
        } else if !s.warm {
            Wire::Standby
        } else if s.paused {
            Wire::Paused
        } else if s.ended {
            Wire::Ended
        } else {
            Wire::Playing
        };
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        Ok(MediaStateInfo {
            source: raw,
            state,
            cursor_ms: millis(s.shown_for),
            duration_ms: s.interval.map(millis),
            index: s.index.map(count),
            count: Some(count(s.count)),
        })
    }

    /// The play state of every slide show the studio runs, for the UI (ADR-0171): a
    /// running one sets `playing`, so the state is sent again when its picture moves.
    pub(super) fn slideshow_ticks(&self, playing: &mut bool) -> Vec<MediaTick> {
        self.studio
            .slideshow_sources()
            .into_iter()
            .filter_map(|id| Some((id, self.studio.slideshow_state(id)?)))
            .map(|(id, s)| {
                let state = slide_state(&s);
                *playing |= state == 0;
                MediaTick {
                    id,
                    state,
                    cursor_ms: 0,
                    duration_ms: 0,
                    index: s.index.map_or(-1, |i| i32::try_from(i).unwrap_or(i32::MAX)),
                    count: u32::try_from(s.count).unwrap_or(u32::MAX),
                }
            })
            .collect()
    }
}

/// A slide show's state as [`MediaTick::state`] numbers it: 0 playing, 1 paused,
/// 3 ended, 4 standby, 5 unavailable (the folder cannot be read).
fn slide_state(s: &rbs_sources::slideshow::SlideState) -> u8 {
    if s.folder_error {
        5
    } else if !s.warm {
        4
    } else if s.paused {
        1
    } else if s.ended {
        3
    } else {
        0
    }
}
