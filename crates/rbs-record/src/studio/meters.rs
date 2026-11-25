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

/// The meter sink while the branch rests: left out of the graph the engine gets.
pub(super) fn resting_sink(studio: &Studio) -> Option<NodeId> {
    if studio.audio_awake {
        return None;
    }
    studio.mixer.as_ref().map(|m| m.sink)
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
        let rest = resting_sink(self);
        send_to(&mut self.engine, graph.unwrap_or(&self.graph), rest, drain)
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
    rest: Option<NodeId>,
    drain: bool,
) -> Result<Applied, EngineError> {
    let stripped;
    let graph = match rest {
        Some(sink) => {
            let mut g = graph.clone();
            let _ = g.set_state(sink, Lifecycle::Standby);
            let _ = g.remove_node(sink);
            stripped = g;
            &stripped
        }
        None => graph,
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
