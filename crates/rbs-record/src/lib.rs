//! Records one canvas of a scene collection to a file (spec `docs/specs/recording.md`,
//! ADR-0040).
//!
//! [`Recording::start`] binds the collection with the scene binder and the source type
//! registry, builds the compositor and the mixer, attaches the video export, the video
//! and audio encoders and a muxed output, adds the partitions to the engine and applies:
//!
//! ```text
//! sources --video--> compositor --canvas--> export ==video==> video encoder ==packets==+
//!  (render)           (render)              (render)          (video encoder)        |
//!                                                                                     +==> output
//! audio inputs --audio--> mixer ==audio==> audio encoder ==packets===================+   (output)
//!  (audio)                (audio)          (audio encoder)
//! ```
//!
//! Adapters come in as probed port registries and are chosen by capability (P1, P3).
//! Every stream is timestamped on the engine's master clock, so the muxer keeps them
//! in sync (ADR-0004).

mod clip;
mod recording;
mod select;
mod studio;
mod timed_show;

pub use clip::{CLIP_WRITE_TIMEOUT, ClipFile, ClipWriteError, ClipWriter, write_clip};
pub use rbs_nodes::replay::{Clip, ClipBounds, ClipError};
pub use rbs_nodes::replay_node::{ReplayStatus, RequestError as ReplayRequestError};
pub use recording::{
    AUDIO, AUDIO_ENCODER, AUDIO_ENCODER_2, AUDIO_ENCODER_3, AUDIO_ENCODER_4, AudioSettings,
    FinishReport, OUTPUT, OUTPUT_2, OUTPUT_3, OUTPUT_4, OutputPartitions, Partitions, Port,
    Progress, RENDER, REPLAY_AUDIO_ENCODER, REPLAY_RING, REPLAY_VIDEO_ENCODER, RecordAdapters,
    RecordError, RecordSettings, Recording, SourceProgress, SourceState, StartFailure, StartReport,
    VIDEO_ENCODER, VIDEO_ENCODER_2, VIDEO_ENCODER_3, VIDEO_ENCODER_4, VideoSettings,
};
pub use select::{Chosen, scheme_of};
pub use studio::{
    END_TAIL_LIMIT, ExtraCanvas, FILTER_UNAVAILABLE, FilterSettingsAdjusted, Link, MAX_CANVASES,
    MAX_LAYERS, MAX_OUTPUTS, MAX_OUTPUTS_PER_CANVAS, MonitorState, OutputId, OutputSettings,
    ReplaySettings, Studio, StudioSettings, Updated, VirtualCameraSettings, VirtualCameraState,
};
