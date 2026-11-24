//! Backends and sessions.

use rbs_core::media::{Size, VideoFrame};

use crate::error::SegmentError;
use crate::info::{BackendInfo, LevelId};
use crate::matte::Matte;

/// What a host lends a backend. Adapters get only what a plugin could also be given
/// (ADR-0162, rule 19): the model files and a log sink. GPU access is added here by the
/// first backend that needs it, as a handle type with no GPU-library types in this
/// crate.
pub trait SegmentHost: Send + Sync {
    /// The bytes of a model the host stores, or `None` when it has none by this name.
    /// Hosts check size and hash before returning a model (spec rule 10).
    fn model(&self, name: &str) -> Option<ModelRef>;
}

/// A checked model the host hands to `open`. A path from a scene document is never a
/// model reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    /// The model's name as the host knows it.
    pub name: String,
    /// The checked bytes.
    pub bytes: std::sync::Arc<[u8]>,
}

/// What `open` is asked for.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct SessionRequest {
    /// The quality level.
    pub level: LevelId,
    /// The size of the first frame the session will get.
    pub input_size: Size,
    /// The model, when the backend needs one.
    pub model: Option<ModelRef>,
}

impl SessionRequest {
    /// Creates a request without a model.
    pub fn new(level: LevelId, input_size: Size) -> Self {
        Self {
            level,
            input_size,
            model: None,
        }
    }

    /// Adds a model.
    pub fn with_model(mut self, model: ModelRef) -> Self {
        self.model = Some(model);
        self
    }
}

/// What `submit` did with a frame.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// The frame is being worked on.
    Accepted,
    /// The session was busy and dropped the frame; the previous matte stays in use.
    DroppedBusy,
    /// `VideoFrame::share` returned `None`, so the session did not retain the frame.
    NotShareable,
}

/// A way of producing mattes; a registry entry (ADR-0162).
pub trait PersonMaskBackend: Send + Sync {
    /// The backend's type id, such as `org.rbs.segment.apple-vision`.
    fn type_id(&self) -> &'static str;

    /// Cheap: allocates no GPU or model resources and starts no thread. Is this backend
    /// usable on this computer, and what does it offer? Control thread.
    fn probe(&self, host: &dyn SegmentHost) -> BackendInfo;

    /// Creates a session and loads the model, the only expensive call (principle P13).
    /// Control thread, never the render thread.
    ///
    /// # Errors
    /// `Unavailable`, `ModelMissing`, `ModelLoadFailed`, `Unsupported`.
    fn open(&self, request: SessionRequest) -> Result<Box<dyn PersonMaskSession>, SegmentError>;
}

/// One running segmentation of one source.
pub trait PersonMaskSession: Send {
    /// Control thread, once after `open`: allocates every buffer, texture and worker the
    /// session will use at the request's input size, so `submit` and `latest` allocate
    /// nothing.
    ///
    /// # Errors
    /// `DeviceLost`, `ModelLoadFailed`.
    fn warm(&mut self) -> Result<(), SegmentError>;

    /// Render thread, non-blocking: hands over a frame. The session may drop it when it
    /// is busy (the newest frame wins) and never queues more than one pending frame.
    fn submit(&mut self, frame: &VideoFrame) -> SubmitOutcome;

    /// Render thread, non-blocking: each finished matte is returned once; `Ok(None)`
    /// while there is no newer one.
    ///
    /// # Errors
    /// The reason the session can no longer produce mattes, such as `DeviceLost`.
    fn latest(&mut self) -> Result<Option<Matte>, SegmentError>;

    /// Render thread: a cut, gap or source restart; a temporal backend forgets its state.
    fn reset(&mut self);

    /// Control thread: stops the worker, releases leases and device handles.
    fn close(self: Box<Self>);
}
