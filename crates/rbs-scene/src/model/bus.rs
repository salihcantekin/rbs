//! Buses and sends (ADR-0097 decision 9, ADR-0170 decision 1): named mixes of the main
//! canvas that outputs pick by name.

use std::collections::BTreeMap;

use super::{BusId, Filter, MAX_SOURCE_VOLUME, MasterAudio};

/// Most buses of one canvas, `main` included (ADR-0170 decision 1).
pub const MAX_BUSES: usize = 6;

/// The name of [`BusId::MAIN`], the canvas's master. No other bus may take it.
pub const MAIN_BUS_NAME: &str = "main";

impl BusId {
    /// The canvas's master mix. Always exists and is never stored as a [`Bus`].
    pub const MAIN: BusId = BusId(0);
}

/// A bus other than `main`: its own fader, mute and audio filters.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct Bus {
    /// Its id, never [`BusId::MAIN`].
    pub id: BusId,
    /// Unique among the canvas's buses, not [`MAIN_BUS_NAME`]; outputs store it.
    pub name: String,
    /// Volume and mute, the last stage of the bus's mix (like a canvas master).
    pub level: MasterAudio,
    /// Audio filters after the bus's sum, in order (ADR-0082 `Filter` model).
    pub filters: Vec<Filter>,
}

impl Bus {
    /// A bus at full volume with no filters, for documents.
    pub fn new(id: BusId, name: &str) -> Self {
        Self {
            id,
            name: name.to_owned(),
            level: MasterAudio::default(),
            filters: Vec::new(),
        }
    }
}

/// One source's send levels (ADR-0097 decision 9): linear gain per bus, post-fader.
/// Only levels that differ from the default are stored: `main` 1.0, any other bus 0.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sends(BTreeMap<BusId, f32>);

impl Sends {
    /// The default level of a send to `bus`.
    pub fn default_level(bus: BusId) -> f32 {
        if bus == BusId::MAIN {
            MAX_SOURCE_VOLUME
        } else {
            0.0
        }
    }

    /// The level of the send to `bus`.
    pub fn level(&self, bus: BusId) -> f32 {
        self.0
            .get(&bus)
            .copied()
            .unwrap_or_else(|| Self::default_level(bus))
    }

    /// Sets the level of the send to `bus`; the default level removes the entry.
    pub fn set(&mut self, bus: BusId, level: f32) {
        if level == Self::default_level(bus) {
            self.0.remove(&bus);
        } else {
            self.0.insert(bus, level);
        }
    }

    /// Removes the send to `bus` (back to its default), returning the stored level.
    pub fn remove(&mut self, bus: BusId) -> Option<f32> {
        self.0.remove(&bus)
    }

    /// True when every send has its default level.
    pub fn is_default(&self) -> bool {
        self.0.is_empty()
    }

    /// The stored (non-default) levels, by bus.
    pub fn stored(&self) -> impl Iterator<Item = (BusId, f32)> + '_ {
        self.0.iter().map(|(b, l)| (*b, *l))
    }

    /// True if a level is finite and in `0.0..=`[`MAX_SOURCE_VOLUME`].
    pub fn level_is_valid(level: f32) -> bool {
        level.is_finite() && (0.0..=MAX_SOURCE_VOLUME).contains(&level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_levels_are_not_stored() {
        let mut s = Sends::default();
        let music = BusId::from_raw(1);
        assert_eq!(s.level(BusId::MAIN), 1.0);
        assert_eq!(s.level(music), 0.0);
        s.set(music, 0.5);
        s.set(BusId::MAIN, 0.0);
        assert_eq!(s.level(music), 0.5);
        assert_eq!(s.level(BusId::MAIN), 0.0);
        s.set(music, 0.0);
        s.set(BusId::MAIN, 1.0);
        assert!(s.is_default());
    }
}
