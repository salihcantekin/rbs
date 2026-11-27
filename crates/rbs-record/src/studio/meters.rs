//! Mixer meters only while something reads them (F-15 of the 2026-11 resource audit;
//! spec `docs/specs/audio-graph.md`, "Mixer").
//!
//! The studio keeps one [`LevelMeter`] per audio source and one for the master, but
//! puts them in the mixer's parameters only while the host says a reader exists (the
//! Mixer is shown, or a control client subscribes to `audio.levels`). Without them the
//! mixer takes its add-only loops and records nothing (F-15a).

use rbs_engine::EngineError;

use super::*;

pub(super) fn for_source(
    meters: &mut BTreeMap<SourceId, Arc<LevelMeter>>,
    source: SourceId,
    graph: &AudioGraphConfig,
) -> Arc<LevelMeter> {
    Arc::clone(
        meters
            .entry(source)
            .or_insert_with(|| Arc::new(LevelMeter::new(graph.format.channels.channels()))),
    )
}

pub(super) fn ensure_sound_meters(studio: &mut Studio, scheduled: &BTreeSet<NodeId>) {
    for entry in &studio.entries {
        if studio
            .binder
            .sound_node(entry.source)
            .is_some_and(|node| scheduled.contains(&node))
        {
            let _meter = for_source(
                &mut studio.meters,
                entry.source,
                &studio.settings.audio_graph,
            );
        }
    }
}

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

/// The mixer's meter sinks: `main`'s for the studio's life, and one on each bus pin
/// with a send (ADR-0170 decision 2), by bus slot.
#[derive(Debug, Clone, Copy)]
pub(super) struct Sinks {
    pub(super) main: NodeId,
    buses: [Option<NodeId>; rbs_audio::MAX_BUS_OUTPUTS],
}

impl Sinks {
    pub(super) fn new(main: NodeId) -> Self {
        Self {
            main,
            buses: [None; rbs_audio::MAX_BUS_OUTPUTS],
        }
    }
}

/// The meter sinks (`main` and every bus's) while the branch rests: left out of the
/// graph the engine gets. Empty while awake, so an awake apply allocates nothing.
pub(super) fn resting_sinks(studio: &Studio) -> Vec<NodeId> {
    match &studio.mixer {
        Some(m) if !studio.audio_awake => std::iter::once(m.sinks.main)
            .chain(m.sinks.buses.iter().flatten().copied())
            .collect(),
        _ => Vec::new(),
    }
}

impl Studio {
    /// Gives each bus pin with a send a meter sink and takes it from a bus that lost
    /// its last send or left the canvas (ADR-0170 decision 2), in the studio's graph;
    /// the next apply sends it. True when a sink came or went.
    pub(super) fn sync_bus_sinks(&mut self) -> Result<bool, RecordError> {
        let Some(mixer) = &self.mixer else {
            return Ok(false);
        };
        let (node, mut sinks) = (mixer.node, mixer.sinks.buses);
        let want = self.buses.metered();
        let partition = self.settings.partitions.audio;
        let mut changed = false;
        for (slot, wanted) in want.into_iter().enumerate() {
            match (sinks[slot], wanted) {
                (None, true) => {
                    let sink = MeterSinkNode::new();
                    let id = add_active(&mut self.graph, Node::<()>::descriptor(&sink), partition)?;
                    self.graph.connect(node, buses::pin(Some(slot)), id, "in")?;
                    self.engine.adopt::<()>(partition, id, Box::new(sink))?;
                    sinks[slot] = Some(id);
                    changed = true;
                }
                (Some(id), false) => {
                    let _ = self.graph.set_state(id, Lifecycle::Standby);
                    let _ = self.graph.remove_node(id);
                    // Taken back once the apply that leaves it out has run.
                    let leaving = Added {
                        node: id,
                        partition,
                        gpu: false,
                    };
                    self.pending_takes.push((leaving, None));
                    sinks[slot] = None;
                    changed = true;
                }
                _ => {}
            }
        }
        if let Some(m) = &mut self.mixer {
            m.sinks.buses = sinks;
        }
        Ok(changed)
    }

    /// What `reconcile` brings in line before its apply: the bus meter sinks and, after
    /// a sync, the preview scene's, hold scenes' and incoming scene's views.
    pub(super) fn sync_before_apply(
        &mut self,
        report: Option<&BindReport>,
        leaving: &mut Vec<Added>,
    ) -> Result<(), RecordError> {
        self.sync_bus_sinks()?;
        if let Some(report) = report {
            self.sync_scene_preview(report, leaving)?;
            self.sync_hold_views(report, leaving)?;
            self.sync_incoming(report, leaving)?;
        }
        Ok(())
    }

    /// How many buses have a meter sink now (ADR-0170 decision 2).
    pub fn bus_meter_sinks(&self) -> usize {
        self.mixer
            .as_ref()
            .map_or(0, |m| m.sinks.buses.iter().flatten().count())
    }

    /// [`Studio::sync_bus_sinks`] for a live send or bus edit ([`Studio::set_mix`]):
    /// a sink that came or went is applied at once. Control path; a failure is only
    /// logged, as the meter is not essential.
    pub(super) fn apply_bus_sinks(&mut self) {
        let result = match self.sync_bus_sinks() {
            Ok(true) => self
                .send_graph(None, false)
                .map(drop)
                .map_err(RecordError::from),
            Ok(false) => Ok(()),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            rbs_core::warn!(rbs_core::log::Area::Audio, "bus meter: {e}");
        }
    }
}

impl Studio {
    /// Wakes the audio branch, or lets it rest (ADR-0169 decision 2): the meter sink
    /// joins or leaves the engine's graph, which a plain apply sends. With no other
    /// reader of the mixer the plan then leaves out the mixer, its inputs and their
    /// filters, and the audio worker stops; their instances stay. The studio's own
    /// graph keeps the sink, so sources added while resting are opened and reported as
    /// usual. A repeat of the current state applies nothing. Returns the apply's
    /// failures (a device that does not start is also recorded on its source).
    pub fn set_audio_awake(&mut self, awake: bool) -> Vec<StartFailure> {
        if self.audio_awake == awake || self.mixer.is_none() {
            self.audio_awake = awake;
            return Vec::new();
        }
        self.audio_awake = awake;
        match self.send_graph(None, false) {
            Ok(applied) => engine_failures(&applied),
            Err(e) => vec![StartFailure {
                node: "engine".to_owned(),
                reason: e.to_string(),
            }],
        }
    }

    /// True while the meter sink is in the engine's graph.
    pub fn audio_awake(&self) -> bool {
        self.audio_awake
    }

    /// Sends `graph` (the studio's own when `None`) to the engine, without the meter
    /// sink while resting, and records the sources and pictures that failed to join
    /// (ADR-0169 decision 1). Every apply of the studio goes through here or, for
    /// `reconcile`, which records failures itself, through [`Studio::send_raw`].
    pub(super) fn send_graph(
        &mut self,
        graph: Option<&GraphDescription>,
        drain: bool,
    ) -> Result<Applied, EngineError> {
        let applied = self.send_raw(graph, drain)?;
        self.note_failed_sources(&applied);
        Ok(applied)
    }

    /// [`Studio::send_graph`] without recording failures.
    pub(super) fn send_raw(
        &mut self,
        graph: Option<&GraphDescription>,
        drain: bool,
    ) -> Result<Applied, EngineError> {
        let rest = resting_sinks(self);
        send_to(&mut self.engine, graph.unwrap_or(&self.graph), &rest, drain)
    }

    /// What `reconcile`'s apply failed: essential and output nodes as failures, a
    /// picture or source on its entry (the first reason stays).
    pub(super) fn record_failures(&mut self, applied: &Applied, failed: &mut Vec<StartFailure>) {
        failed.extend(engine_failures(applied));
        for report in applied.reports.values() {
            for (node, why) in &report.failed {
                let role = self
                    .essential
                    .get(node)
                    .copied()
                    .or_else(|| self.output_role(*node));
                if let Some(role) = role {
                    let failure = StartFailure {
                        node: role.to_owned(),
                        reason: why.to_string(),
                    };
                    self.failed_nodes.insert(*node, failure.clone());
                    failed.push(failure);
                } else if let Some(p) = self
                    .pictures
                    .iter_mut()
                    .find(|p| p.node == *node || p.normalise == Some(*node))
                {
                    // Not essential (ADR-0125 decision 2): only that picture stops.
                    p.failure = Some(why.to_string());
                } else if let Some(e) = self.entries.iter_mut().find(|e| e.state.node == *node) {
                    // The first reason stays: a source without an instance is reported
                    // again on every apply as having none.
                    if e.state.failed.is_none() {
                        e.state.failed = Some(why.to_string());
                        failed.push(StartFailure {
                            node: "source".to_owned(),
                            reason: format!("{}: {why}", e.state.name),
                        });
                    }
                }
            }
        }
    }

    /// A source or picture whose node failed to join keeps the first reason, as
    /// `reconcile` records it.
    fn note_failed_sources(&mut self, applied: &Applied) {
        for report in applied.reports.values() {
            for (node, why) in &report.failed {
                if let Some(p) = self
                    .pictures
                    .iter_mut()
                    .find(|p| p.node == *node || p.normalise == Some(*node))
                {
                    p.failure = Some(why.to_string());
                } else if let Some(e) = self.entries.iter_mut().find(|e| e.state.node == *node)
                    && e.state.failed.is_none()
                {
                    e.state.failed = Some(why.to_string());
                }
            }
        }
    }
}

/// Applies `graph` without `rest` (the meter sink while resting), for callers that
/// hold other borrows of the studio.
pub(super) fn send_to(
    engine: &mut Engine,
    graph: &GraphDescription,
    rest: &[NodeId],
    drain: bool,
) -> Result<Applied, EngineError> {
    let stripped;
    let graph = if rest.is_empty() {
        graph
    } else {
        let mut g = graph.clone();
        for sink in rest {
            let _ = g.set_state(*sink, Lifecycle::Standby);
            let _ = g.remove_node(*sink);
        }
        stripped = g;
        &stripped
    };
    if drain {
        engine.apply_draining(graph)
    } else {
        engine.apply(graph)
    }
}

/// Adds the meter sink on the mixer's `main` (ADR-0041 decision 2) and adopts it.
pub(super) fn add_sink(
    engine: &mut Engine,
    graph: &mut GraphDescription,
    mixer: NodeId,
    partition: PartitionId,
) -> Result<NodeId, RecordError> {
    let sink = MeterSinkNode::new();
    let id = add_active(graph, Node::<()>::descriptor(&sink), partition)?;
    graph.connect(mixer, "main", id, "in")?;
    engine.adopt::<()>(partition, id, Box::new(sink))?;
    Ok(id)
}
