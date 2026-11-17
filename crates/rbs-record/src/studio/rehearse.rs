//! Rehearse (ADR-0135 decisions 11 to 13): the transition a `canvas.transition` would
//! run now, played on the Preview picture only. A compositor of the preview picture's
//! size draws the program compositor's output and, coming in over it, the preview
//! compositor's; the Preview picture shows it until the duration is over. Program,
//! outputs, the mixer and every state a client sees stay as they are.

use super::transition::{rehearsal_layers, rehearsal_plan};
use super::*;

/// Layers of a rehearsal: the program picture, a colour and the preview picture.
const REHEARSAL_LAYERS: u32 = 4;

/// A rehearsal in progress.
pub(super) struct Rehearsal {
    /// Its compositor, between the two compositors and the Preview picture.
    node: NodeId,
    /// Kept so the compositor's layers stay sent while it runs.
    _layers: LayerSender,
    /// The engine time it ends.
    until: Timestamp,
    /// The program and preview scenes it was started for: a change of either ends it.
    scenes: (Option<SceneId>, Option<SceneId>),
}

impl Studio {
    /// Plays the transition a `canvas.transition` would run now on the Preview picture
    /// only (ADR-0135 decision 11). `Ok(false)`, with nothing changed, outside studio
    /// mode, without a preview picture, while a transition runs, waits or holds on the
    /// main canvas, while a rehearsal plays, and for Cut or a duration of 0.
    pub fn rehearse(
        &mut self,
        collection: &SceneCollection,
        sources: &SourceTypes,
    ) -> Result<bool, RecordError> {
        let canvas = self.settings.canvas;
        let (Some(sp), Some(desc)) = (&self.scene_preview, self.settings.preview) else {
            return Ok(false);
        };
        if self.rehearsal.is_some() || self.transitions.contains_key(&canvas) {
            return Ok(false);
        }
        let Some(preview) = collection.canvas(canvas).and_then(|c| c.preview) else {
            return Ok(false);
        };
        let Some((duration, _, _)) = rehearsal_plan(collection, preview) else {
            return Ok(false);
        };
        let program = self.program_compositor(canvas)?;
        let (incoming, picture) = (sp.compositor, sp.picture.0);
        let size = (desc.width, desc.height);
        let (node, mut layers) = CompositorNode::new(
            CanvasDesc {
                width: size.0,
                height: size.1,
            },
            REHEARSAL_LAYERS,
            Vec::new(),
        );
        let id = add_active(
            &mut self.graph,
            Node::<GpuRuntime>::descriptor(&node),
            RENDER,
        )?;
        let linked = self
            .graph
            .connect(program, "canvas", id, "layers")
            .and_then(|_| self.graph.connect(incoming, "canvas", id, "layers"));
        if let Err(e) = linked {
            let _ = self.graph.remove_node(id);
            return Err(e.into());
        }
        relink(&mut self.graph, incoming, id, picture);
        if let Err(e) = self.engine.adopt::<GpuRuntime>(RENDER, id, Box::new(node)) {
            relink(&mut self.graph, id, incoming, picture);
            let _ = self.graph.remove_node(id);
            return Err(e.into());
        }
        let start = self.engine.clock().now();
        let _ = layers.send(rehearsal_layers(
            collection,
            preview,
            (program, incoming),
            size,
            start,
        ));
        self.rehearsal = Some(Rehearsal {
            node: id,
            _layers: layers,
            until: start + duration,
            scenes: (program_scene(collection, canvas), Some(preview)),
        });
        if let Err(e) = self.update_with(collection, sources, None) {
            self.drop_rehearsal();
            return Err(e);
        }
        Ok(true)
    }

    /// True while a rehearsal plays on the Preview picture.
    pub fn rehearsing(&self) -> bool {
        self.rehearsal.is_some()
    }

    /// True when a rehearsal's time is over: call [`Studio::end_rehearsal`].
    pub fn rehearsal_done(&self) -> bool {
        let now = self.engine.clock().now();
        self.rehearsal.as_ref().is_some_and(|r| now >= r.until)
    }

    /// How long the control thread may sleep before a rehearsal ends; `None` without
    /// one.
    pub fn rehearsal_wait(&self, now: Timestamp) -> Option<Duration> {
        self.rehearsal
            .as_ref()
            .map(|r| r.until.saturating_since(now))
    }

    /// Ends a rehearsal (its time is over, or the owner stops it): the Preview picture
    /// shows the preview scene again and the rehearsal compositor leaves after the
    /// update. Without one it changes nothing.
    pub fn end_rehearsal(&mut self, sources: &SourceTypes) -> Result<(), RecordError> {
        if self.rehearsal.is_none() {
            return Ok(());
        }
        self.drop_rehearsal();
        let collection = self.collection.clone();
        self.update(&collection, sources).map(drop)
    }

    /// Ends the rehearsal at once when `collection` changes its program or preview
    /// scene (decision 13); the caller's update applies it.
    pub(super) fn end_rehearsal_for(&mut self, collection: &SceneCollection) {
        let canvas = self.settings.canvas;
        let now = (
            program_scene(collection, canvas),
            collection.canvas(canvas).and_then(|c| c.preview),
        );
        if self.rehearsal.as_ref().is_some_and(|r| r.scenes != now) {
            self.drop_rehearsal();
        }
    }

    /// Links the Preview picture back to the preview compositor and takes the
    /// rehearsal compositor out of the graph; it leaves the engine after the next
    /// apply.
    pub(super) fn drop_rehearsal(&mut self) {
        let Some(r) = self.rehearsal.take() else {
            return;
        };
        if let Some(sp) = &self.scene_preview {
            relink(&mut self.graph, r.node, sp.compositor, sp.picture.0);
        }
        if self.graph.node(r.node).is_some() {
            let _ = self.graph.set_state(r.node, Lifecycle::Standby);
            let _ = self.graph.remove_node(r.node);
        }
        let leaving = Added {
            node: r.node,
            partition: RENDER,
            gpu: true,
        };
        self.pending_takes.push((leaving, None));
    }
}

/// Moves `picture`'s canvas input from `from` to `to`.
fn relink(graph: &mut GraphDescription, from: NodeId, to: NodeId, picture: NodeId) {
    let old: Vec<_> = graph
        .links()
        .iter()
        .filter(|l| l.from.node == from && l.to.node == picture)
        .cloned()
        .collect();
    for link in old {
        graph.disconnect(link);
    }
    let _ = graph.connect(to, "canvas", picture, "canvas");
}
