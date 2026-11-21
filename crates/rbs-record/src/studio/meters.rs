//! Mixer meters only while something reads them (F-15 of the 2026-11 resource audit;
//! spec `docs/specs/audio-graph.md`, "Mixer").
//!
//! The studio keeps one [`LevelMeter`] per audio source and one for the master, but
//! puts them in the mixer's parameters only while the host says a reader exists (the
//! Mixer is shown, or a control client subscribes to `audio.levels`). Without them the
//! mixer takes its add-only loops and records nothing (F-15a).

use super::*;

impl Studio {
    /// Puts meters on the mixer, or takes them off. A repeat of the current state
    /// sends nothing. Control thread only.
    ///
    /// Putting them back first clears what they kept from before they came off, in
    /// both banks: a peak or clip from then is not shown as new.
    pub fn set_meters_on(&mut self, on: bool) {
        if self.meters_on == on {
            return;
        }
        self.meters_on = on;
        if on {
            let master = self.mixer.as_ref().map(|m| &m.master);
            for meter in self.meters.values().chain(master) {
                let _ = (meter.take(), meter.observe());
            }
        }
        self.send_mix();
    }
}
