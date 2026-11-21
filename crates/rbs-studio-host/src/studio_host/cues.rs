//! The time left on running Show cues (ADR-0105), sent apart from the rows so a
//! running cue does not rebuild the source list (F-10).

use super::*;

impl Host {
    /// Sends the time left on each running Show cue when it changed. Without a
    /// running cue this walks the shown scenes' items and allocates nothing.
    pub(super) fn publish_cues(&mut self, notify: &dyn Fn(Update)) {
        let now = self.clock.now();
        let mut cues: Vec<(ItemId, u32)> = Vec::new();
        for scene_id in [
            self.globals(),
            Some(self.scene),
            self.preview(),
            Some(self.edited()),
        ]
        .into_iter()
        .flatten()
        {
            let Some(scene) = self.collection.scene(scene_id) else {
                continue;
            };
            for item in &scene.items {
                if let Some(left) = self.studio.cue_remaining_ms(item.id, now)
                    && left > 0
                    && !cues.iter().any(|(id, _)| *id == item.id)
                {
                    cues.push((item.id, left));
                }
            }
        }
        if cues != self.cues_sent {
            self.cues_sent = cues.clone();
            notify(Update::Cues(cues));
        }
    }
}
