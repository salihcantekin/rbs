//! Native buffer export for encoders on Windows (spec `docs/specs/gpu-compositor.md#native-export`,
//! ADR-0147 decisions 6 and 7).
//!
//! [`D3dBufferExporter`] owns a fixed ring of committed Direct3D 12 buffers in a shared
//! heap, each opened by an NT handle, and one shared timeline fence. A canvas is converted
//! to NV12 or P010 in two plane render targets and copied into a free ring buffer in the
//! same submission, laid out like the readback: the luma plane at offset 0, the
//! interleaved chroma plane right after `height` rows, both with the same 256-byte-aligned
//! stride. After the submission the queue signals the fence with the next value. The frame
//! is handed out at once with `SurfaceSync::TimelineFence`: the encoder waits for that
//! value on its own GPU queue (NVENC through a CUDA external semaphore), so the render
//! thread never waits for the GPU and no byte crosses to the CPU.
//!
//! The lease owner is the ring: it keeps every buffer, its NT handle and the fence (and its
//! handle) alive, so an exported frame stays valid whatever is dropped first. It also keeps
//! the wgpu device, so d3d12.dll stays loaded until those objects are released. The engine
//! keeps the owner until the encoder session is gone (ADR-0147 decision 8).
//!
//! This is a marked low-level module (principle P11): every `unsafe` block states its
//! invariant.
//!
//! Hot path: [`D3dBufferExporter::export`] and [`D3dBufferExporter::take_ready`] do not
//! allocate in RBS code. [`D3dBufferExporter::scaled_tone`] creates every buffer up front.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rbs_core::media::{
    ColorRange, ColorSpace, Lease, LeaseOwner, NativeSurface, PlaneLayout, Size, SurfaceSync,
    TextureId, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_FENCE_FLAG_SHARED, D3D12_HEAP_FLAG_SHARED, D3D12_HEAP_PROPERTIES,
    D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_BUFFER,
    D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COMMON, D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
    ID3D12Fence, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
use windows::core::PCWSTR;

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::native_export::{ExportStats, MAX_EXPORT_SURFACES};
use crate::pool::TexturePool;
use crate::yuv::{YuvConverter, YuvFormat};

type Dx12 = wgpu::hal::api::Dx12;

/// An NT handle this module created; closed on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle was returned by `CreateSharedHandle` to this process, is
            // owned by this value only and is closed exactly once, here. Importers that
            // opened it (CUDA, another device) hold their own references to the object.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

// SAFETY: a kernel handle value may be used and closed from any thread; this value only
// stores it and closes it once on drop.
unsafe impl Send for OwnedHandle {}
// SAFETY: shared references only read the handle value.
unsafe impl Sync for OwnedHandle {}

/// One ring entry: the shared buffer, its NT handle, and the count of leases on it.
struct Entry {
    // Kept alive for the handle's importers; the exporter writes it through `buffer`.
    _resource: ID3D12Resource,
    handle: OwnedHandle,
    count: AtomicU32,
}

/// The lease owner of exported frames: the ring entries and the shared fence. Shared with
/// encoder threads through leases.
struct Ring {
    entries: Box<[Entry]>,
    // Signalled by the render queue after each copy; kept alive while any frame may
    // still name its handle.
    fence: ID3D12Fence,
    fence_handle: OwnedHandle,
    // Declared last so it drops last: the D3D12 objects above are released through
    // d3d12.dll, which wgpu unloads when its last device and instance go away. Leases can
    // outlive the render device (the composition root drops lease owners on the control
    // thread), so the ring keeps the device, and with it the library, loaded until the
    // last lease is gone.
    _device: wgpu::Device,
}

// SAFETY: an entry's buffer is written by the exporter only while its count is zero
// (checked with `Acquire` in `D3dBufferExporter::export`, paired with the `Release`
// decrement here), and the encoder releases its lease only after its reads completed.
// `retain` is reachable only through a `Lease` clone, which already holds a reference, or
// from `take_ready` and `black` on the exporter's own thread, so a count never rises from
// zero behind the exporter's back. The ring owns the buffers, handles and fence, and every
// lease keeps the ring alive, so they stay valid for as long as any lease exists.
unsafe impl LeaseOwner for Ring {
    fn retain(&self, token: u64) {
        if let Some(e) = self.entries.get(token as usize) {
            // Relaxed: the caller already holds a reference, so nothing is published.
            e.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn release(&self, token: u64) {
        if let Some(e) = self.entries.get(token as usize) {
            // Release: the encoder's reads happen before the exporter writes again.
            e.count.fetch_sub(1, Ordering::Release);
        }
    }
}

struct Pending {
    entry: usize,
    /// Fence value signalled after this conversion's copy.
    value: u64,
    timestamp: Timestamp,
    sequence: u64,
}

/// Byte layout of one ring buffer, computed once (the readback's layout).
#[derive(Debug, Clone, Copy)]
struct Layout {
    stride: u32,
    chroma_offset: u64,
    /// Bytes the planes use.
    len: u64,
}

impl Layout {
    fn new(format: YuvFormat, width: u32, height: u32) -> Result<Self, GpuError> {
        let sample = match format {
            YuvFormat::Nv12 => 1,
            YuvFormat::P010 => 2,
        };
        // Luma rows are `width` samples; chroma rows are `width / 2` pairs: the same bytes.
        let stride = (width * sample).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let chroma_offset = u64::from(stride) * u64::from(height);
        let len = chroma_offset + u64::from(stride) * u64::from(height / 2);
        if usize::try_from(len).is_err() {
            return Err(GpuError::InvalidSize { width, height });
        }
        Ok(Self {
            stride,
            chroma_offset,
            len,
        })
    }

    fn planes(&self) -> [PlaneLayout; 2] {
        let stride = self.stride as usize;
        [
            PlaneLayout { offset: 0, stride },
            PlaneLayout {
                // `new` checked that the length fits in `usize`.
                offset: usize::try_from(self.chroma_offset).unwrap_or(usize::MAX),
                stride,
            },
        ]
    }
}

/// Converts canvases into shared D3D12 buffers for encoders that read linear device
/// memory (NVENC through CUDA). Owned by the render thread.
///
/// Installed but unused costs nothing: an exporter exists only while an encoder that
/// takes `d3d12_buffer` memory is Active (P13). It holds no pool slots; dropping it frees
/// its own GPU objects, and the ring lives on only while an exported frame holds it.
pub struct D3dBufferExporter {
    converter: YuvConverter,
    format: VideoFormat,
    layout: Layout,
    /// Size of each buffer's allocation (what an importer maps).
    alloc: u64,
    adapter_luid: u64,
    luma_tex: wgpu::Texture,
    luma: wgpu::TextureView,
    chroma_tex: wgpu::Texture,
    chroma: wgpu::TextureView,
    /// wgpu wrappers of the ring buffers, by entry.
    buffers: Vec<wgpu::Buffer>,
    ring: Arc<Ring>,
    /// Last value the fence was told to signal.
    fence_value: u64,
    /// Converted entries not handed out yet, in submission order (capacity: ring size).
    pending: VecDeque<Pending>,
    /// Converted but not yet handed out, by entry.
    queued: Vec<bool>,
    /// Where the search for a free entry starts (round robin).
    next: usize,
    stats: ExportStats,
}

impl D3dBufferExporter {
    /// Creates `surfaces` shared buffers for `width x height` frames of `format`, converted
    /// from `src_width x src_height` canvases ([`YuvConverter::scaled_tone`]). Not for
    /// real-time threads: it allocates.
    ///
    /// `surfaces` is 2 to [`MAX_EXPORT_SURFACES`]. The render device must be D3D12.
    #[allow(clippy::too_many_arguments)]
    pub fn scaled_tone(
        ctx: &GpuContext,
        format: YuvFormat,
        src_width: u32,
        src_height: u32,
        width: u32,
        height: u32,
        surfaces: usize,
        color: ColorSpace,
        open_shadows: bool,
    ) -> Result<Self, GpuError> {
        if !(2..=MAX_EXPORT_SURFACES).contains(&surfaces) {
            return Err(GpuError::Unsupported("export ring size outside 2..=32"));
        }
        let adapter_luid = ctx
            .adapter_luid()
            .ok_or(GpuError::Unsupported("render device is not D3D12"))?;
        // Checks the size, the colour, the P010 capability and builds the pipelines.
        let converter = YuvConverter::scaled_tone(
            ctx,
            format,
            src_width,
            src_height,
            width,
            height,
            color,
            open_shadows,
        )?;
        let layout = Layout::new(format, width, height)?;
        let plane = |label, w, h, f| {
            ctx.device().create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: f,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let luma_tex = plane("rbs-gpu export luma", width, height, format.luma_format());
        let chroma_tex = plane(
            "rbs-gpu export chroma",
            width / 2,
            height / 2,
            format.chroma_format(),
        );

        // SAFETY: the HAL device is borrowed only to create and share objects of its own;
        // wgpu state is not touched and the guard is dropped at the end of this block.
        let (entries, alloc, fence, fence_handle) = unsafe {
            let hal = ctx
                .device()
                .as_hal::<Dx12>()
                .ok_or(GpuError::Unsupported("render device is not D3D12"))?;
            let device = hal.raw_device();
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Alignment: 0,
                Width: layout.len,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_UNKNOWN,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: D3D12_RESOURCE_FLAG_NONE,
            };
            let alloc = device.GetResourceAllocationInfo(0, &[desc]).SizeInBytes;
            let mut entries = Vec::with_capacity(surfaces);
            for _ in 0..surfaces {
                let mut resource: Option<ID3D12Resource> = None;
                device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_SHARED,
                        &desc,
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut resource,
                    )
                    .map_err(|_| GpuError::Export("CreateCommittedResource failed"))?;
                let resource =
                    resource.ok_or(GpuError::Export("CreateCommittedResource returned null"))?;
                let handle = device
                    .CreateSharedHandle(&resource, None, GENERIC_ALL.0, PCWSTR::null())
                    .map_err(|_| GpuError::Export("CreateSharedHandle failed"))?;
                entries.push(Entry {
                    _resource: resource,
                    handle: OwnedHandle(handle),
                    count: AtomicU32::new(0),
                });
            }
            let fence: ID3D12Fence = device
                .CreateFence(0, D3D12_FENCE_FLAG_SHARED)
                .map_err(|_| GpuError::Export("CreateFence failed"))?;
            let fence_handle = device
                .CreateSharedHandle(&fence, None, GENERIC_ALL.0, PCWSTR::null())
                .map_err(|_| GpuError::Export("CreateSharedHandle (fence) failed"))?;
            (entries, alloc, fence, OwnedHandle(fence_handle))
        };
        let buffers = entries
            .iter()
            .map(|e| {
                // SAFETY: `_resource` is a committed buffer of `layout.len` bytes created on
                // this device above; the wrapper keeps its own COM reference.
                let hal = unsafe {
                    wgpu::hal::dx12::Device::buffer_from_raw(e._resource.clone(), layout.len)
                };
                // SAFETY: the HAL buffer belongs to this device and matches the descriptor
                // (size, copy destination only, not mapped).
                unsafe {
                    ctx.device().create_buffer_from_hal::<Dx12>(
                        hal,
                        &wgpu::BufferDescriptor {
                            label: Some("rbs-gpu export buffer"),
                            size: layout.len,
                            usage: wgpu::BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        },
                    )
                }
            })
            .collect();
        let format_out = VideoFormat::new(Size::new(width, height), format.pixel_format(), color);
        Ok(Self {
            converter,
            format: format_out,
            layout,
            alloc,
            adapter_luid,
            luma: luma_tex.create_view(&wgpu::TextureViewDescriptor::default()),
            luma_tex,
            chroma: chroma_tex.create_view(&wgpu::TextureViewDescriptor::default()),
            chroma_tex,
            buffers,
            ring: Arc::new(Ring {
                entries: entries.into_boxed_slice(),
                fence,
                fence_handle,
                _device: ctx.device().clone(),
            }),
            fence_value: 0,
            pending: VecDeque::with_capacity(surfaces),
            queued: vec![false; surfaces],
            next: 0,
            stats: ExportStats::default(),
        })
    }

    /// Format of the exported frames.
    pub fn format(&self) -> &VideoFormat {
        &self.format
    }

    /// Counters.
    pub fn stats(&self) -> ExportStats {
        self.stats
    }

    /// The scale kernel of the conversion (ADR-0102).
    pub fn kernel(&self) -> crate::yuv::ScaleKernel {
        self.converter.kernel()
    }

    /// Buffers in the ring.
    pub fn surfaces(&self) -> usize {
        self.buffers.len()
    }

    /// Conversions not handed out yet (ADR-0147 decision 5, ADR-0045).
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// The lease owner of exported frames. The engine keeps this handle until the encoder
    /// has shut down and drops it off the real-time threads (ADR-0147 decision 8).
    pub fn lease_owner(&self) -> Arc<dyn LeaseOwner> {
        self.ring.clone()
    }

    /// Exported frames the encoder still holds.
    pub fn held(&self) -> usize {
        self.ring
            .entries
            .iter()
            .filter(|e| e.count.load(Ordering::Acquire) > 0)
            .count()
    }

    /// A free entry, or `ExportBusy` (counted).
    fn free_entry(&mut self) -> Result<usize, GpuError> {
        let n = self.buffers.len();
        let entries = &self.ring.entries;
        let queued = &self.queued;
        // Acquire: pairs with the `Release` decrement of the encoder's last lease, so its
        // reads of the buffer happen before this conversion writes it.
        match (0..n)
            .map(|k| (self.next + k) % n)
            .find(|&i| !queued[i] && entries[i].count.load(Ordering::Acquire) == 0)
        {
            Some(i) => Ok(i),
            None => {
                self.stats.busy += 1;
                Err(GpuError::ExportBusy { surfaces: n as u32 })
            }
        }
    }

    /// Makes the render queue signal the shared fence after everything submitted so far,
    /// and returns the value. Hot path; does not allocate.
    fn signal(&mut self, ctx: &GpuContext) -> Result<u64, GpuError> {
        let value = self.fence_value + 1;
        // SAFETY: the HAL queue is borrowed only to enqueue a signal of a fence this
        // exporter owns; the guard is dropped at the end of this block. A D3D12 command
        // queue is free-threaded, and `Signal` runs after every command list submitted
        // to the queue before it, which includes this exporter's copy (`submit` hands the
        // command lists to the queue before it returns).
        let signalled = unsafe {
            let queue = ctx
                .queue()
                .as_hal::<Dx12>()
                .ok_or(GpuError::Unsupported("render queue is not D3D12"))?;
            queue.as_raw().Signal(&self.ring.fence, value)
        };
        if signalled.is_err() || ctx.metrics().device_lost() {
            return Err(GpuError::DeviceLost);
        }
        self.fence_value = value;
        Ok(value)
    }

    /// Converts `canvas` (a canvas texture in `pool`) into the next free buffer. The frame
    /// is available from [`Self::take_ready`] right away; its fence value tells the
    /// encoder when the GPU has written it.
    ///
    /// Hot path; does not allocate in RBS code. Returns [`GpuError::ExportBusy`] when every
    /// buffer is queued or still held by the encoder; the caller drops the frame.
    pub fn export(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        canvas: TextureId,
        timestamp: Timestamp,
        sequence: u64,
    ) -> Result<(), GpuError> {
        let i = self.free_entry()?;
        let source = self.converter.source(pool, canvas)?;
        let mut encoder = self.converter.record(ctx, source, &self.luma, &self.chroma);
        let buffer = &self.buffers[i];
        let stride = self.layout.stride;
        for (tex, offset) in [
            (&self.luma_tex, 0),
            (&self.chroma_tex, self.layout.chroma_offset),
        ] {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset,
                        bytes_per_row: Some(stride),
                        rows_per_image: Some(tex.height()),
                    },
                },
                tex.size(),
            );
        }
        ctx.queue().submit(Some(encoder.finish()));
        let value = self.signal(ctx)?;
        self.queued[i] = true;
        self.next = (i + 1) % self.buffers.len();
        self.stats.converted += 1;
        // At most one entry per buffer, so this never grows past the reserved capacity.
        self.pending.push_back(Pending {
            entry: i,
            value,
            timestamp,
            sequence,
        });
        Ok(())
    }

    /// Builds the frame of entry `i` with one counted lease.
    fn frame(&self, i: usize, value: u64, timestamp: Timestamp) -> VideoFrame {
        let token = i as u64;
        self.ring.retain(token);
        let owner: Arc<dyn LeaseOwner> = self.ring.clone();
        // SAFETY: one reference to entry `token` was counted just above and is handed over.
        let lease = unsafe { Lease::adopt(owner, token) };
        let surface = NativeSurface::d3d_shared_buffer(
            self.ring.entries[i].handle.0.0 as usize as u64,
            self.alloc,
            self.adapter_luid,
            self.layout.planes(),
        );
        let sync = SurfaceSync::timeline_fence(self.ring.fence_handle.0.0 as usize as u64, value);
        // SAFETY: the handle names a live shared buffer of `self.alloc` bytes on this
        // adapter, holding a frame of `self.format` at `planes` once the fence reaches
        // `value` (signalled after the copy). The ring owns the buffer, both handles and
        // the fence, and `lease` keeps the ring alive, so they stay valid while the lease
        // lives. The exporter writes an entry only when its count is zero, so it does not
        // write while this frame exists. Only the encoder reads it: the native export
        // output pin is exclusive (ADR-0147 decision 3), so other holders only keep the
        // buffer alive.
        unsafe { VideoFrame::exported(self.format, timestamp, surface, sync, lease) }
    }

    /// The oldest converted frame as a native `D3dSharedBuffer` frame with a timeline
    /// fence, holding a lease on its buffer; `None` when nothing is queued. The GPU may
    /// still be writing it: the reader waits for the fence. Does not block.
    ///
    /// Returns `GpuError::DeviceLost` once the device is lost: frames converted on a lost
    /// device are never delivered. Hot path; does not allocate in RBS code.
    pub fn take_ready(&mut self, ctx: &GpuContext) -> Result<Option<VideoFrame>, GpuError> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        if ctx.metrics().device_lost() {
            self.pending.clear();
            self.queued.iter_mut().for_each(|q| *q = false);
            return Err(GpuError::DeviceLost);
        }
        let Some(p) = self.pending.pop_front() else {
            return Ok(None);
        };
        self.queued[p.entry] = false;
        self.stats.delivered += 1;
        let frame = self.frame(p.entry, p.value, p.timestamp);
        Ok(Some(frame.with_sequence(p.sequence)))
    }

    /// [`Self::take_ready`] under the readback's name for the end of a stream: frames come
    /// out in submission order, one per call, none dropped (ADR-0147 decision 5).
    pub fn take_oldest(&mut self, ctx: &GpuContext) -> Result<Option<VideoFrame>, GpuError> {
        self.take_ready(ctx)
    }

    /// A frame of the black picture in a buffer of its own. The caller keeps the frame
    /// (and with it the buffer) for as long as it may send the picture, typically an
    /// export hold (ADR-0147 decision 5); the ring never writes that buffer while the
    /// frame lives. Not for real-time threads: it allocates the picture once.
    /// `GpuError::ExportBusy` when no buffer is free.
    pub fn black(
        &mut self,
        ctx: &GpuContext,
        timestamp: Timestamp,
    ) -> Result<VideoFrame, GpuError> {
        let i = self.free_entry()?;
        let bytes = black_picture(
            self.format.pixel_format == YuvFormat::P010.pixel_format(),
            self.format.color.range,
            self.layout,
        );
        ctx.queue().write_buffer(&self.buffers[i], 0, &bytes);
        ctx.queue().submit(None);
        let value = self.signal(ctx)?;
        Ok(self.frame(i, value, timestamp))
    }
}

/// The bytes of a black frame in `layout`: luma code 16 (8-bit) or 64 (10-bit, in the
/// high bits of P010 words, little-endian) for limited range, 0 for full range, and the
/// neutral chroma code 128 or 512.
fn black_picture(ten_bit: bool, range: ColorRange, layout: Layout) -> Vec<u8> {
    let full = range == ColorRange::Full;
    let len = usize::try_from(layout.len).unwrap_or(0);
    let split = usize::try_from(layout.chroma_offset)
        .unwrap_or(len)
        .min(len);
    let mut bytes = vec![0u8; len];
    if ten_bit {
        let luma: u16 = if full { 0 } else { 64 << 6 };
        let chroma: u16 = 512 << 6;
        for (k, pair) in bytes.chunks_exact_mut(2).enumerate() {
            let code = if k * 2 < split { luma } else { chroma };
            pair.copy_from_slice(&code.to_le_bytes());
        }
    } else {
        let (luma, chroma) = bytes.split_at_mut(split);
        luma.fill(if full { 0 } else { 16 });
        chroma.fill(128);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_puts_chroma_right_after_the_luma_rows() {
        let l = Layout::new(YuvFormat::Nv12, 1920, 1080).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(l.stride, 2048);
        assert_eq!(l.chroma_offset, 2048 * 1080);
        assert_eq!(l.len, 2048 * 1080 + 2048 * 540);
        let p = Layout::new(YuvFormat::P010, 1280, 720).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(p.stride, 2560);
        assert_eq!(p.planes()[1].offset, 2560 * 720);
    }

    #[test]
    fn black_is_limited_range_codes() {
        let l = Layout::new(YuvFormat::Nv12, 4, 2).unwrap_or_else(|e| panic!("{e}"));
        let b = black_picture(false, ColorRange::Limited, l);
        assert_eq!(b[0], 16);
        assert_eq!(b[l.chroma_offset as usize], 128);
        let l10 = Layout::new(YuvFormat::P010, 4, 2).unwrap_or_else(|e| panic!("{e}"));
        let b10 = black_picture(true, ColorRange::Limited, l10);
        assert_eq!(u16::from_le_bytes([b10[0], b10[1]]), 64 << 6);
        let c = l10.chroma_offset as usize;
        assert_eq!(u16::from_le_bytes([b10[c], b10[c + 1]]), 512 << 6);
    }
}
