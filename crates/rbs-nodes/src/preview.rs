//! The preview node: shares the canvas with a UI (spec `docs/specs/io-nodes.md`,
//! "Preview node", ADR-0041), on the render partition.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(any(windows, target_vendor = "apple"))]
use rbs_core::time::Timestamp;
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::share::{PreviewDesc, PreviewEndpoint};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx};

/// Node type id of the preview.
pub const PREVIEW_NODE_TYPE: &str = "org.rbs.preview";
/// Index of the "canvas" input pin.
pub const PREVIEW_CANVAS_PIN: usize = 0;

/// The preview's descriptor: kind `Output`, input "canvas", no outputs.
pub fn preview_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(PREVIEW_NODE_TYPE),
        NodeKind::Output,
        vec![Pin::one("canvas", LinkKind::Video)],
        vec![],
    )
}

/// What a preview node shares with the control and UI threads.
#[derive(Debug, Default)]
pub struct PreviewNodeStats {
    /// Where a UI connects, while the node is prepared. Written in `prepare` and
    /// `release` (control thread), never in `process`.
    endpoint: Mutex<Option<PreviewEndpoint>>,
    /// True while a UI is subscribed.
    pub subscribed: AtomicBool,
    /// Canvas frames scaled into a surface for the UI.
    pub published: AtomicU64,
    /// Canvas frames dropped because the UI held every surface.
    pub busy: AtomicU64,
    /// Publish or tick errors.
    pub errors: AtomicU64,
}

impl PreviewNodeStats {
    /// Where a UI connects (`PreviewSubscriber::connect`), while the node is prepared.
    pub fn endpoint(&self) -> Option<PreviewEndpoint> {
        self.endpoint.lock().ok().and_then(|e| e.clone())
    }

    fn set_endpoint(&self, endpoint: Option<PreviewEndpoint>) {
        if let Ok(mut e) = self.endpoint.lock() {
            *e = endpoint;
        }
    }
}

/// Scales each new canvas frame into a ring of shared surfaces and sends it to the one UI
/// subscribed to its endpoint (`rbs_gpu::share`, ADR-0028, ADR-0036). Nothing is copied
/// to the CPU and nothing is converted while no UI is subscribed.
///
/// An `Output`, so it keeps the compositor and its sources scheduled while nothing
/// records. Available on macOS and Windows; elsewhere `prepare` fails permanently.
pub struct PreviewNode {
    descriptor: NodeDescriptor,
    desc: PreviewDesc,
    #[cfg(any(windows, target_vendor = "apple"))]
    publisher: Option<rbs_gpu::share::PreviewPublisher>,
    #[cfg(any(windows, target_vendor = "apple"))]
    last: Option<(Timestamp, u64)>,
    stats: Arc<PreviewNodeStats>,
}

impl PreviewNode {
    /// A preview of `desc.width x desc.height` (the canvas is scaled to it) over
    /// `desc.surfaces` shared surfaces. Holds only its configuration until `prepare`.
    pub fn new(desc: PreviewDesc) -> Self {
        Self {
            descriptor: preview_descriptor(),
            desc,
            #[cfg(any(windows, target_vendor = "apple"))]
            publisher: None,
            #[cfg(any(windows, target_vendor = "apple"))]
            last: None,
            stats: Arc::default(),
        }
    }

    /// The node's endpoint and counters.
    pub fn stats(&self) -> Arc<PreviewNodeStats> {
        Arc::clone(&self.stats)
    }
}

impl Node<GpuRuntime> for PreviewNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    #[cfg(any(windows, target_vendor = "apple"))]
    fn prepare(&mut self, ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let publisher = rbs_gpu::share::PreviewPublisher::new(ctx.shared, self.desc)
            .map_err(|e| NodeError::Failed(format!("preview: {e}")))?;
        self.stats.set_endpoint(Some(publisher.endpoint().clone()));
        self.publisher = Some(publisher);
        self.last = None;
        Ok(())
    }

    #[cfg(not(any(windows, target_vendor = "apple")))]
    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let _ = self.desc;
        Err(NodeError::Permanent(
            "the preview is not available on this platform yet",
        ))
    }

    #[cfg(any(windows, target_vendor = "apple"))]
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        use rbs_core::media::FramePayload;
        use rbs_gpu::GpuError;

        let gpu = ctx.shared();
        let (inputs, _outputs, local) = ctx.parts();
        let publisher = self.publisher.as_mut().ok_or(NodeError::NotReady)?;
        let mut result = Ok(());
        if let Some(canvas) = inputs.video(PREVIEW_CANVAS_PIN, 0) {
            let key = (canvas.timestamp(), canvas.sequence());
            if self.last != Some(key)
                && let FramePayload::Texture(id) = *canvas.payload()
            {
                self.last = Some(key);
                match publisher.publish(gpu, &local.pool, id, key.0, key.1) {
                    Ok(()) => {
                        if publisher.subscribed() {
                            self.stats.published.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(GpuError::ExportBusy { .. }) => {
                        self.stats.busy.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(GpuError::DeviceLost) => {
                        result = Err(NodeError::Backend("preview: the GPU device was lost"))
                    }
                    Err(_) => {
                        self.stats.errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        // Accepts a UI, reads its releases and sends finished frames; never blocks.
        match publisher.tick(gpu) {
            Ok(()) => {}
            Err(GpuError::DeviceLost) => {
                result = Err(NodeError::Backend("preview: the GPU device was lost"))
            }
            Err(_) => {
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.stats
            .subscribed
            .store(publisher.subscribed(), Ordering::Relaxed);
        result
    }

    #[cfg(not(any(windows, target_vendor = "apple")))]
    fn process(&mut self, _ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Err(NodeError::NotReady)
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        #[cfg(any(windows, target_vendor = "apple"))]
        {
            self.publisher = None;
        }
        self.stats.set_endpoint(None);
        self.stats.subscribed.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_is_a_valid_video_output() {
        let d = preview_descriptor();
        assert_eq!(d.kind, NodeKind::Output);
        assert!(d.validate().is_ok(), "{:?}", d.validate());
        let node = PreviewNode::new(PreviewDesc {
            width: 64,
            height: 36,
            surfaces: 3,
        });
        assert_eq!(Node::<GpuRuntime>::descriptor(&node), &d);
        assert_eq!(
            node.stats().endpoint(),
            None,
            "nothing is shared before prepare"
        );
    }
}
