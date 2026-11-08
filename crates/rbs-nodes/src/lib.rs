//! Built-in graph nodes that wrap the ports (spec `docs/specs/io-nodes.md`, ADR-0038).
//!
//! | Node | Type id | Kind | Partition | Runtime |
//! | --- | --- | --- | --- | --- |
//! | [`capture::CaptureSourceNode`] | `org.rbs.capture` | Source | render | `GpuRuntime` |
//! | [`still::StillSourceNode`] | `org.rbs.still` | Source | render | `GpuRuntime` |
//! | [`export::VideoExportNode`] | `org.rbs.video-export` | VideoFilter | render | `GpuRuntime` |
//! | [`encode::VideoEncoderNode`] | `org.rbs.video-encoder` | Encoder | one per encoder | any |
//! | [`output::OutputNode`] | `org.rbs.output` | Output | one per output | any |
//! | [`preview::PreviewNode`] | `org.rbs.preview` | Output | render | `GpuRuntime` |
//! | [`vcam::VirtualCameraNode`] | `org.rbs.virtual-camera` | Output | render | `GpuRuntime` |
//!
//! A recording is `capture -> compositor -> video export ==queue==> video encoder
//! ==queue==> output`, where `==queue==>` is a partition crossing (ADR-0026). The nodes
//! know only port traits: the composition root (`rbs-app`, `rbs-cli`) picks adapters and
//! hands their factories in, so the same nodes run with WGC, ScreenCaptureKit, PipeWire,
//! NVENC, AMF, VideoToolbox and any later adapter.
//!
//! Every node follows the Installed / Standby / Active lifecycle (ADR-0013): a node that
//! is constructed but not prepared holds only its configuration. The exception is
//! [`encode::VideoEncoderNode::open`], which creates the encoder session so its stream
//! descriptor is known before the output is built; construct it when the output starts.

pub mod capture;
pub mod encode;
pub mod export;
pub mod output;
pub mod preview;
pub mod still;
pub mod vcam;

pub use capture::{CAPTURE_NODE_TYPE, CaptureSourceNode, capture_descriptor, capture_fault};
pub use encode::{VIDEO_ENCODER_NODE_TYPE, VideoEncoderNode, video_encoder_descriptor};
pub use export::{ExportEnd, VIDEO_EXPORT_NODE_TYPE, VideoExportNode, video_export_descriptor};
pub use output::{OUTPUT_NODE_TYPE, OutputNode, output_descriptor};
pub use preview::{PREVIEW_NODE_TYPE, PreviewNode, PreviewNodeStats, preview_descriptor};
pub use still::{STILL_NODE_TYPE, StillError, StillSourceNode, still_descriptor};
pub use vcam::{
    VIRTUAL_CAMERA_NODE_TYPE, VirtualCameraNode, VirtualCameraNodeStats, virtual_camera_descriptor,
};
