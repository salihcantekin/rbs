//! Values that move while the studio plays and that the host sends apart from the
//! rows: each media source's play state (ADR-0108 decision 10), the time left on
//! each running Show cue (ADR-0105) and the audio a fader or mute changed. Each is
//! patched into the rows it moves instead of the host resending every row (F-10).

use rbs_app::studio_host::{AudioPatch, MediaTick, SourceRow};
use rbs_scene::model::ItemId;
use slint::Model;

use super::{Applied, StudioUi};

/// The latest play states and cue times.
#[derive(Debug, Default)]
pub(super) struct LiveTicks {
    /// The play state of each media source, from `Update::Media`.
    pub media: Vec<MediaTick>,
    /// The milliseconds left on each running Show cue, by item, from `Update::Cues`.
    cues: Vec<(ItemId, u32)>,
    /// The Mixer's rows from the last `Update::Heard`, which `Update::SourceAudio`
    /// patches.
    heard: Vec<SourceRow>,
}

impl LiveTicks {
    /// A media source's (state, cursor ms, duration ms); state 4 (standby) when the
    /// studio does not run it.
    pub fn media_fields(&self, id: i32) -> (i32, i32, i32) {
        self.media
            .iter()
            .find(|t| t.id.get() as i32 == id)
            .map_or((4, 0, 0), |t| {
                (
                    i32::from(t.state),
                    clamp_ms(t.cursor_ms),
                    clamp_ms(t.duration_ms),
                )
            })
    }

    /// The milliseconds left on `item`'s running Show cue; 0 when none runs.
    pub fn cue_left(&self, item: ItemId) -> i32 {
        self.cues
            .iter()
            .find(|(id, _)| *id == item)
            .map_or(0, |(_, ms)| clamp_ms(*ms))
    }
}

fn clamp_ms(ms: u32) -> i32 {
    i32::try_from(ms).unwrap_or(i32::MAX)
}

impl StudioUi {
    /// Takes the newest play states and writes them into the rows that changed.
    pub(super) fn set_media_ticks(&mut self, ticks: Vec<MediaTick>) {
        self.live.media = ticks;
        for i in 0..self.sources.row_count() {
            let Some(mut row) = self.sources.row_data(i) else {
                continue;
            };
            if row.slides {
                let tick = self.live.media.iter().find(|t| t.id.get() as i32 == row.id);
                if super::slides::patch_live(&mut row, tick) {
                    self.sources.set_row_data(i, row);
                }
                continue;
            }
            if !row.media {
                continue;
            }
            let before = (row.media_state, row.media_pos_ms, row.media_len_ms);
            let (state, pos, len) = self.live.media_fields(row.id);
            if before != (state, pos, len) {
                row.media_state = state;
                row.media_pos_ms = pos;
                row.media_len_ms = len;
                self.sources.set_row_data(i, row);
            }
        }
    }

    /// Takes the newest cue times and writes them into the rows whose time changed.
    pub(super) fn set_cues(&mut self, cues: Vec<(ItemId, u32)>) {
        self.live.cues = cues;
        for i in 0..self.sources.row_count() {
            let Some(mut row) = self.sources.row_data(i) else {
                continue;
            };
            let left = self.live.cue_left(ItemId::from_raw(row.item_id as u32));
            if row.cue_remaining_ms != left {
                row.cue_remaining_ms = left;
                self.sources.set_row_data(i, row);
            }
        }
    }

    /// Takes the Mixer's rows (`Update::Heard`).
    pub(super) fn take_heard(&mut self, rows: Vec<SourceRow>) {
        self.set_heard(&rows);
        self.live.heard = rows;
    }

    /// Takes the Sources list's rows (`Update::Sources`).
    pub(super) fn take_rows(&mut self, rows: Vec<SourceRow>, out: &mut Applied) {
        self.set_rows(rows);
        out.focus_shape = self.focus_shape.take();
        out.focus_text = self.focus_text.take();
        out.focus_browser = self.focus_browser.take();
        self.refresh_existing();
        self.refresh_marks();
    }

    /// Writes new audio into the rows the host sent last and shows them again
    /// (`Update::SourceAudio`); the models change only where a value did.
    pub(super) fn patch_audio(&mut self, patches: &[AudioPatch], out: &mut Applied) {
        let patch = |rows: &mut [SourceRow]| {
            for row in rows {
                if let Some(p) = patches.iter().find(|p| p.id == row.id) {
                    row.audio = p.audio;
                    row.heard = p.heard;
                }
            }
        };
        let mut heard = std::mem::take(&mut self.live.heard);
        patch(&mut heard);
        self.take_heard(heard);
        let mut rows = self.rows.clone();
        patch(&mut rows);
        self.take_rows(rows, out);
    }

    /// Takes the master when only it changed (`Update::Master`).
    pub(super) fn patch_master(&mut self, master: rbs_scene::model::MasterAudio) {
        if self.master_audio != master {
            self.master_audio = master;
            self.show_master();
        }
    }
}
