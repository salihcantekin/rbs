//! Capture sessions and the factory trait adapters implement.

use std::fmt;
use std::sync::Arc;

use rbs_core::media::{MemoryKind, PixelFormat, Size, VideoFormat, VideoFrame};
use rbs_core::registry::AdapterFactory;
use rbs_core::time::Clock;
use rbs_core::timing::FrameRate;

use crate::error::CaptureError;
use crate::target::{CaptureTarget, TargetKind, TargetSelector};

/// What the consumer asks of a capture session.
#[derive(Clone)]
pub struct CaptureRequest {
    /// Which target to capture.
    pub target: TargetSelector,
    /// Pixel formats the consumer can use, best first. The adapter picks the first one
    /// it can deliver natively, or fails with [`CaptureError::FormatUnsupported`].
    pub pixel_formats: Vec<PixelFormat>,
    /// Memory kinds the consumer can import, best first (for example
    /// `[D3d11, System]`). Same selection rule as `pixel_formats`.
    pub memory: Vec<MemoryKind>,
    /// Desired frame rate; `None` means the target's native rate. Displays and windows
    /// treat it as an upper bound; cameras pick the closest mode.
    pub frame_rate: Option<FrameRate>,
    /// Requested camera size; ignored for displays and windows.
    pub size: Option<Size>,
    /// Draw the cursor into frames (displays and windows only).
    pub cursor: bool,
    /// The master clock frames are stamped with (ADR-0004).
    pub clock: Arc<dyn Clock>,
}

impl CaptureRequest {
    /// A request for `target` with no format or memory preference yet, no cursor and
    /// the native frame rate.
    pub fn new(target: TargetSelector, clock: Arc<dyn Clock>) -> Self {
        Self {
            target,
            pixel_formats: Vec::new(),
            memory: Vec::new(),
            frame_rate: None,
            size: None,
            cursor: false,
            clock,
        }
    }

    /// Sets the acceptable pixel formats, best first.
    pub fn with_pixel_formats(mut self, formats: &[PixelFormat]) -> Self {
        self.pixel_formats = formats.to_vec();
        self
    }

    /// Sets the acceptable memory kinds, best first.
    pub fn with_memory(mut self, memory: &[MemoryKind]) -> Self {
        self.memory = memory.to_vec();
        self
    }

    /// Sets the desired frame rate.
    pub fn with_frame_rate(mut self, rate: FrameRate) -> Self {
        self.frame_rate = Some(rate);
        self
    }

    /// Sets the requested camera size.
    pub fn with_size(mut self, size: Size) -> Self {
        self.size = Some(size);
        self
    }

    /// Enables or disables the cursor.
    pub fn with_cursor(mut self, cursor: bool) -> Self {
        self.cursor = cursor;
        self
    }
}

impl fmt::Debug for CaptureRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureRequest")
            .field("target", &self.target)
            .field("pixel_formats", &self.pixel_formats)
            .field("memory", &self.memory)
            .field("frame_rate", &self.frame_rate)
            .field("size", &self.size)
            .field("cursor", &self.cursor)
            .finish_non_exhaustive()
    }
}

/// Counters of a capture session, for diagnostics and the conformance suite.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureStats {
    /// Frames returned by [`CaptureSession::latest_frame`].
    pub delivered: u64,
    /// Frames produced but replaced by a newer one before delivery, or not produced
    /// because the consumer still held every surface.
    pub dropped: u64,
    /// Frames copied to system memory because zero-copy was not possible (P4).
    pub cpu_copied: u64,
}

/// One capture of one target.
///
/// Created in Standby by [`CaptureFactory::open`]; [`start`](Self::start) makes it
/// Active and [`stop`](Self::stop) returns it to Standby (ADR-0013). Dropping the
/// session releases everything it holds. Driven from one thread at a time.
pub trait CaptureSession: Send {
    /// The negotiated output format. Changes only when the target is resized; the
    /// first frame with the new size carries the new format.
    fn format(&self) -> VideoFormat;

    /// Standby -> Active: starts the OS capture stream.
    ///
    /// May allocate, start threads and block briefly (at most 500 ms). Called during
    /// warm-up, never from the render tick. Calling it on an Active session is a no-op.
    fn start(&mut self) -> Result<(), CaptureError>;

    /// Active -> Standby: stops the stream and releases the threads, pools and GPU
    /// resources `start` acquired. Idempotent. Frames still held by consumers stay
    /// valid until they are dropped.
    fn stop(&mut self);

    /// Returns the newest frame produced since the previous call, or `None`.
    ///
    /// **Real-time**: called from the render thread on every tick. Must not allocate,
    /// block, do I/O, log or take a lock the capture thread can hold for longer than a
    /// pointer swap. Older undelivered frames are released and counted as dropped.
    /// Returns `Ok(None)` while in Standby.
    fn latest_frame(&mut self) -> Result<Option<VideoFrame>, CaptureError>;

    /// Session counters. Cheap enough for every tick.
    fn stats(&self) -> CaptureStats;
}

/// The factory every capture adapter implements.
pub trait CaptureFactory: AdapterFactory {
    /// Lists the targets of `kind` available now.
    ///
    /// Not real-time; may take up to 100 ms. Returns
    /// [`CaptureError::Unsupported`] for kinds the adapter cannot list (for example
    /// kinds offered only through the OS picker). Keeps no resources afterwards.
    fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError>;

    /// Opens a session for one target, in Standby.
    ///
    /// The session owns no threads, OS capture streams or GPU resources until
    /// [`CaptureSession::start`]. The adapter may validate the target and negotiate the
    /// format here.
    fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError>;
}
