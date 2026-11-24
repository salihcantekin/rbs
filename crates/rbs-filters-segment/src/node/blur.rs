//! The composite pass and the blurred background (child of `node`).
//!
//! Blur mode draws the room in three small passes at 1/8 size: an 8x8 average weighted by
//! the background (no person colour in the room), then a Gaussian along x and along y.
//! The composite pass reads the result with a bilinear sampler, so the blur is smooth.
//! The two small textures are taken from the pool for one draw only.

use super::*;

const BLUR_DOWN: &str = include_str!("../shaders/blur_down.wgsl");
const BLUR_H: &str = include_str!("../shaders/blur_h.wgsl");
const BLUR_V: &str = include_str!("../shaders/blur_v.wgsl");

/// How much smaller the blurred room is than the picture.
const SCALE: u32 = 8;

/// The three blur pipelines and their uniforms.
pub(super) struct BlurPass {
    down: wgpu::RenderPipeline,
    horizontal: wgpu::RenderPipeline,
    vertical: wgpu::RenderPipeline,
    buffers: [wgpu::Buffer; 3],
    groups: [wgpu::BindGroup; 3],
}

impl BlurPass {
    pub(super) fn new(
        gpu: &GpuContext,
        uniforms: &wgpu::BindGroupLayout,
        layout: &wgpu::PipelineLayout,
    ) -> Result<Self, NodeError> {
        let device = gpu.device();
        let (b0, g0) = uniform(device, uniforms);
        let (b1, g1) = uniform(device, uniforms);
        let (b2, g2) = uniform(device, uniforms);
        let format = rbs_gpu::CANVAS_FORMAT;
        Ok(Self {
            down: pipeline(gpu, layout, BLUR_DOWN, format)?,
            horizontal: pipeline(gpu, layout, BLUR_H, format)?,
            vertical: pipeline(gpu, layout, BLUR_V, format)?,
            buffers: [b0, b1, b2],
            groups: [g0, g1, g2],
        })
    }
}

fn small_size(size: [u32; 2]) -> [u32; 2] {
    [
        size[0].div_ceil(SCALE).max(1),
        size[1].div_ceil(SCALE).max(1),
    ]
}

/// Writes the blur passes' uniforms (with the composite's, when they change).
pub(super) fn write(
    gpu: &GpuContext,
    prepared: &Prepared,
    (edge, look): ([f32; 2], Look),
    size: [u32; 2],
    decode: Transfer,
) {
    let small = small_size(size);
    let mut down: UniformBlock = [[0.0; 4]; 16];
    down[0] = [edge[0], edge[1], 0.0, 0.0];
    down[15] = [
        1.0 / size[0] as f32,
        1.0 / size[1] as f32,
        0.0,
        decode.code(),
    ];
    let mut gauss: UniformBlock = [[0.0; 4]; 16];
    gauss[2] = [0.0, look.blur[0], look.blur[1], 0.0];
    gauss[15] = [1.0 / small[0] as f32, 1.0 / small[1] as f32, 0.0, 0.0];
    let queue = gpu.queue();
    let blur = &prepared.blur;
    queue.write_buffer(&blur.buffers[0], 0, bytemuck::bytes_of(&down));
    queue.write_buffer(&blur.buffers[1], 0, bytemuck::bytes_of(&gauss));
    queue.write_buffer(&blur.buffers[2], 0, bytemuck::bytes_of(&gauss));
}

/// One full-target pass with bind groups 0, 1, ... in the order given.
fn pass(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    target: &wgpu::TextureView,
    groups: &[&wgpu::BindGroup],
) {
    let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("rbs-filters-segment pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
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
    rp.set_pipeline(pipeline);
    for (index, group) in groups.iter().enumerate() {
        rp.set_bind_group(index as u32, *group, &[]);
    }
    rp.draw(0..3, 0..1);
}

/// Draws the input times the matte into `output`; with `room`, the blurred room of the
/// input (in the first of the two small textures) shows behind the person.
fn composite(
    gpu: &GpuContext,
    pool: &TexturePool,
    prepared: &Prepared,
    (input, matte): (TextureId, TextureId),
    output: TextureId,
    room: Option<(TextureId, TextureId)>,
) -> Result<(), GpuError> {
    let sampled = |id: TextureId| {
        pool.get(id)
            .and_then(|t| t.sampled_bind_group())
            .ok_or(GpuError::StaleTexture(id))
    };
    let (src, mask) = (sampled(input)?, sampled(matte)?);
    let target = pool.get(output).ok_or(GpuError::StaleTexture(output))?;
    let mut encoder = gpu
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("rbs-filters-segment composite"),
        });
    let blur = &prepared.blur;
    let mut behind = mask;
    if let Some((a, b)) = room {
        let (a_group, b_group) = (sampled(a)?, sampled(b)?);
        let a_view = pool.get(a).ok_or(GpuError::StaleTexture(a))?.view();
        let b_view = pool.get(b).ok_or(GpuError::StaleTexture(b))?.view();
        let g = &blur.groups;
        pass(&mut encoder, &blur.down, a_view, &[&g[0], src, mask]);
        pass(
            &mut encoder,
            &blur.horizontal,
            b_view,
            &[&g[1], a_group, mask],
        );
        pass(
            &mut encoder,
            &blur.vertical,
            a_view,
            &[&g[2], b_group, mask],
        );
        behind = a_group;
    }
    pass(
        &mut encoder,
        &prepared.comp,
        target.view(),
        &[&prepared.comp_group, src, mask, behind],
    );
    gpu.queue().submit(Some(encoder.finish()));
    Ok(())
}

/// Draws the composite into a new output and makes the frame; the frame's lease keeps
/// the output alive and the pool reuses the rest at once. A failed draw is returned
/// inside `Ok` so the caller can publish nothing; a pool failure is the node's.
pub(super) fn render_output(
    gpu: &GpuContext,
    pool: &mut TexturePool,
    prepared: &Prepared,
    (input, matte): (TextureId, TextureId),
    size: [u32; 2],
    time: Timestamp,
    blurred: bool,
) -> Result<Result<(TextureId, VideoFrame), GpuError>, NodeError> {
    let output = pool
        .acquire(gpu, &TextureDesc::canvas(size[0], size[1]))
        .map_err(backend)?;
    let mut room = None;
    if blurred {
        let small = small_size(size);
        let desc = TextureDesc::canvas(small[0], small[1]);
        let first = pool.acquire(gpu, &desc);
        let second = pool.acquire(gpu, &desc);
        match (first, second) {
            (Ok(a), Ok(b)) => room = Some((a, b)),
            (first, second) => {
                for id in [first, second].into_iter().flatten() {
                    let _ = pool.release(id);
                }
                pool.release(output).map_err(backend)?;
                return Err(NodeError::Backend("texture pool full"));
            }
        }
    }
    let result = composite(gpu, pool, prepared, (input, matte), output, room).and_then(|()| {
        let format = VideoFormat::new(
            Size::new(size[0], size[1]),
            PixelFormat::Rgba16Float,
            ColorSpace::LINEAR_BT709,
        );
        pool.frame(output, format, time)
    });
    if let Some((a, b)) = room {
        let _ = pool.release(a);
        let _ = pool.release(b);
    }
    pool.release(output).map_err(backend)?;
    Ok(result.map(|frame| (output, frame)))
}
