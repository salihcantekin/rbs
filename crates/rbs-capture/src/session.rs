//! Capture sessions and the factory trait adapters implement.

use std::fmt;
use std::sync::Arc;

use rbs_core::media::{MemoryKind, PixelFormat, Size, VideoFormat, VideoFrame};
use rbs_core::registry::AdapterFactory;
use rbs_core::time::Clock;
use rbs_core::timing::FrameRate;

use crate::error::CaptureError;
use crate::target::{CaptureTarget, Permission, TargetId, TargetKind};

/// A GPU device, identified the way OS interop APIs identify it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GpuDeviceId {
    /// A Windows adapter LUID (DXGI, D3D11, D3D12, Vulkan `deviceLUID`).
    Luid([u8; 8]),
    /// A Vulkan or CUDA device UUID.
    Uuid([u8; 16]),
    /// A Linux DRM render node, as `(major, minor)` device numbers.
    DrmRenderNode {
        /// Device major number.
        major: u32,
        /// Device minor number.
        minor: u32,
    },
    /// A macOS Metal device registry id.
    MetalRegistryId(u64),
}

/// A synchronisation primitive the consumer can wait on before reading a native
/// surface. Mirrors the variants of `rbs_core::media::SurfaceSync`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncKind {
    /// The surface is ready when delivered; no wait needed.
    Ready,
    /// A DXGI keyed mutex.
    KeyedMutex,
    /// A shared timeline fence or semaphore.
    TimelineFence,
    /// A Linux sync file.
    SyncFile,
}

/// Where native GPU frames will be imported, so the adapter can produce surfaces that
/// device can open without a copy (principle P4). Filled in by `rbs-gpu`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuImportTarget {
    /// The importing device. Native surfaces must be shareable with it (same adapter).
    pub device: GpuDeviceId,
    /// DMA-BUF `(fourcc, modifier)` pairs the importer accepts. Empty means linear
    /// layout only.
    pub dmabuf_modifiers: Vec<(u32, u64)>,
    /// Synchronisation primitives the importer can wait on, best first.
    pub sync: Vec<SyncKind>,
}

impl GpuImportTarget {
    /// An import target for `device` that accepts linear DMA-BUFs and the given sync
    /// primitives.
    pub fn new(device: GpuDeviceId, sync: &[SyncKind]) -> Self {
        Self {
            device,
            dmabuf_modifiers: Vec::new(),
            sync: sync.to_vec(),
        }
    }

    /// Sets the accepted DMA-BUF `(fourcc, modifier)` pairs.
    pub fn with_dmabuf_modifiers(mut self, modifiers: &[(u32, u64)]) -> Self {
        self.dmabuf_modifiers = modifiers.to_vec();
        self
    }
}

/// What the consumer asks of a capture session.
#[non_exhaustive]
#[derive(Clone)]
pub struct CaptureRequest {
    /// Which target to capture, from `enumerate` or `pick`.
    pub target: TargetId,
    /// Pixel formats the consumer can use, best first. The adapter picks the first one
    /// it can deliver natively, or fails with [`CaptureError::FormatUnsupported`].
    pub pixel_formats: Vec<PixelFormat>,
    /// Memory kinds the consumer can import, best first (for example
    /// `[D3d11, System]`). Same selection rule as `pixel_formats`.
    pub memory: Vec<MemoryKind>,
    /// The device native GPU frames are imported into. Required for every GPU memory
    /// kind in `memory`; without it only `System` can be delivered.
    pub gpu: Option<GpuImportTarget>,
    /// Desired frame rate; `None` means the target's native rate. Displays and windows
    /// treat it as an upper bound; cameras pick the closest supported rate.
    pub frame_rate: Option<FrameRate>,
    /// Requested camera size; ignored for displays and windows.
    pub size: Option<Size>,
    /// Draw the cursor into frames (displays and windows only).
    pub cursor: bool,
    /// The master clock frames are stamped with (ADR-0004).
    pub clock: Arc<dyn Clock>,
}

impl CaptureRequest {
    /// A request for `target` with no format or memory preference yet, no GPU import
    /// target, no cursor and the native frame rate.
    pub fn new(target: TargetId, clock: Arc<dyn Clock>) -> Self {
        Self {
            target,
            pixel_formats: Vec::new(),
            memory: Vec::new(),
            gpu: None,
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

    /// Sets the GPU device frames are imported into.
    pub fn with_gpu(mut self, gpu: GpuImportTarget) -> Self {
        self.gpu = Some(gpu);
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
            .field("gpu", &self.gpu)
            .field("frame_rate", &self.frame_rate)
            .field("size", &self.size)
            .field("cursor", &self.cursor)
            .finish_non_exhaustive()
    }
}

/// A change to an open session.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureChange {
    /// Show or hide the cursor.
    Cursor(bool),
    /// Change the desired frame rate (same rules as [`CaptureRequest::frame_rate`]).
    FrameRate(FrameRate),
}

/// Counters of a capture session, for diagnostics and the conformance suite.
///
/// Non-exhaustive: adapters start from `CaptureStats::default()` and set fields.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureStats {
    /// Frames returned by [`CaptureSession::latest_frame`].
    pub delivered: u64,
    /// Frames received from the OS but never delivered: replaced by a newer one, or
    /// discarded because the consumer held every surface. Frames the OS drops
    /// internally are not visible to the adapter and not counted.
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
    /// free, block, do I/O, log or take any lock; the hand-off from the capture thread
    /// is lock-free (for example an atomic latest-slot swap). Replaced frames are
    /// counted as dropped; their OS objects are released on the producer's own thread,
    /// never inside this call. Returns `Ok(None)` while in Standby.
    fn latest_frame(&mut self) -> Result<Option<VideoFrame>, CaptureError>;

    /// Session counters. Cheap enough for every tick.
    fn stats(&self) -> CaptureStats;

    /// Applies a change while open. Not real-time. Adapters that cannot apply a change
    /// return [`CaptureError::Unsupported`]; they never ignore it silently.
    fn reconfigure(&mut self, change: CaptureChange) -> Result<(), CaptureError> {
        let _ = change;
        Err(CaptureError::Unsupported("reconfigure"))
    }
}

/// The factory every capture adapter implements.
///
/// Methods added in later minor versions come with a default implementation that
/// returns [`CaptureError::Unsupported`], so existing adapters keep compiling.
pub trait CaptureFactory: AdapterFactory {
    /// Lists the targets of `kind` available now.
    ///
    /// Not real-time; may take up to 100 ms. Never shows a permission prompt: returns
    /// [`CaptureError::PermissionNotDetermined`] or [`CaptureError::PermissionDenied`]
    /// instead. Returns [`CaptureError::Unsupported`] for kinds the adapter cannot list
    /// (for example kinds offered only through [`pick`](Self::pick)). Keeps no
    /// resources afterwards.
    fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError>;

    /// Opens a session for one target, in Standby.
    ///
    /// Never prompts and never activates the device: the session owns no threads, OS
    /// capture streams, device handles or GPU resources until
    /// [`CaptureSession::start`] (a camera's light stays off). The adapter validates
    /// the target and negotiates the format from what `enumerate` or `pick` reported.
    fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError>;

    /// Shows the operating system's picker for `kind` and returns the chosen target.
    ///
    /// Blocks until the user chooses or cancels (cancel returns
    /// [`CaptureError::PermissionDenied`]); call it from a control thread, never from
    /// the render loop. The returned id carries any OS restore token, so it can be
    /// stored in a scene and passed to `open` later without a new prompt.
    fn pick(&self, kind: TargetKind) -> Result<CaptureTarget, CaptureError> {
        let _ = kind;
        Err(CaptureError::Unsupported("pick"))
    }

    /// Whether capturing `kind` is currently allowed. Fast; never prompts.
    fn permission(&self, kind: TargetKind) -> Permission {
        let _ = kind;
        Permission::Granted
    }

    /// Asks the user for permission to capture `kind` if it is not determined yet.
    /// Blocks while the OS prompt is shown; call it from a control thread.
    fn request_permission(&self, kind: TargetKind) -> Result<Permission, CaptureError> {
        Ok(self.permission(kind))
    }
}
