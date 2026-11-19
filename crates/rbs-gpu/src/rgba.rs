//! A source's picture to a straight-alpha sRGB RGBA8 frame in system memory, for an
//! image file of a still source (ADR-0117 decision 5; spec
//! `docs/specs/gpu-compositor.md`, "Image download (RGBA8)").
//!
//! [`RgbaDownload`] takes one picture at a time. A filter's or the normalisation pass's
//! output (linear `Rgba16Float`, premultiplied) goes through one pass that
//! un-premultiplies, encodes sRGB and writes RGBA8. A still source's own upload
//! (`Rgba8`, sRGB, straight alpha) is copied as it is, with no pass. Either way the
//! bytes go to one mappable staging buffer that is mapped without waiting;
//! [`RgbaDownload::take`] returns the frame once the GPU has finished.
//!
//! Hot path only in a narrow sense: [`RgbaDownload::submit_at`] runs on the render
//! thread once per picture change, never on a steady tick. A larger picture than the
//! download has held creates a larger staging buffer and target on that call (the
//! ADR-0062 decision 5 exception: once per size growth). [`RgbaDownload::take`] copies
//! the rows into one buffer this download keeps. The first picture, and a larger one,
//! may allocate; after that the same buffer is reused once the caller has dropped the
//! previous frame. A frame still held is left alone. `take` runs on the render thread
//! once per change and counts in `GpuMetrics::frames_cpu_copied` (P4, ADR-0038's
//! exception for image tracks).

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use bytes::BytesMut;
use rbs_core::media::{
    Bytes, ColorSpace, FramePayload, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat,
    VideoFrame,
};
use rbs_core::time::Timestamp;

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::TexturePool;

const IDLE: u8 = 0;
const MAPPING: u8 = 1;
const MAPPED: u8 = 2;
const FAILED: u8 = 3;

/// Counters of one image download.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RgbaStats {
    /// Pictures converted by the pass (premultiplied linear input).
    pub converted: u64,
    /// Pictures copied as they are (straight sRGB `Rgba8` input).
    pub copied: u64,
    /// Frames returned by `take`.
    pub delivered: u64,
    /// `submit_at` calls refused because the previous readback was still in flight.
    pub busy: u64,
    /// Times the staging buffer or the target grew for a larger picture.
    pub grown: u64,
    /// Readbacks the device could not map.
    pub failed: u64,
}

struct Pending {
    timestamp: Timestamp,
    sequence: u64,
    width: u32,
    height: u32,
    stride: u32,
}

/// Reads one picture at a time back as straight-alpha sRGB RGBA8, without blocking.
pub struct RgbaDownload {
    pipeline: wgpu::RenderPipeline,
    /// The pass's target, as large as the largest premultiplied picture so far; a
    /// smaller one renders into its top-left corner.
    target: Option<(wgpu::Texture, wgpu::TextureView)>,
    staging: wgpu::Buffer,
    capacity: u64,
    /// Tightly packed RGBA8 of the largest picture taken. Shared with the frame
    /// last returned until that frame is dropped, then reused
    /// (`Bytes::try_into_mut`, as [`crate::download::YuvDownload`]'s `read` does).
    pixels: Bytes,
    state: Arc<AtomicU8>,
    pending: Pending,
    stats: RgbaStats,
}

/// Bytes per row of a `width`-pixel RGBA8 row in the staging buffer.
fn padded_stride(width: u32) -> u32 {
    (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
}

fn staging(ctx: &GpuContext, len: u64) -> wgpu::Buffer {
    ctx.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("rbs-gpu rgba staging"),
        size: len,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    })
}

impl RgbaDownload {
    /// A download sized for `width x height` pictures. Not real-time: builds the pass
    /// and one staging buffer. A larger picture later grows it once.
    pub fn new(ctx: &GpuContext, width: u32, height: u32) -> Result<Self, GpuError> {
        if width == 0 || height == 0 {
            return Err(GpuError::InvalidSize { width, height });
        }
        let device = ctx.device();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rbs-gpu rgba"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/rgba.wgsl").into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-gpu rgba"),
            bind_group_layouts: &[Some(&ctx.sampled_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rbs-gpu rgba"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_fullscreen"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_rgba"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let capacity = u64::from(padded_stride(width)) * u64::from(height);
        let packed = packed_len(width, height).ok_or(GpuError::InvalidSize { width, height })?;
        Ok(Self {
            pipeline,
            target: None,
            staging: staging(ctx, capacity),
            capacity,
            pixels: shared_pixels(packed),
            state: Arc::new(AtomicU8::new(IDLE)),
            pending: Pending {
                timestamp: Timestamp::ZERO,
                sequence: 0,
                width: 0,
                height: 0,
                stride: 0,
            },
            stats: RgbaStats::default(),
        })
    }

    /// Counters.
    pub fn stats(&self) -> RgbaStats {
        self.stats
    }

    /// Whether a readback is in flight or waits to be taken.
    pub fn in_flight(&self) -> bool {
        self.state.load(Ordering::Acquire) != IDLE
    }

    /// Starts reading `picture` (a pool texture frame) back, stamped `timestamp` and
    /// `sequence`. Returns `Ok(false)` while the previous readback is in flight (counted
    /// in `busy`): the caller keeps the change and tries again later. `Rgba16Float`
    /// input, and `Rgba8Srgb` input in an `Rgba8UnormSrgb` texture (a decoded NV12
    /// frame), are taken as linear and premultiplied; `Rgba8` input must be sRGB with
    /// straight alpha and copyable. Never waits.
    pub fn submit_at(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        picture: &VideoFrame,
        timestamp: Timestamp,
        sequence: u64,
    ) -> Result<bool, GpuError> {
        let FramePayload::Texture(id) = *picture.payload() else {
            return Err(GpuError::Unsupported(
                "image download needs a pool texture frame",
            ));
        };
        if self.in_flight() {
            self.stats.busy += 1;
            return Ok(false);
        }
        let tex = pool.get(id).ok_or(GpuError::StaleTexture(id))?;
        let d = *tex.desc();
        let direct = match d.format {
            wgpu::TextureFormat::Rgba16Float => false,
            // A decoded 8-bit YUV frame: samples are linear, like the canvas format.
            wgpu::TextureFormat::Rgba8UnormSrgb
                if picture.format().pixel_format == PixelFormat::Rgba8Srgb =>
            {
                false
            }
            wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb
                if picture.format().pixel_format == PixelFormat::Rgba8
                    && d.usage.contains(wgpu::TextureUsages::COPY_SRC) =>
            {
                true
            }
            _ => return Err(GpuError::TargetMismatch(id)),
        };
        let stride = padded_stride(d.width);
        let len = u64::from(stride) * u64::from(d.height);
        if len > self.capacity {
            // Once per growth, never on a steady tick (ADR-0062 decision 5 exception).
            self.staging = staging(ctx, len);
            self.capacity = len;
            self.stats.grown += 1;
        }
        let mut encoder = ctx
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rbs-gpu rgba"),
            });
        let extent = wgpu::Extent3d {
            width: d.width,
            height: d.height,
            depth_or_array_layers: 1,
        };
        let source = if direct {
            tex.texture()
        } else {
            let bind = tex
                .sampled_bind_group()
                .ok_or(GpuError::TargetMismatch(id))?;
            let fits = self
                .target
                .as_ref()
                .is_some_and(|(t, _)| t.width() >= d.width && t.height() >= d.height);
            if !fits {
                let (w, h) = self
                    .target
                    .as_ref()
                    .map_or((0, 0), |(t, _)| (t.width(), t.height()));
                let texture = ctx.device().create_texture(&wgpu::TextureDescriptor {
                    label: Some("rbs-gpu rgba target"),
                    size: wgpu::Extent3d {
                        width: w.max(d.width),
                        height: h.max(d.height),
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                if self.target.is_some() {
                    self.stats.grown += 1;
                }
                self.target = Some((texture, view));
            }
            let Some((texture, view)) = self.target.as_ref() else {
                return Err(GpuError::Unsupported("image download target missing"));
            };
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("rbs-gpu rgba"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_viewport(0.0, 0.0, d.width as f32, d.height as f32, 0.0, 1.0);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, bind, &[]);
                pass.draw(0..3, 0..1);
            }
            texture
        };
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: source,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(d.height),
                },
            },
            extent,
        );
        ctx.queue().submit(Some(encoder.finish()));

        self.pending = Pending {
            timestamp,
            sequence,
            width: d.width,
            height: d.height,
            stride,
        };
        self.state.store(MAPPING, Ordering::Release);
        let state = Arc::clone(&self.state);
        self.staging
            .slice(..len)
            .map_async(wgpu::MapMode::Read, move |result| {
                let next = if result.is_ok() { MAPPED } else { FAILED };
                state.store(next, Ordering::Release);
            });
        if direct {
            self.stats.copied += 1;
        } else {
            self.stats.converted += 1;
        }
        Ok(true)
    }

    /// The picture whose readback has completed, if any: a `System` frame of the
    /// picture's size, `Rgba8`, `ColorSpace::SRGB`, straight alpha, rows packed tightly
    /// (stride `width * 4`), with the `submit_at` stamps. Call after `GpuContext::poll`.
    /// Copies the rows into the buffer this download keeps. Never waits.
    pub fn take(&mut self, ctx: &GpuContext) -> Result<Option<VideoFrame>, GpuError> {
        match self.state.load(Ordering::Acquire) {
            MAPPED => {}
            FAILED => {
                self.state.store(IDLE, Ordering::Release);
                self.stats.failed += 1;
                return Err(GpuError::Readback("image staging buffer map failed".into()));
            }
            _ => return Ok(None),
        }
        let width = self.pending.width;
        let height = self.pending.height;
        let src_stride = self.pending.stride;
        let timestamp = self.pending.timestamp;
        let sequence = self.pending.sequence;
        let len = u64::from(src_stride) * u64::from(height);
        let packed = packed_len(width, height).filter(|n| *n > 0);
        let row = packed.and_then(|n| usize::try_from(height).ok().and_then(|h| n.checked_div(h)));
        let (Some(packed), Some(row)) = (packed, row) else {
            self.staging.unmap();
            self.state.store(IDLE, Ordering::Release);
            self.stats.failed += 1;
            return Err(GpuError::Readback("image frame layout".into()));
        };
        // Reuse the kept buffer when this download is its only owner. A frame the
        // caller still holds stays as it was; that call gets a buffer of its own.
        // `writable_pixels` leaves `self.pixels` empty only when it took the kept buffer.
        let mut pixels = writable_pixels(&mut self.pixels, packed);
        let read = match self.staging.slice(..len).get_mapped_range() {
            Ok(mapped) => copy_tight(
                &mut pixels,
                &mapped,
                height as usize,
                src_stride as usize,
                row,
            ),
            Err(_) => false,
        };
        self.staging.unmap();
        self.state.store(IDLE, Ordering::Release);
        if !read {
            restore(&mut self.pixels, pixels);
            self.stats.failed += 1;
            return Err(GpuError::Readback("image staging buffer not mapped".into()));
        }
        let bytes = pixels.freeze();
        let Some(frame_bytes) = prefix(&bytes, packed) else {
            restore(&mut self.pixels, bytes);
            self.stats.failed += 1;
            return Err(GpuError::Readback("image frame layout".into()));
        };
        // Keep the larger allocation. A frame still held keeps the previous one;
        // this frame holds `frame_bytes`, which is not written again.
        if self.pixels.is_empty() || bytes.len() > self.pixels.len() {
            self.pixels = bytes;
        }
        let format = VideoFormat::new(
            Size::new(width, height),
            PixelFormat::Rgba8,
            ColorSpace::SRGB,
        );
        let planes = [PlaneLayout {
            offset: 0,
            stride: row,
        }];
        let system = SystemFrame::new(format, frame_bytes, &planes)
            .map_err(|_| GpuError::Readback("image frame layout".into()))?;
        ctx.metrics().count_cpu_copy();
        self.stats.delivered += 1;
        Ok(Some(
            VideoFrame::system(timestamp, system, None).with_sequence(sequence),
        ))
    }
}

/// Tightly packed RGBA8 byte count of a `width x height` picture.
fn packed_len(width: u32, height: u32) -> Option<usize> {
    usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)
}

/// A zeroed buffer of `len` bytes in `Bytes`' shared representation, so cloning it
/// and turning it back into `BytesMut` when unique keeps the allocation.
fn shared_pixels(len: usize) -> Bytes {
    let mut buffer = BytesMut::zeroed(len.saturating_add(1));
    // Splitting moves the buffer to the shared representation.
    let _tail = buffer.split_off(len);
    buffer.freeze()
}

/// `slot` as a writable buffer of at least `need` bytes when this download is its
/// only owner. Otherwise a new buffer, so a frame the caller still holds is not
/// written.
fn writable_pixels(slot: &mut Bytes, need: usize) -> BytesMut {
    if slot.is_unique() {
        match std::mem::take(slot).try_into_mut() {
            Ok(mut buf) => {
                if buf.capacity() < need {
                    buf.reserve(need - buf.len());
                }
                if buf.len() < need {
                    buf.resize(need, 0);
                }
                return buf;
            }
            Err(shared) => *slot = shared,
        }
    }
    BytesMut::zeroed(need)
}

/// The first `len` bytes, or `None` when `bytes` is shorter. A handle only:
/// the bytes stay in `bytes`' allocation.
fn prefix(bytes: &Bytes, len: usize) -> Option<Bytes> {
    if len > bytes.len() {
        None
    } else {
        Some(bytes.slice(..len))
    }
}

/// Puts `buf` back when `writable_pixels` took the kept buffer (it leaves the slot
/// empty). A slot that still holds the caller's frame is left alone.
fn restore(slot: &mut Bytes, buf: impl Into<Bytes>) {
    let buf = buf.into();
    if slot.is_empty() {
        *slot = buf;
    }
}

/// Copies `height` rows of `row` bytes from `src` (stride `src_stride`) into the
/// front of `dst`. `false` when a row does not fit.
fn copy_tight(
    dst: &mut BytesMut,
    src: &[u8],
    height: usize,
    src_stride: usize,
    row: usize,
) -> bool {
    for y in 0..height {
        let Some(s) = y.checked_mul(src_stride) else {
            return false;
        };
        let Some(d) = y.checked_mul(row) else {
            return false;
        };
        let Some(s_end) = s.checked_add(row) else {
            return false;
        };
        let Some(d_end) = d.checked_add(row) else {
            return false;
        };
        let (Some(from), Some(to)) = (src.get(s..s_end), dst.get_mut(d..d_end)) else {
            return false;
        };
        to.copy_from_slice(from);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{copy_tight, padded_stride, prefix, shared_pixels, writable_pixels};
    use bytes::BytesMut;

    #[test]
    fn rows_are_padded_to_the_copy_alignment() {
        assert_eq!(padded_stride(1), 256);
        assert_eq!(padded_stride(64), 256);
        assert_eq!(padded_stride(65), 512);
    }

    /// One read into the kept buffer, as `take` does after the staging map.
    fn paint(slot: &mut super::Bytes, packed: usize, byte: u8) -> super::Bytes {
        let mut buf = writable_pixels(slot, packed);
        if let Some(dst) = buf.get_mut(..packed) {
            dst.fill(byte);
        }
        let bytes = buf.freeze();
        let Some(frame) = prefix(&bytes, packed) else {
            if slot.is_empty() {
                *slot = bytes;
            }
            return super::Bytes::new();
        };
        if slot.is_empty() || bytes.len() > slot.len() {
            *slot = bytes;
        }
        frame
    }

    #[test]
    fn the_pixel_buffer_is_reused_after_the_frame_is_dropped() {
        let mut slot = shared_pixels(16);
        let first = paint(&mut slot, 16, 1);
        assert_eq!(first.as_ref(), &[1u8; 16]);
        assert_eq!(first.as_ptr(), slot.as_ptr());
        let ptr = first.as_ptr();
        drop(first);
        let second = paint(&mut slot, 16, 2);
        assert_eq!(second.as_ptr(), ptr);
        assert_eq!(slot.as_ptr(), ptr);
        assert_eq!(second.as_ref(), &[2u8; 16]);
    }

    #[test]
    fn a_held_frame_is_not_overwritten() {
        let mut slot = shared_pixels(16);
        let held = paint(&mut slot, 16, 7);
        assert_eq!(held.as_ptr(), slot.as_ptr());
        let next = paint(&mut slot, 16, 8);
        assert_eq!(held.as_ref(), &[7u8; 16]);
        assert_eq!(next.as_ref(), &[8u8; 16]);
        assert_ne!(held.as_ptr(), next.as_ptr());
        assert_eq!(slot.as_ptr(), held.as_ptr());
    }

    #[test]
    fn a_larger_picture_grows_the_buffer_and_a_smaller_one_reuses_it() {
        let mut slot = shared_pixels(8);
        drop(paint(&mut slot, 8, 1));
        let big = paint(&mut slot, 32, 2);
        assert_eq!(big.len(), 32);
        assert!(big.iter().all(|b| *b == 2));
        assert_eq!(big.as_ptr(), slot.as_ptr());
        let ptr = slot.as_ptr();
        drop(big);
        let again = paint(&mut slot, 8, 3);
        assert_eq!(again.len(), 8);
        assert!(again.iter().all(|b| *b == 3));
        assert_eq!(again.as_ptr(), ptr);
        assert_eq!(slot.as_ptr(), ptr);
        assert!(slot.len() >= 32);
    }

    #[test]
    fn tight_rows_drop_the_staging_padding() {
        let mut src = vec![0u8; 256 * 2];
        src[..4].copy_from_slice(&[1, 2, 3, 4]);
        src[256..260].copy_from_slice(&[5, 6, 7, 8]);
        let mut dst = BytesMut::zeroed(8);
        assert!(copy_tight(&mut dst, &src, 2, 256, 4));
        assert_eq!(&dst[..], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(!copy_tight(&mut dst, &[1, 2, 3], 1, 4, 4));
    }
}
