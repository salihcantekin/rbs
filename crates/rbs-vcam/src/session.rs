use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use rbs_core::capability::CapabilitySet;
use rbs_core::media::{MemoryKind, PixelFormat, VideoFrame};
use rbs_core::timing::FrameRate;

use crate::error::VcamError;
use crate::handoff::SlotToken;

/// Whether this computer has the camera (spec, "Installation").
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstallState {
    /// This computer cannot have the camera; `reason` is shown to the user as is.
    Unsupported {
        /// Why, in words for the user.
        reason: String,
    },
    /// Not added.
    NotInstalled,
    /// The installed consumer side has another layout version than this app.
    NeedsUpdate {
        /// Why, in words for the user.
        reason: String,
    },
    /// Added: apps can list the camera.
    Installed,
}

/// The one format the camera announces to apps (ADR-0066 decision 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CameraFormat {
    /// Width in pixels, even.
    pub width: u32,
    /// Height in pixels, even.
    pub height: u32,
    /// Frames per second announced.
    pub rate: FrameRate,
    /// Pixel format (NV12).
    pub pixel: PixelFormat,
}

impl CameraFormat {
    /// A format; checked by the factory and the session against the capabilities.
    pub fn new(width: u32, height: u32, rate: FrameRate, pixel: PixelFormat) -> Self {
        Self {
            width,
            height,
            rate,
            pixel,
        }
    }
}

/// What a session sends: the announced format and the memory kind frames come in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct VirtualCameraConfig {
    /// The announced format; every frame has its size and pixel format.
    pub format: CameraFormat,
    /// One of the adapter's `vcam.input.memory`.
    pub memory: MemoryKind,
}

impl VirtualCameraConfig {
    /// A configuration.
    pub fn new(format: CameraFormat, memory: MemoryKind) -> Self {
        Self { format, memory }
    }
}

/// An OS virtual camera mechanism. Every call but `capabilities` may block and runs on
/// the control thread, never the UI thread or the render thread.
pub trait VirtualCameraFactory: Send + Sync {
    /// Stable adapter id, for example `org.rbs.vcam.mf`.
    fn id(&self) -> &'static str;
    /// Static capabilities; no OS call.
    fn capabilities(&self) -> &CapabilitySet;
    /// A cheap probe: no thread, no prompt.
    fn status(&self) -> Result<InstallState, VcamError>;
    /// Adds the camera. May show OS prompts and block until they are answered.
    /// Idempotent.
    fn install(&self) -> Result<InstallState, VcamError>;
    /// Removes the camera. Idempotent.
    fn uninstall(&self) -> Result<InstallState, VcamError>;
    /// The one format the camera announces. Applies when no app reads the camera:
    /// `Ok(false)` means it is pending.
    fn set_format(&self, format: &CameraFormat) -> Result<bool, VcamError>;
    /// Opens a sending session; `NotInstalled` unless the camera is added.
    fn open(
        &self,
        config: &VirtualCameraConfig,
    ) -> Result<Box<dyn VirtualCameraSession>, VcamError>;
}

/// A sending session. `reserve` and `send` run on the render thread: they never wait,
/// never allocate in steady state and make at most one system call (the sender
/// thread's wake). Dropping the session stops and joins its sender thread; drop it on
/// the control thread.
pub trait VirtualCameraSession: Send {
    /// Reserves a slot for a frame about to be produced; `None` (counted as
    /// `consumer_busy`) means skip this camera frame before any GPU work.
    fn reserve(&self) -> Option<SlotToken>;
    /// Hands `frame` over in the reserved slot. Refuses a frame that breaks the
    /// configuration (`InvalidFrame`); the slot is freed and the frame dropped.
    fn send(&self, slot: SlotToken, frame: VideoFrame) -> Result<(), VcamError>;
    /// The counters.
    fn stats(&self) -> VcamStats;
    /// The counters the session shares with its sender thread, for a reader that keeps
    /// them after the session moved into a graph node (a status line in a UI or the
    /// CLI). Read them with [`VcamCounters::snapshot`]. `None` when the adapter keeps
    /// no shared counters.
    fn counters(&self) -> Option<Arc<VcamCounters>> {
        None
    }
}

/// Counters a session and its sender thread share (atomics, no lock).
#[derive(Debug, Default)]
pub struct VcamCounters {
    pub(crate) sent: AtomicU64,
    pub(crate) replaced: AtomicU64,
    pub(crate) refused: AtomicU64,
    pub(crate) consumer_busy: AtomicU64,
    pub(crate) consumer_absent: AtomicU64,
    pub(crate) format_pending: AtomicU64,
    pub(crate) os_errors: AtomicU64,
    pub(crate) last_os_error: AtomicU32,
}

impl VcamCounters {
    /// A frame reached the consumer side.
    pub fn count_sent(&self) {
        self.sent.fetch_add(1, Ordering::Relaxed);
    }

    /// No room on the consumer side (a full ring) for a taken frame.
    pub fn count_consumer_busy(&self) {
        self.consumer_busy.fetch_add(1, Ordering::Relaxed);
    }

    /// A frame taken while no app reads the camera.
    pub fn count_consumer_absent(&self) {
        self.consumer_absent.fetch_add(1, Ordering::Relaxed);
    }

    /// A frame taken while a format change waits for apps to close the camera.
    pub fn count_format_pending(&self) {
        self.format_pending.fetch_add(1, Ordering::Relaxed);
    }

    /// An OS call failed with `code` (stored as a non-zero 32-bit code).
    pub fn count_os_error(&self, code: u32) {
        self.os_errors.fetch_add(1, Ordering::Relaxed);
        self.last_os_error.store(code.max(1), Ordering::Relaxed);
    }

    /// A snapshot.
    pub fn snapshot(&self) -> VcamStats {
        VcamStats {
            sent: self.sent.load(Ordering::Relaxed),
            replaced: self.replaced.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            consumer_busy: self.consumer_busy.load(Ordering::Relaxed),
            consumer_absent: self.consumer_absent.load(Ordering::Relaxed),
            format_pending: self.format_pending.load(Ordering::Relaxed),
            os_errors: self.os_errors.load(Ordering::Relaxed),
            last_os_error: self.last_os_error.load(Ordering::Relaxed),
        }
    }
}

/// A snapshot of a session's counters. New counters are added without breaking callers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct VcamStats {
    sent: u64,
    replaced: u64,
    refused: u64,
    consumer_busy: u64,
    consumer_absent: u64,
    format_pending: u64,
    os_errors: u64,
    last_os_error: u32,
}

impl VcamStats {
    /// Frames handed to the consumer side.
    pub fn sent(&self) -> u64 {
        self.sent
    }
    /// Frames a newer frame replaced before the sender took them (dropped by the sender).
    pub fn replaced(&self) -> u64 {
        self.replaced
    }
    /// Frames `send` refused.
    pub fn refused(&self) -> u64 {
        self.refused
    }
    /// Camera frames skipped for lack of a free slot, or dropped for lack of room on
    /// the consumer side.
    pub fn consumer_busy(&self) -> u64 {
        self.consumer_busy
    }
    /// Frames taken while no app read the camera.
    pub fn consumer_absent(&self) -> u64 {
        self.consumer_absent
    }
    /// Frames sent while a format change waited for apps to close the camera.
    pub fn format_pending(&self) -> u64 {
        self.format_pending
    }
    /// Failed OS calls.
    pub fn os_errors(&self) -> u64 {
        self.os_errors
    }
    /// The last OS error code, if any.
    pub fn last_os_error(&self) -> Option<u32> {
        (self.last_os_error != 0).then_some(self.last_os_error)
    }
}
