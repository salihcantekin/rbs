//! Buses and sends (ADR-0097 decision 9, ADR-0170): named mixes of a canvas besides
//! its master (`main`), and each source's send level to each of them.
//!
//! Bus volume and mute and send levels are live controls with records of their own,
//! like the canvas master (ADR-0081): every other canvas record keeps the current
//! buses and sends. Removing a bus removes every send to it in the same record;
//! removing a source removes its sends on every canvas in its record.

use std::collections::BTreeSet;

use super::{
    NextIds, Op, SceneCollection, SceneError, Undo, check_name, check_settings, check_type, issue,
};
use crate::model::{
    Bus, BusId, Canvas, CanvasId, FilterId, MAIN_BUS_NAME, MAX_BUSES, MasterAudio, Sends, SourceId,
};

/// One bus or send record.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BusOp {
    /// Puts a bus back at `index` with the sends it had.
    Insert {
        canvas: CanvasId,
        index: usize,
        bus: Bus,
        sends: Vec<(SourceId, f32)>,
    },
    Remove {
        canvas: CanvasId,
        bus: BusId,
    },
    /// Replaces a bus's name and filters; its `level` stays as it is (a live control).
    Replace {
        canvas: CanvasId,
        expected: Bus,
        new: Bus,
    },
    SetLevel {
        canvas: CanvasId,
        bus: BusId,
        expected: MasterAudio,
        new: MasterAudio,
    },
    SetSend {
        canvas: CanvasId,
        source: SourceId,
        bus: BusId,
        expected: f32,
        new: f32,
    },
}

impl BusOp {
    /// A live level edit whose new value is in range (`field.rs`).
    pub(super) fn is_field(&self) -> bool {
        match self {
            BusOp::SetLevel { new, .. } => new.is_valid(),
            BusOp::SetSend { new, .. } => Sends::level_is_valid(*new),
            _ => false,
        }
    }
}

/// Counters above every bus and bus filter id of a loaded canvas, whose bus names are
/// trimmed as edits trim them.
pub(super) fn counters_above(
    c: &mut Canvas,
    bus: u32,
    filter: u32,
) -> Result<(u32, u32), SceneError> {
    let above = |counter: u32, id: u32| -> Result<u32, SceneError> {
        Ok(counter.max(id.checked_add(1).ok_or(SceneError::IdsExhausted)?))
    };
    let (mut bus, mut filter) = (bus, filter);
    for b in &mut c.buses {
        b.name = b.name.trim().to_owned();
        bus = above(bus, b.id.0)?;
        for f in &b.filters {
            filter = above(filter, f.id.0)?;
        }
    }
    Ok((bus, filter))
}

/// A canvas replacement keeps the current buses and sends (live controls).
pub(super) fn keep_live(slot: &Canvas, expected: &mut Canvas, new: &mut Canvas) {
    expected.buses.clone_from(&slot.buses);
    expected.sends.clone_from(&slot.sends);
    new.buses.clone_from(&slot.buses);
    new.sends.clone_from(&slot.sends);
}

impl SceneCollection {
    // ----- queries -----

    /// The canvas's buses other than `main`, in Mixer order.
    pub fn buses(&self, canvas: CanvasId) -> Result<&[Bus], SceneError> {
        Ok(&self.get_canvas(canvas)?.buses)
    }

    /// The bus called `name` on the canvas; [`MAIN_BUS_NAME`] is [`BusId::MAIN`].
    /// Outputs store names (ADR-0097 decision 11) and resolve them here.
    pub fn bus_by_name(&self, canvas: CanvasId, name: &str) -> Option<BusId> {
        if name == MAIN_BUS_NAME {
            return self.canvases.contains_key(&canvas).then_some(BusId::MAIN);
        }
        let c = self.canvases.get(&canvas)?;
        c.buses.iter().find(|b| b.name == name).map(|b| b.id)
    }

    /// The name of a bus of the canvas.
    pub fn bus_name(&self, canvas: CanvasId, bus: BusId) -> Option<&str> {
        if bus == BusId::MAIN {
            return self.canvases.contains_key(&canvas).then_some(MAIN_BUS_NAME);
        }
        let c = self.canvases.get(&canvas)?;
        c.buses
            .iter()
            .find(|b| b.id == bus)
            .map(|b| b.name.as_str())
    }

    /// A source's send levels on the canvas (the default sends when it has no entry).
    pub fn sends(&self, canvas: CanvasId, source: SourceId) -> Sends {
        self.canvases
            .get(&canvas)
            .and_then(|c| c.sends.get(&source))
            .cloned()
            .unwrap_or_default()
    }

    // ----- edits -----

    /// Adds a bus at the end of the canvas's buses, at full volume, with no sends and
    /// no filters. `DuplicateName` when the canvas has a bus of that name or it is
    /// [`MAIN_BUS_NAME`]; `TooManyBuses` past [`MAX_BUSES`].
    pub fn add_bus(&mut self, canvas: CanvasId, name: &str) -> Result<(BusId, Undo), SceneError> {
        let index = self.get_canvas(canvas)?.buses.len();
        let mut next: NextIds = self.next.clone();
        next.bus = next.bus.max(1);
        let id = BusId(issue(&mut next.bus)?);
        let bus = Bus::new(id, name.trim());
        let op = BusOp::Insert {
            canvas,
            index,
            bus,
            sends: Vec::new(),
        };
        let undo = self.apply_with(Op::Bus(op), next)?;
        Ok((id, undo))
    }

    /// Removes a bus with its filters and every send to it, in one record. Whether an
    /// output still uses it is the host's check (ADR-0097 decision 12).
    pub fn remove_bus(&mut self, canvas: CanvasId, bus: BusId) -> Result<Undo, SceneError> {
        self.apply(Op::Bus(BusOp::Remove { canvas, bus }))
    }

    /// Renames a bus. The host updates the loaded profile's references in the same
    /// step (ADR-0097 decision 12).
    pub fn rename_bus(
        &mut self,
        canvas: CanvasId,
        bus: BusId,
        name: &str,
    ) -> Result<Undo, SceneError> {
        let expected = self.bus(canvas, bus)?.clone();
        let new = Bus {
            name: name.trim().to_owned(),
            ..expected.clone()
        };
        self.apply(Op::Bus(BusOp::Replace {
            canvas,
            expected,
            new,
        }))
    }

    /// Sets a bus's volume, a linear factor in `0.0..=1.0`.
    pub fn set_bus_volume(
        &mut self,
        canvas: CanvasId,
        bus: BusId,
        volume: f32,
    ) -> Result<Undo, SceneError> {
        let expected = self.bus(canvas, bus)?.level;
        self.set_bus_level(canvas, bus, expected, MasterAudio { volume, ..expected })
    }

    /// Mutes or unmutes a bus; the volume is kept.
    pub fn set_bus_muted(
        &mut self,
        canvas: CanvasId,
        bus: BusId,
        muted: bool,
    ) -> Result<Undo, SceneError> {
        let expected = self.bus(canvas, bus)?.level;
        self.set_bus_level(canvas, bus, expected, MasterAudio { muted, ..expected })
    }

    fn set_bus_level(
        &mut self,
        canvas: CanvasId,
        bus: BusId,
        expected: MasterAudio,
        new: MasterAudio,
    ) -> Result<Undo, SceneError> {
        if !new.is_valid() {
            return Err(SceneError::InvalidVolume);
        }
        self.apply(Op::Bus(BusOp::SetLevel {
            canvas,
            bus,
            expected,
            new,
        }))
    }

    /// Sets a source's send level to a bus of the canvas, `main` included: a linear
    /// factor in `0.0..=1.0`, 0 meaning no send.
    pub fn set_send(
        &mut self,
        canvas: CanvasId,
        source: SourceId,
        bus: BusId,
        level: f32,
    ) -> Result<Undo, SceneError> {
        if !Sends::level_is_valid(level) {
            return Err(SceneError::InvalidVolume);
        }
        let expected = self.sends(canvas, source).level(bus);
        self.apply(Op::Bus(BusOp::SetSend {
            canvas,
            source,
            bus,
            expected,
            new: level,
        }))
    }

    fn bus(&self, canvas: CanvasId, bus: BusId) -> Result<&Bus, SceneError> {
        self.get_canvas(canvas)?
            .buses
            .iter()
            .find(|b| b.id == bus)
            .ok_or(SceneError::UnknownBus(bus))
    }

    // ----- records -----

    pub(super) fn perform_bus(&mut self, op: BusOp) -> Result<BusOp, SceneError> {
        match op {
            BusOp::Insert {
                canvas,
                index,
                bus,
                sends,
            } => {
                let (next_bus, next_filter) = (self.next.bus, self.next.filter);
                if bus.id == BusId::MAIN || bus.filters.iter().any(|f| f.id.0 >= next_filter) {
                    return Err(SceneError::UndoConflict);
                }
                let c = self.canvas_mut(canvas)?;
                let taken = c.buses.iter().any(|b| b.id == bus.id);
                Self::restorable(next_bus, bus.id.0, taken)?;
                if index > c.buses.len() {
                    return Err(SceneError::IndexOutOfRange(index));
                }
                let id = bus.id;
                c.buses.insert(index, bus);
                for (source, level) in sends {
                    c.sends.entry(source).or_default().set(id, level);
                }
                Ok(BusOp::Remove { canvas, bus: id })
            }
            BusOp::Remove { canvas, bus } => {
                let c = self.canvas_mut(canvas)?;
                let index = c
                    .buses
                    .iter()
                    .position(|b| b.id == bus)
                    .ok_or(SceneError::UnknownBus(bus))?;
                let removed = c.buses.remove(index);
                let mut sends = Vec::new();
                for (source, s) in &mut c.sends {
                    if let Some(level) = s.remove(bus) {
                        sends.push((*source, level));
                    }
                }
                c.sends.retain(|_, s| !s.is_default());
                Ok(BusOp::Insert {
                    canvas,
                    index,
                    bus: removed,
                    sends,
                })
            }
            BusOp::Replace {
                canvas,
                mut expected,
                mut new,
            } => {
                let c = self.canvas_mut(canvas)?;
                let slot = c
                    .buses
                    .iter_mut()
                    .find(|b| b.id == new.id)
                    .ok_or(SceneError::UnknownBus(new.id))?;
                expected.level = slot.level;
                new.level = slot.level;
                if *slot != expected || expected.id != new.id {
                    return Err(SceneError::UndoConflict);
                }
                *slot = new.clone();
                Ok(BusOp::Replace {
                    canvas,
                    expected: new,
                    new: expected,
                })
            }
            BusOp::SetLevel {
                canvas,
                bus,
                expected,
                new,
            } => {
                let c = self.canvas_mut(canvas)?;
                let slot = c
                    .buses
                    .iter_mut()
                    .find(|b| b.id == bus)
                    .ok_or(SceneError::UnknownBus(bus))?;
                if slot.level != expected {
                    return Err(SceneError::UndoConflict);
                }
                slot.level = new;
                Ok(BusOp::SetLevel {
                    canvas,
                    bus,
                    expected: new,
                    new: expected,
                })
            }
            BusOp::SetSend {
                canvas,
                source,
                bus,
                expected,
                new,
            } => {
                self.get_source(source)?;
                let c = self.canvas_mut(canvas)?;
                if bus != BusId::MAIN && !c.buses.iter().any(|b| b.id == bus) {
                    return Err(SceneError::UnknownBus(bus));
                }
                let sends = c.sends.entry(source).or_default();
                if sends.level(bus) != expected {
                    if sends.is_default() {
                        c.sends.remove(&source);
                    }
                    return Err(SceneError::UndoConflict);
                }
                sends.set(bus, new);
                if sends.is_default() {
                    c.sends.remove(&source);
                }
                Ok(BusOp::SetSend {
                    canvas,
                    source,
                    bus,
                    expected: new,
                    new: expected,
                })
            }
        }
    }

    /// The canvas master is the `main` bus's level (ADR-0081).
    pub(super) fn perform_master(
        &mut self,
        canvas: CanvasId,
        expected: MasterAudio,
        new: MasterAudio,
    ) -> Result<Op, SceneError> {
        let slot = self.canvas_mut(canvas)?;
        if slot.master != expected {
            return Err(SceneError::UndoConflict);
        }
        slot.master = new;
        Ok(Op::SetCanvasMaster {
            canvas,
            expected: new,
            new: expected,
        })
    }

    fn canvas_mut(&mut self, canvas: CanvasId) -> Result<&mut Canvas, SceneError> {
        self.canvases
            .get_mut(&canvas)
            .ok_or(SceneError::UnknownCanvas(canvas))
    }

    /// Removes a deleted source's sends from every canvas, for its undo record.
    pub(super) fn take_sends(&mut self, source: SourceId) -> Vec<(CanvasId, Sends)> {
        self.canvases
            .values_mut()
            .filter_map(|c| c.sends.remove(&source).map(|s| (c.id, s)))
            .collect()
    }

    /// Puts a restored source's sends back on the canvases that still exist, to the
    /// buses that still exist.
    pub(super) fn restore_sends(&mut self, source: SourceId, sends: Vec<(CanvasId, Sends)>) {
        for (canvas, s) in sends {
            if let Some(c) = self.canvases.get_mut(&canvas) {
                let mut kept = Sends::default();
                for (bus, level) in s.stored() {
                    if bus == BusId::MAIN || c.buses.iter().any(|b| b.id == bus) {
                        kept.set(bus, level);
                    }
                }
                if !kept.is_default() {
                    c.sends.insert(source, kept);
                }
            }
        }
    }

    /// The bus invariants (scene model spec): at most [`MAX_BUSES`] with `main`, ids
    /// issued and unique, names unique and not `main`, levels in range, filter ids
    /// unique across the collection, sends only from known sources to known buses.
    pub(super) fn validate_buses(
        &self,
        filters: &mut BTreeSet<FilterId>,
    ) -> Result<(), SceneError> {
        let mut all = BTreeSet::new();
        for c in self.canvases.values() {
            if c.buses.len() >= MAX_BUSES {
                return Err(SceneError::TooManyBuses);
            }
            let mut names = BTreeSet::from([MAIN_BUS_NAME]);
            let mut ids = BTreeSet::new();
            for b in &c.buses {
                if b.id == BusId::MAIN {
                    return Err(SceneError::DuplicateName(MAIN_BUS_NAME.to_owned()));
                }
                if b.id.0 >= self.next.bus {
                    return Err(SceneError::IdsExhausted);
                }
                // Bus ids are unique in the whole collection, not only on the canvas.
                if !all.insert(b.id) {
                    return Err(SceneError::DuplicateId(b.id.to_string()));
                }
                ids.insert(b.id);
                check_name(&b.name)?;
                if !names.insert(b.name.as_str()) {
                    return Err(SceneError::DuplicateName(b.name.clone()));
                }
                if !b.level.is_valid() {
                    return Err(SceneError::InvalidVolume);
                }
                for f in &b.filters {
                    if !filters.insert(f.id) {
                        return Err(SceneError::DuplicateFilter(f.id));
                    }
                    check_type(&f.type_id)?;
                    check_settings(&f.settings)?;
                }
            }
            for (source, sends) in &c.sends {
                self.get_source(*source)?;
                for (bus, level) in sends.stored() {
                    if bus != BusId::MAIN && !ids.contains(&bus) {
                        return Err(SceneError::UnknownBus(bus));
                    }
                    if !Sends::level_is_valid(level) {
                        return Err(SceneError::InvalidVolume);
                    }
                }
            }
        }
        Ok(())
    }
}
