//! Records one canvas of a scene collection to a file (spec `docs/specs/recording.md`,
//! ADR-0040).
//!
//! [`Recording::start`] binds the collection with the scene binder and the source type
//! registry, builds the compositor and the mixer, attaches a video export and encoder
//! when the recording has a picture, an audio encoder when it has sound, and a muxed
//! output, adds the partitions to the engine and applies:
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

pub mod calibration;
mod clip;
pub mod estimate;
mod iso;
mod media_span;
mod png_writer;
pub mod protection;
mod recording;
mod select;
mod studio;
pub mod sync_measure;
mod timed_show;

pub use clip::{CLIP_WRITE_TIMEOUT, ClipFile, ClipWriteError, ClipWriter, write_clip};
pub use iso::{
    IsoError, IsoPlan, IsoRole, IsoTrack, MASTER_STEM, MAX_ISO_AUDIO, MAX_ISO_IMAGES,
    MAX_ISO_VIDEO, manifest_at_start, plan, write_manifest,
};
pub use media_span::MediaSpan;
pub use png_writer::{ImageFiles, PNG_WRITER_NODE_TYPE, PngWriterNode};
/// The recording manifest model (ADR-0086, ADR-0089), owned by `rbs-manifest`.
pub use rbs_manifest::{ManifestFile, RecordingManifest, SourceRef};
pub use rbs_nodes::replay::{Clip, ClipBounds, ClipError};
pub use rbs_nodes::replay_node::{ReplayStatus, RequestError as ReplayRequestError};
pub use rbs_nodes::{MEDIA_PAUSE_RELEASE, MediaControl, MediaNodeError, MediaPlayState};
pub use recording::{
    AUDIO, AUDIO_ENCODER, AUDIO_ENCODER_2, AUDIO_ENCODER_3, AUDIO_ENCODER_4, AUDIO_ENCODER_POOL,
    AudioSettings, FinishReport, ISO_AUDIO_ENCODER, ISO_OUTPUT, ISO_VIDEO_ENCODERS, OUTPUT,
    OUTPUT_2, OUTPUT_3, OUTPUT_4, OUTPUT_POOL, OutputPartitions, PartitionPool, Partitions, Port,
    Progress, RENDER, REPLAY_AUDIO_ENCODER, REPLAY_RING, REPLAY_VIDEO_ENCODER, RecordAdapters,
    RecordError, RecordSettings, Recording, SourceProgress, SourceState, StartFailure, StartReport,
    VIDEO_ENCODER, VIDEO_ENCODER_2, VIDEO_ENCODER_3, VIDEO_ENCODER_4, VIDEO_ENCODER_POOL,
    VideoSettings,
};
pub use select::{Chosen, scheme_of};
pub use studio::{
    AudioLatency, DEFAULT_GIVE_WAY_SHARE, END_TAIL_LIMIT, ExtraCanvas, FILTER_UNAVAILABLE,
    FilterSettingsAdjusted, HOLD_SCENE_TIMEOUT, HoldFallback, HoldRefused, HoldSound, HoldState,
    IsoAudioStart, IsoAudioStarted, IsoImageTrack, IsoSkip, IsoSkipped, IsoVideoStart,
    IsoVideoStarted, IsoVideoTrack, Link, MAX_CANVASES, MAX_CHAINS, MAX_CHAINS_PER_CANVAS,
    MAX_LAYERS, MAX_OUTPUTS, MAX_OUTPUTS_PER_CANVAS, ManualStep, MonitorState, OutputId,
    OutputSettings, PauseRefused, PauseState, PictureId, PlannedOutput, READY_WAIT, ReplaySettings,
    SharedPause, SourceSettingsAdjusted, StartPlanRequest, Studio, StudioSettings, TEXT_WAIT_TICKS,
    TextDue, TextFonts, TextTimer, TimerAction, TimerRefused, TransitionEvent, TransitionOnce,
    TransitionPhase, TransitionState, Updated, VirtualCameraSettings, VirtualCameraState,
    compensate_filter_latency,
};
pub use studio::{
    MEASURE_CLAPS, MEASURE_WINDOW, SyncMeasureFailure, SyncMeasureRefused, SyncMeasureResult,
    SyncMeasureSignals, SyncMeasureState, SyncMeasureStatus,
};
pub use studio::{STINGER_WAIT, ScrollHandover, StingerFacts};
