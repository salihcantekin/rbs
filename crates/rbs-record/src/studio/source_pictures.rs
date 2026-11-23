//! Pictures of one source: the Shown hold and relinking of Interact pictures
//! (ADR-0132 decision 6, ADR-0152 decision 2), and one frame of a running source for
//! its Region preview (ADR-0176 decision 2): a
//! normalisation pass from the source's own node at the grab size and a
//! [`SourceGrabNode`] behind it, kept as a picture without an endpoint so a replaced
//! source node is relinked like an Interact picture's. The source is not held at
//! Shown (decision 3): a source in standby has no node and the grab is `NotBound`.

use std::sync::Arc;

use rbs_filters::{FilterNode, FilterType, Normalise, Values};
use rbs_gpu::node::GpuRuntime;
use rbs_graph::description::NodeId;
use rbs_graph::runtime::Node;
use rbs_nodes::{GrabSlot, SourceGrabNode};
use rbs_scene::SceneCollection;
use rbs_scene::model::SourceId;
use rbs_sources::SourceTypes;

use super::{AddedPicture, PictureId, Studio, iso_video};
use crate::recording::{RENDER, RecordError, StartFailure, add_active};

/// The grab size of a `width x height` picture: fits a square of `max_long`, keeps the
/// aspect, never larger than the picture and at least 1 x 1. `None` for an empty
/// picture or `max_long`.
fn grab_size(width: u32, height: u32, max_long: u32) -> Option<(u32, u32)> {
    if width == 0 || height == 0 || max_long == 0 {
        return None;
    }
    let long = width.max(height);
    if long <= max_long {
        return Some((width, height));
    }
    let scale = |side: u32| {
        let scaled = u64::from(side) * u64::from(max_long) / u64::from(long);
        u32::try_from(scaled).unwrap_or(max_long).max(1)
    };
    Some((scale(width), scale(height)))
}

impl Studio {
    /// Undoes the hold of a failed [`Studio::add_source_picture`] (`fresh`: it added
    /// it) and binds again without it.
    pub(super) fn forget_shown_by(
        &mut self,
        fresh: bool,
        source: SourceId,
        collection: &SceneCollection,
        sources: &SourceTypes,
    ) {
        if fresh
            && !self
                .pictures
                .iter()
                .any(|p| p.holds && p.source == Some(source))
        {
            self.shown_by.remove(&source);
            let _ = self.update_with(collection, sources, None);
        }
    }

    /// A source picture left: its source is no longer held, unless another picture
    /// still shows it.
    pub(super) fn release_shown_by(&mut self, source: Option<SourceId>) {
        if let Some(source) = source
            && !self
                .pictures
                .iter()
                .any(|p| p.holds && p.source == Some(source))
        {
            self.shown_by.remove(&source);
        }
    }

    /// Links each source picture from its source's node again after a sync that
    /// replaced the node (a changed address, a restart).
    pub(super) fn relink_source_pictures(&mut self) {
        let pairs: Vec<(SourceId, NodeId)> = self
            .pictures
            .iter()
            .filter_map(|p| Some((p.source?, p.normalise?)))
            .collect();
        for (source, to) in pairs {
            let Some(node) = self.binder.source_node(source) else {
                continue;
            };
            let current = self
                .graph
                .links()
                .iter()
                .find(|l| l.to.node == to && l.to.pin == 0)
                .copied();
            if current.is_some_and(|l| l.from.node == node) {
                continue;
            }
            if let Some(link) = current {
                self.graph.disconnect(link);
            }
            if let Ok(pin) = iso_video::video_pin(&self.graph, node) {
                let _ = self.graph.connect(node, &pin, to, "in");
            }
        }
    }

    /// Adds a one-frame grab of running source `source` (ADR-0176 decision 2): the
    /// source's own picture, before its filters and crops, at [`grab_size`] of its
    /// picture size and `max_long`, read back once as straight-alpha sRGB RGBA8 into
    /// the returned slot. The control thread is woken when the frame is there. Remove it
    /// with [`Studio::remove_picture`] once taken; a fault shows in
    /// [`Studio::picture_failure`]. `NotBound` when the source has no node (standby,
    /// gone) or no picture size yet. The source is not held at Shown.
    pub fn add_source_grab(
        &mut self,
        collection: &SceneCollection,
        sources: &SourceTypes,
        source: SourceId,
        max_long: u32,
    ) -> Result<(PictureId, Arc<GrabSlot>), RecordError> {
        if collection.source(source).is_none() {
            return Err(RecordError::NotBound("source"));
        }
        let node = self
            .binder
            .source_node(source)
            .ok_or(RecordError::NotBound("source"))?;
        let (w, h) = self
            .picture_size(source)
            .and_then(|(w, h)| grab_size(w, h, max_long))
            .ok_or(RecordError::NotBound("picture size"))?;
        let grab = SourceGrabNode::new(w, h).waking(self.control_wake());
        let slot = grab.slot();
        let mut made: Vec<NodeId> = Vec::new();
        let added = (|| {
            let pin = iso_video::video_pin(&self.graph, node)?;
            let ty: Arc<dyn FilterType> = Arc::new(Normalise);
            let values = Values::defaults(ty.as_ref());
            let (pass, _, _) = FilterNode::new(ty, values, Some([w, h]));
            let norm = add_active(
                &mut self.graph,
                Node::<GpuRuntime>::descriptor(&pass),
                RENDER,
            )?;
            made.push(norm);
            self.graph.connect(node, &pin, norm, "in")?;
            self.engine
                .adopt::<GpuRuntime>(RENDER, norm, Box::new(pass))?;
            let id = add_active(
                &mut self.graph,
                Node::<GpuRuntime>::descriptor(&grab),
                RENDER,
            )?;
            made.push(id);
            self.graph.connect(norm, "out", id, "picture")?;
            self.engine
                .adopt::<GpuRuntime>(RENDER, id, Box::new(grab))?;
            Ok::<_, RecordError>((id, norm))
        })();
        let (node, normalise) = match added {
            Ok(nodes) => nodes,
            Err(e) => {
                for n in made {
                    self.take_out(n);
                }
                return Err(e);
            }
        };
        let id = PictureId(self.next_picture);
        self.next_picture += 1;
        self.pictures.push(AddedPicture {
            id,
            canvas: None,
            source: Some(source),
            node,
            normalise: Some(normalise),
            holds: false,
            stats: Arc::default(),
            failure: None,
        });
        let updated = self.update_with(collection, sources, None);
        let failure = self
            .pictures
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.failure.clone());
        match (updated, failure) {
            (Ok(_), None) => Ok((id, slot)),
            (Ok(_), Some(why)) => {
                self.drop_picture(id);
                let _ = self.update_with(collection, sources, None);
                Err(RecordError::Start(vec![StartFailure {
                    node: "source grab".to_owned(),
                    reason: why,
                }]))
            }
            (Err(e), _) => {
                self.drop_picture(id);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::grab_size;

    #[test]
    fn the_grab_fits_a_square_and_keeps_the_aspect() {
        assert_eq!(grab_size(1280, 720, 960), Some((960, 540)));
        assert_eq!(grab_size(720, 1280, 960), Some((540, 960)));
        assert_eq!(grab_size(640, 480, 960), Some((640, 480)));
        assert_eq!(grab_size(5000, 1, 960), Some((960, 1)));
        assert_eq!(grab_size(0, 720, 960), None);
        assert_eq!(grab_size(1280, 720, 0), None);
    }
}
