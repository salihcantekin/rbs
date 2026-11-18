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
//! the rows into a new buffer of the picture's size; it runs on the render thread once
//! per change and counts in `GpuMetrics::frames_cpu_copied` (P4, ADR-0038's exception
//! for image tracks).

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

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
        Ok(Self {
            pipeline,
            target: None,
            staging: staging(ctx, capacity),
            capacity,
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
    /// Copies the rows into a new buffer (once per picture change). Never waits.
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
        let p = &self.pending;
        let row = p.width as usize * 4;
        let len = u64::from(p.stride) * u64::from(p.height);
        let mut pixels = Vec::with_capacity(row * p.height as usize);
        let read = match self.staging.slice(..len).get_mapped_range() {
            Ok(mapped) => {
                for y in 0..p.height as usize {
                    let start = y * p.stride as usize;
                    pixels.extend_from_slice(&mapped[start..start + row]);
                }
                true
            }
            Err(_) => false,
        };
        self.staging.unmap();
        self.state.store(IDLE, Ordering::Release);
        if !read {
            self.stats.failed += 1;
            return Err(GpuError::Readback("image staging buffer not mapped".into()));
        }
        let format = VideoFormat::new(
            Size::new(p.width, p.height),
            PixelFormat::Rgba8,
            ColorSpace::SRGB,
        );
        let planes = [PlaneLayout {
            offset: 0,
            stride: row,
        }];
        let system = SystemFrame::new(format, Bytes::from(pixels), &planes)
            .map_err(|_| GpuError::Readback("image frame layout".into()))?;
        ctx.metrics().count_cpu_copy();
        self.stats.delivered += 1;
        Ok(Some(
            VideoFrame::system(p.timestamp, system, None).with_sequence(p.sequence),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::padded_stride;

    #[test]
    fn rows_are_padded_to_the_copy_alignment() {
        assert_eq!(padded_stride(1), 256);
        assert_eq!(padded_stride(64), 256);
        assert_eq!(padded_stride(65), 512);
    }
}
