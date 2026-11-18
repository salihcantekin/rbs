//! Scrolling layers on the studio side (ADR-0150 decisions 5 and 7): the scroll runs
//! every list shares, and each compositor's channels.

use std::collections::BTreeSet;

use rbs_gpu::node::{CompositorNode, LayerSender, NodeLayer};
use rbs_gpu::{GpuContext, ScrollBinding, ScrollSender};
use rbs_scene::ScrollKey;
use rbs_scene::binder::BoundLayer;

/// A compositor's layer channel and scroll channel. The scroll binding (the wrap
/// pipelines of the device and a record buffer) is made and sent before the first list
/// with a scrolling layer, so a compositor that never scrolls has none (P13).
pub(crate) struct CompositorPort {
    layers: LayerSender,
    scroll: ScrollSender,
    bound: bool,
}

impl CompositorPort {
    /// A new compositor node (as [`CompositorNode::new`] returns it) with its scroll
    /// channel, and the port that feeds it.
    pub(crate) fn of_new((node, layers): (CompositorNode, LayerSender)) -> (CompositorNode, Self) {
        let (node, scroll) = node.with_scroll();
        (
            node,
            Self {
                layers,
                scroll,
                bound: false,
            },
        )
    }

    /// Sends `layers`, first binding scrolling on `gpu` when one scrolls and this
    /// compositor has no binding yet. Builds GPU objects then: control thread only.
    pub(crate) fn send(&mut self, layers: Vec<NodeLayer>, gpu: &GpuContext) {
        if !self.bound && layers.iter().any(|l| l.scroll.is_some()) {
            let _ = self.scroll.send(Some(ScrollBinding::new(gpu)));
            self.bound = true;
        }
        let _ = self.layers.send(layers);
    }

    /// The last list sent has not been taken by the node yet.
    pub(crate) fn pending(&self) -> bool {
        self.layers.pending()
    }

    /// The node is gone.
    pub(crate) fn is_closed(&self) -> bool {
        self.layers.is_closed()
    }
}

/// The run keys of `layers`.
pub(crate) fn keys_of<'a>(
    layers: impl IntoIterator<Item = &'a BoundLayer>,
    out: &mut BTreeSet<ScrollKey>,
) {
    out.extend(layers.into_iter().filter_map(|l| l.scroll_key));
}
