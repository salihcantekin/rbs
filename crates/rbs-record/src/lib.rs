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

mod recording;
mod select;
mod studio;

pub use recording::{
    AUDIO, AUDIO_ENCODER, AUDIO_ENCODER_2, AudioSettings, FinishReport, OUTPUT, OUTPUT_2,
    OutputPartitions, Partitions, Port, Progress, RENDER, REPLAY_AUDIO_ENCODER, REPLAY_RING,
    REPLAY_VIDEO_ENCODER, RecordAdapters, RecordError, RecordSettings, Recording, SourceProgress,
    SourceState, StartFailure, StartReport, VIDEO_ENCODER, VIDEO_ENCODER_2, VideoSettings,
};
pub use select::{Chosen, scheme_of};
pub use studio::{
    END_TAIL_LIMIT, FILTER_UNAVAILABLE, FilterSettingsAdjusted, Link, MAX_LAYERS, MAX_OUTPUTS,
    OutputId, OutputSettings, ReplaySettings, Studio, StudioSettings, Updated,
    VirtualCameraSettings, VirtualCameraState,
};
