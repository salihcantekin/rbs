//! Canvas to system-memory 4:2:0 frames for encoders that take only system memory
//! (spec `docs/specs/gpu-compositor.md`, "System-memory download"; ADR-0038).
//!
//! [`YuvDownload`] converts a canvas to NV12 or P010 on the GPU, copies both planes into
//! one of a fixed ring of mappable staging buffers and maps it without waiting. Once the
//! GPU has finished, [`YuvDownload::take_ready`] copies the mapped bytes into a recycled
//! frame buffer and returns a `System` frame. Nothing blocks: a ring that is full drops
//! the new frame (counted), like the native exporter (ADR-0025).
//!
//! This is the CPU fallback of the encoder conversion: every delivered frame counts in
//! `GpuMetrics::frames_cpu_copied` (principle P4). It is the default only where the
//! encoder cannot read GPU memory yet (NVENC and AMF on Windows today, ADR-0038).
//!
//! Hot path: [`YuvDownload::submit`] and [`YuvDownload::take_ready`] do not allocate in
//! RBS code once warm; wgpu allocates internally while recording a command buffer and
//! for the map callback. Frame buffers are recycled through `Bytes::try_into_mut`, which
//! keeps a shared buffer's allocation when the ring holds the only reference.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use bytes::BytesMut;
use rbs_core::media::{
    Bytes, ColorSpace, FramePayload, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::TexturePool;
use crate::yuv::{YuvConverter, YuvFormat};

/// Staging buffers in flight between the GPU and the CPU. At 60 fps a frame normally
/// completes within one or two ticks; four leave room for a slow readback.
pub const DOWNLOAD_RING: usize = 4;

/// Frame buffers handed to consumers and recycled. A consumer chain (render output slot,
/// the queue to the encoder, the encoder's input) holds about three at once.
pub const DOWNLOAD_BUFFERS: usize = 6;

const IDLE: u8 = 0;
const MAPPING: u8 = 1;
const MAPPED: u8 = 2;
const FAILED: u8 = 3;

/// Counters of one download.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DownloadStats {
    /// Frames converted and queued for readback.
    pub converted: u64,
    /// Frames returned by `take_ready` or `take_oldest`.
    pub delivered: u64,
    /// `submit` calls refused because every staging buffer was in flight.
    pub busy: u64,
    /// Read-back frames dropped because a newer one completed in the same poll
    /// (`take_ready` only).
    pub stale: u64,
    /// Read-back frames dropped because consumers held every frame buffer.
    pub no_buffer: u64,
    /// Readbacks that failed: the device could not map them, or the frame could not be
    /// built.
    pub failed: u64,
}

struct Slot {
    staging: wgpu::Buffer,
    state: Arc<AtomicU8>,
    timestamp: Timestamp,
    sequence: u64,
}

/// Converts canvases to NV12 or P010 frames in system memory, without blocking.
pub struct YuvDownload {
    converter: YuvConverter,
    format: VideoFormat,
    luma: wgpu::TextureView,
    luma_tex: wgpu::Texture,
    chroma: wgpu::TextureView,
    chroma_tex: wgpu::Texture,
    /// Bytes per row of both planes in the staging buffers and the frames, a multiple of
    /// `wgpu::COPY_BYTES_PER_ROW_ALIGNMENT`.
    stride: u32,
    /// Offset of the chroma plane.
    chroma_offset: u64,
    len: u64,
    slots: Vec<Slot>,
    /// Next slot to submit into; slots are used in ring order.
    head: usize,
    /// Oldest slot that may be in flight.
    tail: usize,
    buffers: Vec<Bytes>,
    stats: DownloadStats,
}

impl YuvDownload {
    /// A download of `width x height` canvases to `format`. Not real-time: creates the
    /// converter, two plane textures, [`DOWNLOAD_RING`] staging buffers and
    /// [`DOWNLOAD_BUFFERS`] frame buffers. Width and height must be even.
    pub fn new(
        ctx: &GpuContext,
        format: YuvFormat,
        width: u32,
        height: u32,
    ) -> Result<Self, GpuError> {
        let converter = YuvConverter::new(ctx, format, width, height)?;
        let sample = match format {
            YuvFormat::Nv12 => 1,
            YuvFormat::P010 => 2,
        };
        // Luma rows are `width` samples; chroma rows are `width / 2` pairs: the same bytes.
        let stride = (width * sample).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let chroma_offset = u64::from(stride) * u64::from(height);
        let len = chroma_offset + u64::from(stride) * u64::from(height / 2);
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
        let luma_tex = plane("rbs-gpu download luma", width, height, format.luma_format());
        let chroma_tex = plane(
            "rbs-gpu download chroma",
            width / 2,
            height / 2,
            format.chroma_format(),
        );
        let slots = (0..DOWNLOAD_RING)
            .map(|_| Slot {
                staging: ctx.device().create_buffer(&wgpu::BufferDescriptor {
                    label: Some("rbs-gpu download staging"),
                    size: len,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
                state: Arc::new(AtomicU8::new(IDLE)),
                timestamp: Timestamp::ZERO,
                sequence: 0,
            })
            .collect();
        let size = usize::try_from(len).map_err(|_| GpuError::InvalidSize { width, height })?;
        let buffers = (0..DOWNLOAD_BUFFERS).map(|_| shared_buffer(size)).collect();
        Ok(Self {
            converter,
            format: VideoFormat::new(
                Size::new(width, height),
                format.pixel_format(),
                ColorSpace::BT709_LIMITED,
            ),
            luma: luma_tex.create_view(&wgpu::TextureViewDescriptor::default()),
            luma_tex,
            chroma: chroma_tex.create_view(&wgpu::TextureViewDescriptor::default()),
            chroma_tex,
            stride,
            chroma_offset,
            len,
            slots,
            head: 0,
            tail: 0,
            buffers,
            stats: DownloadStats::default(),
        })
    }

    /// The format of the frames this download delivers: the canvas size, NV12 or P010,
    /// BT.709 limited range (the converter's output).
    pub fn format(&self) -> &VideoFormat {
        &self.format
    }

    /// Bytes per row of both planes in delivered frames.
    pub fn stride(&self) -> usize {
        self.stride as usize
    }

    /// Counters.
    pub fn stats(&self) -> DownloadStats {
        self.stats
    }

    /// Readbacks in flight.
    pub fn in_flight(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.state.load(Ordering::Acquire) != IDLE)
            .count()
    }

    /// Converts `canvas` (a `Texture` frame of the canvas size) and starts reading it
    /// back. Returns `Ok(false)` when every staging buffer is in flight: the frame is
    /// dropped and counted in `busy`. Hot path; never waits.
    pub fn submit(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        canvas: &VideoFrame,
    ) -> Result<bool, GpuError> {
        let FramePayload::Texture(id) = *canvas.payload() else {
            return Err(GpuError::Unsupported("download needs a pool texture frame"));
        };
        let slot = &mut self.slots[self.head];
        if slot.state.load(Ordering::Acquire) != IDLE {
            self.stats.busy += 1;
            return Ok(false);
        }
        let source = self.converter.source(pool, id)?;
        let mut encoder = self.converter.record(ctx, source, &self.luma, &self.chroma);
        let copy = |encoder: &mut wgpu::CommandEncoder, tex: &wgpu::Texture, offset: u64| {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &slot.staging,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset,
                        bytes_per_row: Some(self.stride),
                        rows_per_image: Some(tex.height()),
                    },
                },
                tex.size(),
            );
        };
        copy(&mut encoder, &self.luma_tex, 0);
        copy(&mut encoder, &self.chroma_tex, self.chroma_offset);
        ctx.queue().submit(Some(encoder.finish()));

        slot.timestamp = canvas.timestamp();
        slot.sequence = canvas.sequence();
        slot.state.store(MAPPING, Ordering::Release);
        let state = Arc::clone(&slot.state);
        slot.staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let next = if result.is_ok() { MAPPED } else { FAILED };
                state.store(next, Ordering::Release);
            });
        self.head = (self.head + 1) % self.slots.len();
        self.stats.converted += 1;
        Ok(true)
    }

    /// The newest frame whose readback has completed, if any. Older completed frames are
    /// dropped (`stale`). Call after `GpuContext::poll` (the render runtime polls at the
    /// end of each tick). Hot path; never waits.
    pub fn take_ready(&mut self, ctx: &GpuContext) -> Result<Option<VideoFrame>, GpuError> {
        self.take::<true>(ctx)
    }

    /// The oldest frame whose readback has completed, if any; newer completed frames
    /// stay for later calls. For the end of a stream, where every frame is passed on in
    /// order and none is dropped as stale (ADR-0044). Hot path; never waits.
    pub fn take_oldest(&mut self, ctx: &GpuContext) -> Result<Option<VideoFrame>, GpuError> {
        self.take::<false>(ctx)
    }

    /// `take_ready` (`NEWEST`) or `take_oldest`.
    fn take<const NEWEST: bool>(
        &mut self,
        ctx: &GpuContext,
    ) -> Result<Option<VideoFrame>, GpuError> {
        let mut newest = None;
        let mut failed = false;
        // Slots complete in submission order; walk from the oldest while they are done.
        while self.tail != self.head || self.state(self.tail) != IDLE {
            match self.state(self.tail) {
                MAPPED => {
                    if let Some(older) = newest.replace(self.tail) {
                        self.recycle(older);
                        self.stats.stale += 1;
                    }
                    if !NEWEST {
                        self.tail = (self.tail + 1) % self.slots.len();
                        break;
                    }
                }
                FAILED => {
                    self.recycle(self.tail);
                    self.stats.failed += 1;
                    failed = true;
                }
                _ => break,
            }
            self.tail = (self.tail + 1) % self.slots.len();
        }
        let Some(index) = newest else {
            return if failed {
                Err(GpuError::Readback("staging buffer map failed".into()))
            } else {
                Ok(None)
            };
        };
        let frame = self.read(index);
        self.recycle(index);
        if frame.is_some() {
            ctx.metrics().count_cpu_copy();
            self.stats.delivered += 1;
        }
        Ok(frame)
    }

    fn state(&self, index: usize) -> u8 {
        self.slots[index].state.load(Ordering::Acquire)
    }

    /// Unmaps slot `index` and makes it free again.
    fn recycle(&mut self, index: usize) {
        let slot = &self.slots[index];
        if slot.state.load(Ordering::Acquire) == MAPPED {
            slot.staging.unmap();
        }
        slot.state.store(IDLE, Ordering::Release);
    }

    /// Copies mapped slot `index` into a free frame buffer.
    fn read(&mut self, index: usize) -> Option<VideoFrame> {
        let Some(free) = self.buffers.iter().position(Bytes::is_unique) else {
            self.stats.no_buffer += 1;
            return None;
        };
        // Unique, so this keeps the allocation (no copy, no allocation).
        let mut buffer: BytesMut = match std::mem::take(&mut self.buffers[free]).try_into_mut() {
            Ok(b) => b,
            Err(shared) => {
                self.buffers[free] = shared;
                self.stats.no_buffer += 1;
                return None;
            }
        };
        let slot = &self.slots[index];
        match slot.staging.slice(..).get_mapped_range() {
            Ok(mapped) => buffer.copy_from_slice(&mapped),
            Err(_) => {
                // Put the (still unique) buffer back; the frame is lost.
                self.buffers[free] = buffer.freeze();
                self.stats.failed += 1;
                return None;
            }
        }
        let bytes = buffer.freeze();
        self.buffers[free] = bytes.clone();
        let stride = self.stride as usize;
        let planes = [
            PlaneLayout { offset: 0, stride },
            PlaneLayout {
                offset: self.chroma_offset as usize,
                stride,
            },
        ];
        let Ok(system) = SystemFrame::new(self.format, bytes, &planes) else {
            // Cannot happen for the layout `new` computed; counted, never silent.
            self.stats.failed += 1;
            return None;
        };
        Some(VideoFrame::system(slot.timestamp, system, None).with_sequence(slot.sequence))
    }

    /// Bytes of one delivered frame.
    pub fn frame_len(&self) -> u64 {
        self.len
    }
}

/// A zeroed buffer of `len` bytes in `Bytes`' shared representation, so cloning it and
/// turning it back into `BytesMut` when unique never allocates.
fn shared_buffer(len: usize) -> Bytes {
    let mut buffer = BytesMut::zeroed(len + 1);
    // Splitting moves the buffer to the shared (reference-counted) representation.
    drop(buffer.split_off(len));
    buffer.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recycled_buffers_keep_their_allocation() {
        let mut b = shared_buffer(64);
        let ptr = b.as_ptr();
        for round in 0..4u8 {
            let held = b.clone();
            assert!(!b.is_unique());
            drop(held);
            assert!(b.is_unique());
            let mut m = b.try_into_mut().unwrap_or_else(|_| BytesMut::new());
            assert_eq!(m.len(), 64);
            m.fill(round);
            b = m.freeze();
            assert_eq!(b.as_ptr(), ptr);
        }
    }
}
