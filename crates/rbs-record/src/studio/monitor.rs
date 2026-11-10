//! The monitor output on a running studio (ADR-0097 F1; spec `docs/specs/recording.md`,
//! "Monitor output").
//!
//! The main canvas's mixer always declares its `monitor` pin; it does monitor work only
//! while a node reads that pin. [`Studio::start_monitor`] adds an [`OutputNode`] for a
//! device output the host opened and links it to that pin; [`Studio::stop_monitor`]
//! removes it with a plain swap and drops the device off the audio thread. Linking and
//! unlinking keep the mixer instance, so strip ramps survive and the stream never
//! glitches. While no monitor plays, nothing of it exists (ADR-0013): the host asks
//! [`Studio::monitor_wanted`] and opens the device only when a strip is monitored or
//! soloed. Opening, closing and retrying the device stay with the host (decision 4).

use rbs_audio::{DEFAULT_OUTPUT_TARGET, OutputNode, OutputNodeStats as MonitorNodeStats};
use rbs_audio::{MAX_OUTPUT_VOLUME, OutputParams};
use rbs_audio_io::AudioOutput;

use super::*;

/// The monitor output while it plays.
pub(super) struct MonitorRun {
    node: Added,
    params: ParamSender<OutputParams>,
    stats: Arc<MonitorNodeStats>,
    latency: Duration,
    pub(super) failure: Option<String>,
}

impl MonitorRun {
    pub(super) fn node(&self) -> NodeId {
        self.node.node
    }
}

/// The monitor output while it plays ([`Studio::monitor`]).
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct MonitorState {
    /// How far the device plays behind the graph's real time: graph latency, the
    /// output ring's target and the device's own latency (ADR-0097 decision 3).
    pub latency: Duration,
    /// The output node's counters.
    pub stats: Arc<MonitorNodeStats>,
    /// Why its node failed, until it runs again (the device was lost, for example).
    /// The studio, its mix and its outputs run on either way; the host closes the
    /// monitor and opens the device again with back-off.
    pub failure: Option<String>,
}

impl Studio {
    /// True when the monitor output has something to play: a strip of the main
    /// canvas's mixer is monitored (`Only` or `Also`) or soloed. The host opens the
    /// monitor device when this turns true and closes it when it turns false
    /// (ADR-0097 decision 7). False without audio.
    pub fn monitor_wanted(&self) -> bool {
        self.mixer.is_some()
            && self
                .strips
                .iter()
                .any(|s| s.audio.monitor != SourceMonitor::Off || self.solo.contains(&s.source))
    }

    /// Starts the monitor output on `output`, a device output the host opened (in
    /// Standby) for the audio graph's format: adds an [`OutputNode`] on the audio
    /// partition, links the mixer's `monitor` pin to it and applies, at `volume`
    /// (clamped to `0.0..=MAX_OUTPUT_VOLUME`). The mixer instance stays, so the stream
    /// does not glitch. Fails with `MonitorPlaying` while a monitor plays and
    /// `NotBound("mixer")` without audio; on any failure nothing of the monitor stays
    /// and the output is dropped.
    pub fn start_monitor(
        &mut self,
        output: Box<dyn AudioOutput>,
        volume: f32,
    ) -> Result<(), RecordError> {
        if self.monitor.is_some() {
            return Err(RecordError::MonitorPlaying);
        }
        let Some(mixer) = &self.mixer else {
            return Err(RecordError::NotBound("mixer"));
        };
        let mixer_id = mixer.node;
        let partition = self.settings.partitions.audio;
        let (node, mut params, stats) =
            OutputNode::new(output, self.settings.audio_graph, DEFAULT_OUTPUT_TARGET);
        let latency = node.latency();
        let _ = params.send(OutputParams::with_volume(clamp_volume(volume)));
        let mut graph = self.graph.clone();
        let id = add_active(&mut graph, Node::<()>::descriptor(&node), partition)?;
        graph.connect(mixer_id, "monitor", id, "in")?;
        // A refused adoption drops the node, and with it the output, here.
        self.engine.adopt::<()>(partition, id, Box::new(node))?;
        let added = Added {
            node: id,
            partition,
            gpu: false,
        };
        let applied = match self.engine.apply(&graph) {
            Ok(applied) => applied,
            Err(e) => {
                if !take(&mut self.engine, added) {
                    self.pending_takes.push((added, None));
                }
                return Err(e.into());
            }
        };
        // A device that cannot start fails the node's prepare: the monitor stays,
        // reports why and plays nothing; the mixer does not count it as a reader, so
        // `Only` strips stay in the mix (ADR-0097 decision 5).
        let failure = applied
            .reports
            .values()
            .flat_map(|r| r.failed.iter())
            .find(|(n, _)| *n == id)
            .map(|(_, why)| why.to_string());
        self.graph = graph;
        self.monitor = Some(MonitorRun {
            node: added,
            params,
            stats,
            latency,
            failure,
        });
        Ok(())
    }

    /// Stops the monitor output: unlinks and removes its node with a plain swap (the
    /// mixer's `Only` strips ramp back into the mix). The node comes back through the
    /// pending takes and is dropped on this (the control) thread with its device,
    /// never on the audio thread. False when no monitor played.
    pub fn stop_monitor(&mut self) -> bool {
        let Some(run) = self.monitor.take() else {
            return false;
        };
        let _ = self.graph.set_state(run.node.node, Lifecycle::Standby);
        let _ = self.graph.remove_node(run.node.node);
        // An apply error leaves the node to the pending takes like any other.
        let _ = self.engine.apply(&self.graph);
        if !take(&mut self.engine, run.node) {
            self.pending_takes.push((run.node, None));
        }
        true
    }

    /// Sets the monitor volume (clamped to `0.0..=MAX_OUTPUT_VOLUME`); the node ramps
    /// to it. False when no monitor plays.
    pub fn set_monitor_volume(&mut self, volume: f32) -> bool {
        let Some(run) = &mut self.monitor else {
            return false;
        };
        let _ = run
            .params
            .send(OutputParams::with_volume(clamp_volume(volume)));
        true
    }

    /// The monitor output while it plays.
    pub fn monitor(&self) -> Option<MonitorState> {
        self.monitor.as_ref().map(|m| MonitorState {
            latency: m.latency,
            stats: Arc::clone(&m.stats),
            failure: m.failure.clone(),
        })
    }

    /// Sets the soloed sources (session state, ADR-0097 decision 5): while any strip is
    /// soloed, the monitor plays only soloed strips; the mix and the encoders are
    /// unchanged. Sources that have no strip are kept and apply once they have one.
    /// Returns false if the studio has no mixer.
    pub fn set_solo(&mut self, solo: BTreeSet<SourceId>) -> bool {
        self.solo = solo;
        let Some(mixer) = &mut self.mixer else {
            return false;
        };
        let _ = mixer.params.send(mixer_params(
            &self.strips,
            &mixer.master,
            self.master,
            &self.solo,
        ));
        true
    }

    /// The soloed sources.
    pub fn solo(&self) -> &BTreeSet<SourceId> {
        &self.solo
    }
}

fn clamp_volume(volume: f32) -> f32 {
    if volume.is_finite() {
        volume.clamp(0.0, MAX_OUTPUT_VOLUME)
    } else {
        0.0
    }
}
