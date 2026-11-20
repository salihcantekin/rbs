//! The virtual camera's run inside a studio (ADR-0066 decisions 6 and 7) and the
//! readback it shares with an encode of the same shape (audit F-04).

use std::sync::Arc;

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{ColorSpace, MemoryKind, PixelFormat};
use rbs_core::timing::FrameRate;
use rbs_gpu::YuvFormat;
use rbs_gpu::download::DownloadShare;
use rbs_gpu::node::GpuRuntime;
use rbs_graph::runtime::Node;
use rbs_nodes::{VirtualCameraNode, VirtualCameraNodeStats};
use rbs_scene::model::CanvasId;
use rbs_vcam::{CameraFormat, VcamError, VirtualCameraConfig, VirtualCameraFactory};

use super::{Added, Chains, Studio, take};
use crate::recording::{RENDER, VideoSettings, add_active};

/// What the virtual camera announces and RBS sends (ADR-0066 decision 7): NV12 at this
/// size (both even) and rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct VirtualCameraSettings {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Frames per second the camera announces; RBS sends at this rate, or at the canvas
    /// rate when that is lower.
    pub rate: FrameRate,
}

impl VirtualCameraSettings {
    /// A camera of `width x height` at `rate`.
    pub fn new(width: u32, height: u32, rate: FrameRate) -> Self {
        Self {
            width,
            height,
            rate,
        }
    }
}

/// The virtual camera while it sends ([`Studio::virtual_camera`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct VirtualCameraState {
    /// The configuration its session was opened with.
    pub config: VirtualCameraConfig,
    /// Its node's counters: frames handed to the session, and frames skipped before
    /// conversion (`no_reader` while no app reads the camera).
    pub stats: Arc<VirtualCameraNodeStats>,
    /// The session's counters, when the adapter shares them: what its sender did with
    /// the frames (`sent` to the consumer side, `consumer_absent` and `format_pending`
    /// for the few frames in flight when an app stops reading, since the node skips
    /// frames nobody reads; its `consumer_busy` includes the node's own skips).
    pub session: Option<rbs_vcam::VcamCountersReader>,
    /// Why its node failed, until it runs again. The studio runs on either way.
    pub failure: Option<String>,
}

/// The virtual camera's node while it sends.
pub(super) struct CameraRun {
    pub(super) node: Added,
    pub(super) config: VirtualCameraConfig,
    pub(super) stats: Arc<VirtualCameraNodeStats>,
    pub(super) session: Option<rbs_vcam::VcamCountersReader>,
    pub(super) failure: Option<String>,
    /// The readback it shares with an encode of the same shape (audit F-04).
    pub(super) download: CameraDownload,
}

/// The virtual camera's readback share: NV12, Rec. 709 limited, closed shadows, one
/// canvas and one output size. An encode of that shape uses the same share, and the
/// camera uses an encode's when one is already running. Either side may stop first.
pub(crate) struct CameraDownload {
    pub(super) canvas: CanvasId,
    pub(super) canvas_size: (u32, u32),
    pub(super) output: (u32, u32),
    pub(super) share: Arc<DownloadShare>,
}

/// Whether `video` at `output` on `canvas` is the virtual camera's readback shape.
pub(super) fn camera_readback_shape(
    video: &VideoSettings,
    canvas: CanvasId,
    canvas_size: (u32, u32),
    output: (u32, u32),
    cam: &CameraDownload,
) -> bool {
    video.format == YuvFormat::Nv12
        && video.color == ColorSpace::BT709_LIMITED
        && !video.open_shadows
        && cam.canvas == canvas
        && cam.canvas_size == canvas_size
        && cam.output == output
}

/// The readback share for a new system-memory export: the camera's when the shape
/// matches, else a running readback chain of that shape, else a new one. Native
/// chains are not sources; they do not read back.
pub(super) fn download_share(
    running: &Chains,
    camera: Option<&CameraDownload>,
    canvas: CanvasId,
    canvas_size: (u32, u32),
    output: (u32, u32),
    video: &VideoSettings,
) -> Arc<DownloadShare> {
    if let Some(cam) = camera
        && camera_readback_shape(video, canvas, canvas_size, output, cam)
    {
        return Arc::clone(&cam.share);
    }
    running
        .video
        .iter()
        .flatten()
        .find(|c| {
            c.owners.is_none()
                && c.canvas == canvas
                && c.size == output
                && c.settings.format == video.format
                && c.settings.color == video.color
                && c.settings.open_shadows == video.open_shadows
        })
        .map_or_else(|| Arc::new(DownloadShare::new()), |c| Arc::clone(&c.share))
}

impl Studio {
    /// Starts sending the program to a virtual camera (ADR-0066 decision 6): opens a
    /// session of `factory` for `settings` on this (the control) thread, then adds a
    /// [`VirtualCameraNode`] on [`RENDER`] after the program compositor and applies. It
    /// is not one of the [`MAX_OUTPUTS`] outputs and touches none of them. Fails with
    /// `InvalidConfig` while the camera already sends; an engine or graph refusal is
    /// `Os { code: 0, .. }`, and nothing of the camera stays.
    pub fn start_virtual_camera(
        &mut self,
        factory: &dyn VirtualCameraFactory,
        settings: VirtualCameraSettings,
    ) -> Result<(), VcamError> {
        if self.camera.is_some() {
            return Err(VcamError::InvalidConfig(
                "the virtual camera already sends".into(),
            ));
        }
        let config = VirtualCameraConfig::new(
            CameraFormat::new(
                settings.width,
                settings.height,
                settings.rate,
                PixelFormat::Nv12,
            ),
            MemoryKind::System,
        );
        let session = factory.open(&config)?;
        let counters = session.counters();
        let (width, height) = self.size;
        let canvas = self.settings.canvas;
        let video = VideoSettings::default();
        let share = download_share(
            &self.chains,
            None,
            canvas,
            (width, height),
            (settings.width, settings.height),
            &video,
        );
        let node =
            VirtualCameraNode::new(session, config, width, height).with_share(Arc::clone(&share));
        let stats = node.stats();
        let engine_error = |e: String| VcamError::Os {
            code: 0,
            message: e,
        };
        let mut graph = self.graph.clone();
        let id = add_active(&mut graph, Node::<GpuRuntime>::descriptor(&node), RENDER)
            .map_err(|e| engine_error(e.to_string()))?;
        graph
            .connect(self.compositor, "canvas", id, "canvas")
            .map_err(|e| engine_error(e.to_string()))?;
        // A refused adoption drops the node, and with it the session, here.
        self.engine
            .adopt::<GpuRuntime>(RENDER, id, Box::new(node))
            .map_err(|e| engine_error(e.to_string()))?;
        let added = Added {
            node: id,
            partition: RENDER,
            gpu: true,
        };
        if let Err(e) = self.engine.apply(&graph) {
            if !take(&mut self.engine, added) {
                self.pending_takes.push((added, None));
            }
            return Err(engine_error(e.to_string()));
        }
        self.graph = graph;
        self.camera = Some(CameraRun {
            node: added,
            config,
            stats,
            session: counters,
            failure: None,
            download: CameraDownload {
                canvas,
                canvas_size: (width, height),
                output: (settings.width, settings.height),
                share,
            },
        });
        Ok(())
    }

    /// Stops sending to the virtual camera: removes its node with a plain swap. The node
    /// comes back through the pending takes and is dropped here, on the control thread,
    /// with its session (whose drop joins the adapter's sender thread), never on
    /// [`RENDER`]. The camera itself stays installed. False when it was not sending.
    pub fn stop_virtual_camera(&mut self) -> bool {
        let Some(run) = self.camera.take() else {
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

    /// The virtual camera while it sends: its configuration, counters and failure.
    pub fn virtual_camera(&self) -> Option<VirtualCameraState> {
        self.camera.as_ref().map(|c| VirtualCameraState {
            config: c.config,
            stats: Arc::clone(&c.stats),
            session: c.session.clone(),
            failure: c.failure.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use rbs_core::media::Size;
    use rbs_scene::SceneCollection;

    use super::*;

    #[test]
    fn the_camera_joins_only_an_nv12_limited_download_of_its_size() {
        let mut col = SceneCollection::new();
        let (canvas, _) = col
            .add_canvas("Main", Size::new(64, 36), FrameRate::FPS_30)
            .unwrap();
        let cam = CameraDownload {
            canvas,
            canvas_size: (64, 36),
            output: (64, 36),
            share: Arc::new(DownloadShare::new()),
        };
        let video = VideoSettings::default();
        assert!(camera_readback_shape(
            &video,
            canvas,
            (64, 36),
            (64, 36),
            &cam
        ));
        let mut shadows = video.clone();
        shadows.open_shadows = true;
        assert!(!camera_readback_shape(
            &shadows,
            canvas,
            (64, 36),
            (64, 36),
            &cam
        ));
        let mut ten_bit = video.clone();
        ten_bit.format = YuvFormat::P010;
        assert!(!camera_readback_shape(
            &ten_bit,
            canvas,
            (64, 36),
            (64, 36),
            &cam
        ));
        assert!(!camera_readback_shape(
            &video,
            canvas,
            (64, 36),
            (32, 18),
            &cam
        ));
        assert!(!camera_readback_shape(
            &video,
            canvas,
            (32, 18),
            (64, 36),
            &cam
        ));
    }
}
