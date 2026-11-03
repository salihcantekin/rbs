//! Built-in graph nodes that wrap the ports (spec `docs/specs/io-nodes.md`, ADR-0038).
//!
//! | Node | Type id | Kind | Partition | Runtime |
//! | --- | --- | --- | --- | --- |
//! | [`capture::CaptureSourceNode`] | `org.rbs.capture` | Source | render | `GpuRuntime` |
//! | [`export::VideoExportNode`] | `org.rbs.video-export` | VideoFilter | render | `GpuRuntime` |
//! | [`encode::VideoEncoderNode`] | `org.rbs.video-encoder` | Encoder | one per encoder | any |
//! | [`output::OutputNode`] | `org.rbs.output` | Output | one per output | any |
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

pub use capture::{CAPTURE_NODE_TYPE, CaptureSourceNode, capture_descriptor};
pub use encode::{VIDEO_ENCODER_NODE_TYPE, VideoEncoderNode, video_encoder_descriptor};
pub use export::{VIDEO_EXPORT_NODE_TYPE, VideoExportNode, video_export_descriptor};
pub use output::{OUTPUT_NODE_TYPE, OutputNode, output_descriptor};
