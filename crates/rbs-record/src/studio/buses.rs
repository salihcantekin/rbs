//! The buses of the main canvas's mixer (ADR-0097 F2, ADR-0170): which mixer bus pin
//! carries each bus, its fader and meter, each source's sends, and the mixer
//! parameters built from them. Control path only: the mixer gets them as parameters.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rbs_audio::{BUS_PINS, LevelMeter, MAX_BUS_OUTPUTS, MixerParams, MonitorMode, Strip};
use rbs_scene::SceneCollection;
use rbs_scene::model::{
    BusId, CanvasId, MAIN_BUS_NAME, MAX_BUSES, MasterAudio, Sends, SourceId, SourceMonitor,
};

use super::MixStrip;
use crate::recording::RecordError;

// Every bus but `main` has a mixer pin of its own.
const _: () = assert!(MAX_BUSES - 1 == MAX_BUS_OUTPUTS);

/// The mixer's `main` pin.
pub(super) const MAIN_PIN: &str = "main";

/// The buses the mixer carries, by bus pin slot.
pub(super) struct MixBuses {
    /// The canvas's master volume and mute (ADR-0081); applies to `main` only.
    pub(super) master: MasterAudio,
    /// The bus on each slot. A slot keeps its bus while the bus exists, so the pin a
    /// running output reads never changes bus when another bus is added or removed.
    /// A removed bus's slot is given to another bus only once no chain reads it.
    slots: [Option<BusId>; MAX_BUS_OUTPUTS],
    /// The slot's bus name, `None` once the bus left the canvas.
    names: [Option<String>; MAX_BUS_OUTPUTS],
    /// The slot's bus fader (muted once the bus left the canvas).
    levels: [MasterAudio; MAX_BUS_OUTPUTS],
    /// The slot's meter, given to the mixer while meters are on.
    meters: [Arc<LevelMeter>; MAX_BUS_OUTPUTS],
    /// Each source's stored sends (absent: the default sends).
    sends: BTreeMap<SourceId, Sends>,
}

impl MixBuses {
    /// No buses, meters of `channels` channels.
    pub(super) fn new(channels: usize) -> Self {
        Self {
            master: MasterAudio::default(),
            slots: [None; MAX_BUS_OUTPUTS],
            names: Default::default(),
            levels: [MasterAudio::default(); MAX_BUS_OUTPUTS],
            meters: std::array::from_fn(|_| Arc::new(LevelMeter::new(channels))),
            sends: BTreeMap::new(),
        }
    }

    /// The master, buses and sends of `canvas`, meters of `channels`.
    pub(super) fn of(
        collection: &SceneCollection,
        canvas: CanvasId,
        channels: rbs_core::media::ChannelLayout,
    ) -> Self {
        let mut b = Self::new(channels.channels());
        b.refresh(collection, canvas, |_| false);
        b
    }

    /// Takes the master, buses and sends of `canvas` from `collection`. `read(slot)` is true
    /// while a running chain reads the slot's pin; such a slot keeps its bus even when
    /// the bus is gone (it is then silent).
    pub(super) fn refresh(
        &mut self,
        collection: &SceneCollection,
        canvas: CanvasId,
        read: impl Fn(usize) -> bool,
    ) {
        self.master = master_of(collection, canvas);
        let buses = collection.buses(canvas).unwrap_or(&[]);
        for slot in 0..MAX_BUS_OUTPUTS {
            let Some(id) = self.slots[slot] else { continue };
            match buses.iter().find(|b| b.id == id) {
                Some(bus) => {
                    self.names[slot] = Some(bus.name.clone());
                    self.levels[slot] = bus.level;
                }
                None => {
                    self.names[slot] = None;
                    self.levels[slot] = MasterAudio::new(0.0, true);
                    if !read(slot) {
                        self.slots[slot] = None;
                    }
                }
            }
        }
        for bus in buses {
            if self.slots.contains(&Some(bus.id)) {
                continue;
            }
            // A bus with no free slot (every slot held by a gone bus a chain still
            // reads) has no pin until one frees; an output naming it is refused.
            if let Some(slot) = self.slots.iter().position(Option::is_none) {
                self.slots[slot] = Some(bus.id);
                self.names[slot] = Some(bus.name.clone());
                self.levels[slot] = bus.level;
            }
        }
        self.sends = collection
            .canvas(canvas)
            .map(|c| c.sends.clone())
            .unwrap_or_default();
    }

    /// The slot of the bus an output names: `None` for `main` (or no name).
    pub(super) fn slot_of(&self, name: Option<&str>) -> Result<Option<usize>, RecordError> {
        match name {
            None | Some(MAIN_BUS_NAME) => Ok(None),
            Some(name) => self
                .names
                .iter()
                .position(|n| n.as_deref() == Some(name))
                .map(Some)
                .ok_or_else(|| RecordError::UnknownBus(name.to_owned())),
        }
    }

    /// The meter of the bus called `name` (not `main`).
    pub(super) fn meter(&self, name: &str) -> Option<&Arc<LevelMeter>> {
        let slot = self.slot_of(Some(name)).ok()??;
        Some(&self.meters[slot])
    }

    /// `strip` with `source`'s sends.
    fn sends_of(&self, strip: Strip, source: SourceId) -> Strip {
        let Some(sends) = self.sends.get(&source) else {
            return strip;
        };
        let mut strip = strip.with_main_send(sends.level(BusId::MAIN));
        for (slot, bus) in self.slots.iter().enumerate() {
            if let Some(bus) = bus {
                strip = strip.with_send(slot, sends.level(*bus));
            }
        }
        strip
    }

    /// `params` with every carried bus's fader, and its meter while `meters_on`.
    fn outputs(&self, mut params: MixerParams, meters_on: bool) -> MixerParams {
        for (slot, bus) in self.slots.iter().enumerate() {
            if bus.is_some() {
                let meter = meters_on.then(|| Arc::clone(&self.meters[slot]));
                params = params.with_bus(slot, self.levels[slot].gain(), meter);
            }
        }
        params
    }
}

impl super::Studio {
    /// Takes the main canvas's buses and sends from `collection`; a slot a running
    /// audio chain reads keeps its bus.
    pub(super) fn refresh_buses(&mut self, collection: &SceneCollection) {
        let chains = &self.chains.audio;
        self.buses
            .refresh(collection, self.settings.canvas, |slot| {
                chains.iter().flatten().any(|c| c.bus == Some(slot))
            });
    }

    /// The meter of the bus called `name` on the main canvas (not `main`: that is
    /// [`Studio::master_meter`](super::Studio::master_meter)). It reads levels only
    /// while an output takes the bus and meters are on.
    pub fn bus_meter(&self, name: &str) -> Option<&Arc<LevelMeter>> {
        self.mixer.as_ref()?;
        self.buses.meter(name)
    }
}

/// The mixer pin of a bus slot (`None`: `main`).
pub(super) fn pin(slot: Option<usize>) -> &'static str {
    slot.and_then(|s| BUS_PINS.get(s).copied())
        .unwrap_or(MAIN_PIN)
}

/// The mixer's monitor mode for a source's saved one (ADR-0097 decision 5).
fn monitor_mode(m: SourceMonitor) -> MonitorMode {
    match m {
        SourceMonitor::Off => MonitorMode::Off,
        SourceMonitor::Only => MonitorMode::Only,
        SourceMonitor::Also => MonitorMode::Also,
    }
}

/// The canvas's master volume and mute (the defaults for a canvas that is gone).
pub(super) fn master_of(collection: &SceneCollection, canvas: CanvasId) -> MasterAudio {
    collection
        .canvas(canvas)
        .map(|c| c.master)
        .unwrap_or_default()
}

/// The mixer's parameters for `strips` (each at its source's volume, mute and sends),
/// the master gain (its volume, or silence while muted), the bus faders and the
/// meters.
pub(super) fn mixer_params(
    strips: &[MixStrip],
    meter: &Arc<LevelMeter>,
    buses: &MixBuses,
    solo: &BTreeSet<SourceId>,
    meters_on: bool,
) -> MixerParams {
    let strips = strips
        .iter()
        .map(|s| {
            let strip = Strip::new(s.producer, s.audio.volume)
                .with_muted(s.audio.muted)
                .with_monitor(monitor_mode(s.audio.monitor))
                .with_solo(solo.contains(&s.source))
                .with_ramp_override(s.ramp);
            let strip = buses.sends_of(strip, s.source);
            match &s.meter {
                Some(m) if meters_on => strip.with_meter(Arc::clone(m)),
                _ => strip,
            }
        })
        .collect();
    let params = buses.outputs(
        MixerParams::new(strips).with_master(buses.master.gain()),
        meters_on,
    );
    if meters_on {
        params.with_master_meter(Arc::clone(meter))
    } else {
        params
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rbs_core::media::Size;
    use rbs_core::timing::FrameRate;
    use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};

    fn any_node() -> rbs_graph::description::NodeId {
        let mut g = rbs_graph::description::GraphDescription::new();
        let d = NodeDescriptor::new(
            NodeTypeId::new_static("test.node"),
            NodeKind::Source,
            vec![],
            vec![Pin::one("audio", LinkKind::Audio)],
        );
        g.add_node(Arc::new(d)).unwrap()
    }

    fn canvas() -> (SceneCollection, CanvasId) {
        let mut col = SceneCollection::new();
        let (canvas, _) = col
            .add_canvas("Main", Size::new(64, 36), FrameRate::FPS_30)
            .unwrap();
        (col, canvas)
    }

    #[test]
    fn a_slot_keeps_its_bus_while_another_bus_leaves() {
        let (mut col, canvas) = canvas();
        col.add_bus(canvas, "Music").unwrap();
        col.add_bus(canvas, "Voice").unwrap();
        let mut b = MixBuses::new(2);
        b.refresh(&col, canvas, |_| false);
        assert_eq!(b.slot_of(Some("Music")).unwrap(), Some(0));
        assert_eq!(b.slot_of(Some("Voice")).unwrap(), Some(1));
        assert_eq!(b.slot_of(None).unwrap(), None);
        assert_eq!(b.slot_of(Some("main")).unwrap(), None);
        let music = col.bus_by_name(canvas, "Music").unwrap();
        col.remove_bus(canvas, music).unwrap();
        b.refresh(&col, canvas, |_| false);
        assert_eq!(b.slot_of(Some("Voice")).unwrap(), Some(1), "no shift");
        assert!(matches!(
            b.slot_of(Some("Music")),
            Err(RecordError::UnknownBus(n)) if n == "Music"
        ));
        assert_eq!(pin(Some(1)), "bus2");
        assert_eq!(pin(None), "main");
    }

    #[test]
    fn a_read_slot_of_a_gone_bus_stays_silent_and_is_not_given_away() {
        let (mut col, canvas) = canvas();
        col.add_bus(canvas, "Music").unwrap();
        let mut b = MixBuses::new(2);
        b.refresh(&col, canvas, |_| false);
        let music = col.bus_by_name(canvas, "Music").unwrap();
        col.remove_bus(canvas, music).unwrap();
        col.add_bus(canvas, "Voice").unwrap();
        b.refresh(&col, canvas, |slot| slot == 0);
        assert_eq!(b.slot_of(Some("Voice")).unwrap(), Some(1));
        assert_eq!(b.levels[0].gain(), 0.0);
        // Once nothing reads it, the slot is free for the next bus.
        b.refresh(&col, canvas, |_| false);
        col.add_bus(canvas, "Chat").unwrap();
        b.refresh(&col, canvas, |_| false);
        assert_eq!(b.slot_of(Some("Chat")).unwrap(), Some(0));
    }

    #[test]
    fn sends_and_bus_faders_reach_the_mixer_parameters() {
        let (mut col, canvas) = canvas();
        let (mic, _) = col
            .add_source(
                "Mic",
                NodeTypeId::new_static(rbs_sources::types::AUDIO_INPUT_SOURCE),
                rbs_scene::model::Settings::new(),
            )
            .unwrap();
        let (music, _) = col.add_bus(canvas, "Music").unwrap();
        col.set_send(canvas, mic, music, 0.5).unwrap();
        col.set_send(canvas, mic, BusId::MAIN, 0.0).unwrap();
        col.set_bus_volume(canvas, music, 0.25).unwrap();
        let mut b = MixBuses::new(2);
        b.refresh(&col, canvas, |_| false);
        let strip = b.sends_of(Strip::new(any_node(), 1.0), mic);
        assert_eq!(strip.main_send, 0.0);
        assert_eq!(strip.sends[0], 0.5);
        let params = b.outputs(MixerParams::new(Vec::new()), false);
        assert_eq!(params.buses[0].gain, 0.25);
        assert!(params.buses[0].meter.is_none());
        let params = b.outputs(MixerParams::new(Vec::new()), true);
        assert!(params.buses[0].meter.is_some());
        assert!(b.meter("Music").is_some() && b.meter("Chat").is_none());
    }
}
