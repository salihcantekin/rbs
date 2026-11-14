//! The loudness meter on a running studio (ADR-0092 stage 1; spec
//! `docs/specs/recording.md`, "Loudness meter").
//!
//! [`Studio::start_loudness`] adds a [`LoudnessTapNode`] on the audio partition and
//! links it to the main canvas mixer's `main` output, beside the meter sink and the
//! encoders: a parallel reader, so the mix, the encoders and the meters are unchanged
//! and nothing dips. [`Studio::stop_loudness`] removes it with a plain swap. While it
//! is off nothing of it exists (ADR-0013). The mixer's `main` is the master tap point
//! while the master has no filter chain; when one is added (ADR-0092 decision 2) the
//! tap moves to the chain's tail.

use rbs_audio::{Loudness, LoudnessReadout, LoudnessTapNode};

use super::*;

/// The loudness tap while it runs.
pub(super) struct LoudnessRun {
    node: Added,
    readout: Arc<LoudnessReadout>,
}

impl Studio {
    /// Starts measuring the loudness of the main canvas's mix. True when it started,
    /// false when it was already running (it keeps its measurement). Fails with
    /// `NotBound("mixer")` without audio; on any failure nothing of it stays.
    pub fn start_loudness(&mut self) -> Result<bool, RecordError> {
        if self.loudness.is_some() {
            return Ok(false);
        }
        let Some(mixer) = &self.mixer else {
            return Err(RecordError::NotBound("mixer"));
        };
        let mixer_id = mixer.node;
        let partition = self.settings.partitions.audio;
        let (node, readout, _stats) = LoudnessTapNode::new(self.settings.audio_graph);
        let mut graph = self.graph.clone();
        let id = add_active(&mut graph, Node::<()>::descriptor(&node), partition)?;
        graph.connect(mixer_id, "main", id, "in")?;
        self.engine.adopt::<()>(partition, id, Box::new(node))?;
        let added = Added {
            node: id,
            partition,
            gpu: false,
        };
        if let Err(e) = self.engine.apply(&graph) {
            if !take(&mut self.engine, added) {
                self.pending_takes.push((added, None));
            }
            return Err(e.into());
        }
        self.graph = graph;
        self.loudness = Some(LoudnessRun {
            node: added,
            readout,
        });
        Ok(true)
    }

    /// Stops measuring loudness: unlinks and removes the tap with a plain swap. The
    /// node comes back through the pending takes and is dropped on this thread. False
    /// when it was not running.
    pub fn stop_loudness(&mut self) -> bool {
        let Some(run) = self.loudness.take() else {
            return false;
        };
        let _ = self.graph.set_state(run.node.node, Lifecycle::Standby);
        let _ = self.graph.remove_node(run.node.node);
        let _ = self.engine.apply(&self.graph);
        if !take(&mut self.engine, run.node) {
            self.pending_takes.push((run.node, None));
        }
        true
    }

    /// The loudness of the main canvas's mix while it is measured; `None` while the
    /// meter is off.
    pub fn loudness(&self) -> Option<Loudness> {
        self.loudness.as_ref().map(|l| l.readout.get())
    }

    /// Restarts the loudness measurement (integrated loudness, range, maximum true
    /// peak, maxima and windows) at the next audio block. False while the meter is
    /// off.
    pub fn reset_loudness(&self) -> bool {
        match &self.loudness {
            Some(l) => {
                l.readout.reset();
                true
            }
            None => false,
        }
    }
}
