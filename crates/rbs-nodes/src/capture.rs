//! The capture source node: a capture session's frames on a video link, on the render
//! partition (spec `docs/specs/io-nodes.md`, "Capture source node").

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_capture::{
    CaptureError, CaptureFactory, CaptureRequest, CaptureSession, GpuDeviceId, GpuImportTarget,
    SyncKind,
};
use rbs_core::capability::CapabilityValue;
use rbs_core::media::{FramePayload, MemoryKind, PixelFormat};
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

/// Counters of a capture source node, readable from any thread.
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
        }
    }

    /// The node's counters.
    pub fn stats(&self) -> Arc<CaptureNodeStats> {
        Arc::clone(&self.stats)
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
        let mut session = self.factory.open(&request).map_err(|e| match e {
            e if !e.is_recoverable() => capture_error(&e),
            e => NodeError::Failed(format!("capture open: {e}")),
        })?;
        // `start` may block briefly; this is the control thread (capture port spec).
        session.start().map_err(|e| match e {
            e if !e.is_recoverable() => capture_error(&e),
            e => NodeError::Failed(format!("capture start: {e}")),
        })?;
        self.session = Some(session);
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let session = self.session.as_mut().ok_or(NodeError::NotReady)?;
        let frame = match session.latest_frame() {
            // No new frame: the slot keeps the last one.
            Ok(None) => return Ok(()),
            Ok(Some(frame)) => frame,
            Err(e) => return Err(capture_error(&e)),
        };
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
        // The decoder's pipelines and bind groups go too: nothing GPU-side in Standby
        // (P13). `cool` already returned the plane textures.
        self.upload = SourceUpload::new();
    }
}
