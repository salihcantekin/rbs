//! The capture source node: a capture session's frames on a video link, on the render
//! partition (spec `docs/specs/io-nodes.md`, "Capture source node").

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use rbs_capture::{
    CaptureError, CaptureFactory, CaptureRequest, CaptureSession, GpuDeviceId, GpuImportTarget,
    SyncKind,
};
use rbs_core::capability::CapabilityValue;
use rbs_core::fault::SourceFault;
use rbs_core::media::{FramePayload, MemoryKind, PixelFormat, Size};
use rbs_gpu::context::CAP_P010;
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::upload::{SourceUpload, UPLOAD_FORMATS};
use rbs_gpu::{GpuContext, GpuError};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, WarmCtx};

/// Node type id of capture sources (displays, windows and cameras).
pub const CAPTURE_NODE_TYPE: &str = "org.rbs.capture";
/// Index of the "video" output pin.
pub const VIDEO_PIN: usize = 0;

/// The capture source's descriptor: kind `Source`, output "video".
pub fn capture_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(CAPTURE_NODE_TYPE),
        NodeKind::Source,
        Vec::new(),
        vec![Pin::one("video", LinkKind::Video)],
    )
}

/// Counters of a capture source node, readable from any thread. Made by the node only
/// (`non_exhaustive`, so fields can be added).
#[non_exhaustive]
#[derive(Debug, Default)]
pub struct CaptureNodeStats {
    /// Frames put on the video output.
    pub frames: AtomicU64,
    /// Native frames imported zero-copy.
    pub imported: AtomicU64,
    /// System-memory frames uploaded.
    pub uploaded: AtomicU64,
    /// Frames lost to an import or upload error.
    pub gpu_errors: AtomicU64,
    /// Size of the picture put on the output: width in the high 32 bits, height in the
    /// low 32 bits, 0 while unknown (ADR-0058). Read it with [`Self::picture_size`].
    pub picture_size: AtomicU64,
    /// The cause of the node's last failure, as [`SourceFault::code`]; 0 while it runs
    /// (ADR-0077). Read it with [`Self::fault`].
    pub fault: AtomicU8,
}

impl CaptureNodeStats {
    /// Why the node last failed, until it runs again.
    pub fn fault(&self) -> Option<SourceFault> {
        SourceFault::from_code(self.fault.load(Ordering::Relaxed))
    }

    /// Width and height in pixels of the picture the node delivers: the session's
    /// format once it is open, then the size of the newest frame. `None` before the
    /// session opened.
    pub fn picture_size(&self) -> Option<(u32, u32)> {
        let packed = self.picture_size.load(Ordering::Relaxed);
        let (w, h) = ((packed >> 32) as u32, packed as u32);
        (w > 0 && h > 0).then_some((w, h))
    }
}

/// `size` packed as [`CaptureNodeStats::picture_size`] stores it.
fn pack(size: Size) -> u64 {
    (u64::from(size.width) << 32) | u64::from(size.height)
}

fn add(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// What this render device can take from capture adapters: the memory kinds the
/// importer reads natively, best first, then `System` (uploaded), and the import target
/// those native kinds need. Decided by the device's graphics API, never by vendor.
///
/// - D3D12: shared D3D11/D3D12 handles on the render adapter (`adapter_luid`), with
///   a shared timeline fence or `Ready`.
/// - Metal: IOSurfaces (`Ready`).
/// - Vulkan: system memory only until DMA-BUF import is verified on hardware.
pub fn import_preferences(ctx: &GpuContext) -> (Vec<MemoryKind>, Option<GpuImportTarget>) {
    match ctx.adapter().get_info().backend {
        wgpu::Backend::Dx12 => match ctx.adapter_luid() {
            Some(luid) => (
                vec![MemoryKind::D3d11, MemoryKind::D3d12, MemoryKind::System],
                Some(GpuImportTarget::new(
                    GpuDeviceId::Luid(luid.to_le_bytes()),
                    &[SyncKind::TimelineFence, SyncKind::Ready],
                )),
            ),
            None => (vec![MemoryKind::System], None),
        },
        wgpu::Backend::Metal => (
            vec![MemoryKind::Metal, MemoryKind::System],
            Some(GpuImportTarget::new(
                GpuDeviceId::MetalRegistryId(0),
                &[SyncKind::Ready],
            )),
        ),
        _ => (vec![MemoryKind::System], None),
    }
}

/// Fills what `request` leaves open from what `ctx` can take: memory kinds, the GPU
/// import target, and the pixel formats the upload path reads (the importer accepts
/// the same ones). Values the request already has are kept.
pub fn complete_request(ctx: &GpuContext, mut request: CaptureRequest) -> CaptureRequest {
    let (memory, gpu) = import_preferences(ctx);
    if request.memory.is_empty() {
        request = request.with_memory(&memory);
    }
    if request.gpu.is_none()
        && let Some(gpu) = gpu
    {
        request = request.with_gpu(gpu);
    }
    if request.pixel_formats.is_empty() {
        // P010 planes are 16-bit normalized textures (`gpu.p010`).
        let p010 = ctx.capabilities().get(&CAP_P010) == Some(&CapabilityValue::Flag(true));
        let formats: Vec<PixelFormat> = UPLOAD_FORMATS
            .iter()
            .copied()
            .filter(|f| p010 || *f != PixelFormat::P010)
            .collect();
        request = request.with_pixel_formats(&formats);
    }
    request
}

/// Opens a capture session when it joins the schedule and puts its newest frame on the
/// "video" output each render tick: native surfaces through the render runtime's
/// importer (zero copy), system-memory frames through a [`SourceUpload`].
pub struct CaptureSourceNode {
    descriptor: NodeDescriptor,
    factory: Arc<dyn CaptureFactory>,
    request: CaptureRequest,
    session: Option<Box<dyn CaptureSession>>,
    upload: SourceUpload,
    stats: Arc<CaptureNodeStats>,
    /// The size last stored in `stats.picture_size`, packed.
    shown_size: u64,
}

impl CaptureSourceNode {
    /// A source for `request` from `factory`. Holds nothing until `prepare`; memory kinds,
    /// GPU target and pixel formats the request leaves empty are filled from the render
    /// device in `prepare` ([`complete_request`]).
    pub fn new(factory: Arc<dyn CaptureFactory>, request: CaptureRequest) -> Self {
        Self {
            descriptor: capture_descriptor(),
            factory,
            request,
            session: None,
            upload: SourceUpload::new(),
            stats: Arc::default(),
            shown_size: 0,
        }
    }

    /// The node's counters.
    pub fn stats(&self) -> Arc<CaptureNodeStats> {
        Arc::clone(&self.stats)
    }
}

/// The fault a capture error stands for (ADR-0077; capture port spec).
pub fn capture_fault(e: &CaptureError) -> SourceFault {
    match e {
        CaptureError::TargetNotFound | CaptureError::TargetLost => SourceFault::TargetGone,
        CaptureError::PermissionDenied | CaptureError::PermissionNotDetermined => {
            SourceFault::PermissionDenied
        }
        CaptureError::DeviceBusy => SourceFault::Busy,
        _ => SourceFault::Other,
    }
}

fn capture_error(e: &CaptureError) -> NodeError {
    match e {
        CaptureError::PermissionDenied => NodeError::Permanent("capture permission denied"),
        CaptureError::PermissionNotDetermined => {
            NodeError::Permanent("capture permission not granted yet")
        }
        CaptureError::FormatUnsupported => NodeError::Permanent("capture format unsupported"),
        CaptureError::TargetNotFound => NodeError::Permanent("capture target not found"),
        e if e.is_recoverable() => NodeError::Backend("capture failed"),
        _ => NodeError::Permanent("capture failed"),
    }
}

impl Node<GpuRuntime> for CaptureSourceNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let request = complete_request(ctx.shared, self.request.clone());
        self.stats.fault.store(0, Ordering::Relaxed);
        let mut session = self
            .factory
            .open(&request)
            .map_err(|e| {
                self.stats
                    .fault
                    .store(capture_fault(&e).code(), Ordering::Relaxed);
                e
            })
            .map_err(|e| match e {
                e if !e.is_recoverable() => capture_error(&e),
                e => NodeError::Failed(format!("capture open: {e}")),
            })?;
        // `start` may block briefly; this is the control thread (capture port spec).
        session
            .start()
            .map_err(|e| {
                self.stats
                    .fault
                    .store(capture_fault(&e).code(), Ordering::Relaxed);
                e
            })
            .map_err(|e| match e {
                e if !e.is_recoverable() => capture_error(&e),
                e => NodeError::Failed(format!("capture start: {e}")),
            })?;
        self.shown_size = pack(session.format().size);
        self.stats
            .picture_size
            .store(self.shown_size, Ordering::Relaxed);
        self.session = Some(session);
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let session = self.session.as_mut().ok_or(NodeError::NotReady)?;
        let frame = match session.latest_frame() {
            // No new frame: the slot keeps the last one.
            Ok(None) => return Ok(()),
            Ok(Some(frame)) => frame,
            Err(e) => {
                // An error path, not a frame: one store.
                self.stats
                    .fault
                    .store(capture_fault(&e).code(), Ordering::Relaxed);
                return Err(capture_error(&e));
            }
        };
        // A resized target: one comparison per new frame, one store per resize.
        let size = pack(frame.format().size);
        if size != self.shown_size {
            self.shown_size = size;
            self.stats.picture_size.store(size, Ordering::Relaxed);
        }
        let gpu = ctx.shared();
        let (_, outputs, local) = ctx.parts();
        let texture = match frame.payload() {
            FramePayload::Native { .. } => {
                let imported = local.importer.import(gpu, &mut local.pool, frame);
                if imported.is_ok() {
                    add(&self.stats.imported);
                }
                imported
            }
            FramePayload::System(_) => {
                let uploaded = self.upload.upload(gpu, &mut local.pool, frame);
                if uploaded.is_ok() {
                    add(&self.stats.uploaded);
                }
                uploaded
            }
            _ => Ok(frame),
        };
        let texture = match texture {
            Ok(t) => t,
            Err(e) => {
                // The frame (and the producer's lease) is dropped; the last frame stays.
                add(&self.stats.gpu_errors);
                return Err(match e {
                    // The source sends what this device cannot take; a restart would
                    // get the same frames.
                    GpuError::Unsupported(_) | GpuError::InvalidSize { .. } => {
                        NodeError::Permanent("capture frame format unsupported by the GPU")
                    }
                    _ => NodeError::Backend("capture frame import failed"),
                });
            }
        };
        // Dropping the previous frame only releases its lease.
        drop(outputs.set_video(VIDEO_PIN, Some(texture))?);
        add(&self.stats.frames);
        Ok(())
    }

    fn cool(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) {
        self.upload.release(&mut ctx.local.pool);
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        if let Some(mut session) = self.session.take() {
            session.stop();
        }
        self.shown_size = 0;
        self.stats.picture_size.store(0, Ordering::Relaxed);
        // The decoder's pipelines and bind groups go too: nothing GPU-side in Standby
        // (P13). `cool` already returned the plane textures.
        self.upload = SourceUpload::new();
    }
}

#[cfg(test)]
mod fault_tests {
    use super::*;

    #[test]
    fn capture_errors_map_to_faults() {
        assert_eq!(
            capture_fault(&CaptureError::TargetNotFound),
            SourceFault::TargetGone
        );
        assert_eq!(
            capture_fault(&CaptureError::TargetLost),
            SourceFault::TargetGone
        );
        assert_eq!(
            capture_fault(&CaptureError::PermissionDenied),
            SourceFault::PermissionDenied
        );
        assert_eq!(
            capture_fault(&CaptureError::PermissionNotDetermined),
            SourceFault::PermissionDenied
        );
        assert_eq!(capture_fault(&CaptureError::DeviceBusy), SourceFault::Busy);
        assert_eq!(
            capture_fault(&CaptureError::FormatUnsupported),
            SourceFault::Other
        );
        let stats = CaptureNodeStats::default();
        assert_eq!(stats.fault(), None);
        stats
            .fault
            .store(SourceFault::Busy.code(), Ordering::Relaxed);
        assert_eq!(stats.fault(), Some(SourceFault::Busy));
    }
}
