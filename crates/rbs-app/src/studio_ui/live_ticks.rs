//! Values that move while the studio plays and that the host sends apart from the
//! rows: each media source's play state (ADR-0108 decision 10) and the time left on
//! each running Show cue (ADR-0105). Each is patched into the source rows whose
//! value changed; the other rows stay as they are (F-10).

use rbs_app::studio_host::MediaTick;
use rbs_scene::model::ItemId;
use slint::Model;

use super::StudioUi;

/// The latest play states and cue times.
#[derive(Debug, Default)]
pub(super) struct LiveTicks {
    /// The play state of each media source, from `Update::Media`.
    pub media: Vec<MediaTick>,
    /// The milliseconds left on each running Show cue, by item, from `Update::Cues`.
    cues: Vec<(ItemId, u32)>,
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
}
