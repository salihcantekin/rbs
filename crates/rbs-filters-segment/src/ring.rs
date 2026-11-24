//! Exact mode's pictures (ADR-0183): the node copies each new input picture, on the GPU,
//! into a texture of its own and keeps the last few, so the mask made from a picture is
//! drawn on that picture. The copies are the node's, never leases on the source's
//! textures: a source has few pictures in flight (the Windows camera lends three), and
//! holding more than that stops the camera.

use rbs_core::media::{ColorSpace, PixelFormat, Size, VideoFormat, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_gpu::{GpuContext, TextureDesc, TextureId, TexturePool};

use crate::node::{Picture, UniformBlock, describe, draw};

/// The copy pass's shader.
pub(crate) const COPY: &str = include_str!("shaders/copy.wgsl");

/// How many pictures Exact mode keeps: the mask comes about three pictures late at 30 fps.
const KEPT: usize = 4;

/// How far behind the newest picture a held one may be before Exact gives up on it.
const STALE: std::time::Duration = std::time::Duration::from_millis(500);

/// The copy pass: pipeline, uniforms and their bind group.
pub(crate) struct CopyPass {
    pub(crate) pipeline: wgpu::RenderPipeline,
    pub(crate) buffer: wgpu::Buffer,
    pub(crate) group: wgpu::BindGroup,
}

#[derive(Clone, Copy)]
struct Kept {
    id: TextureId,
    size: [u32; 2],
    time: Timestamp,
}

/// The last copies, oldest replaced first.
#[derive(Default)]
pub(crate) struct Ring {
    slots: [Option<Kept>; KEPT],
    next: usize,
    last: Option<Timestamp>,
}

impl Ring {
    /// Gives every copy back to the pool (a lease on one, `held`, keeps it alive).
    pub(crate) fn clear(&mut self, pool: &mut TexturePool) {
        for slot in &mut self.slots {
            if let Some(kept) = slot.take() {
                let _ = pool.release(kept.id);
            }
        }
        self.next = 0;
        self.last = None;
    }

    /// Exact: keeps a copy of `picture` unless it is the one kept last. Fast: keeps none.
    pub(crate) fn sync(
        &mut self,
        exact: bool,
        held: &mut Option<VideoFrame>,
        (gpu, pool, copy): (&GpuContext, &mut TexturePool, &CopyPass),
        picture: Picture,
    ) {
        if !exact {
            self.clear(pool);
            *held = None;
            return;
        }
        let (input, size, decode, time) = picture;
        if self.last == Some(time) {
            return;
        }
        if self.slots.iter().flatten().any(|k| k.size != size) {
            self.clear(pool);
            *held = None;
        }
        let Ok(id) = pool.acquire(gpu, &TextureDesc::canvas(size[0], size[1])) else {
            return;
        };
        let (Some(src), Some(target)) = (
            pool.get(input).and_then(|t| t.sampled_bind_group()),
            pool.get(id),
        ) else {
            let _ = pool.release(id);
            return;
        };
        let mut u: UniformBlock = [[0.0; 4]; 16];
        u[15] = [
            1.0 / size[0] as f32,
            1.0 / size[1] as f32,
            0.0,
            decode.code(),
        ];
        gpu.queue()
            .write_buffer(&copy.buffer, 0, bytemuck::bytes_of(&u));
        let mut encoder = gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rbs-filters-segment copy"),
            });
        draw(
            &mut encoder,
            &copy.pipeline,
            target.view(),
            &copy.group,
            src,
            None,
        );
        gpu.queue().submit(Some(encoder.finish()));
        let old = self.slots[self.next].replace(Kept { id, size, time });
        if let Some(old) = old {
            let _ = pool.release(old.id);
        }
        self.next = (self.next + 1) % KEPT;
        self.last = Some(time);
    }

    fn find(&self, time: Timestamp) -> Option<Kept> {
        self.slots
            .iter()
            .flatten()
            .find(|k| k.time == time)
            .copied()
    }
}

/// The picture to draw this tick. Fast: the current one. Exact: the copy a new matte was
/// made from, and until the next matte the same one again.
pub(crate) fn picture_to_draw(
    held: &mut Option<VideoFrame>,
    ring: &Ring,
    pool: &mut TexturePool,
    exact: bool,
    matte_time: Option<Timestamp>,
    current: Picture,
) -> Picture {
    if !exact {
        *held = None;
        return current;
    }
    if let Some(time) = matte_time
        && let Some(kept) = ring.find(time)
    {
        let format = VideoFormat::new(
            Size::new(kept.size[0], kept.size[1]),
            PixelFormat::Rgba16Float,
            ColorSpace::LINEAR_BT709,
        );
        if let Ok(frame) = pool.frame(kept.id, format, kept.time) {
            *held = Some(frame);
        }
    }
    // Masks stopped coming (a stalled or busy session): show the newest picture again
    // instead of one old picture for ever.
    if held
        .as_ref()
        .is_some_and(|f| current.3.saturating_since(f.timestamp()) > STALE)
    {
        *held = None;
    }
    held.as_ref()
        .and_then(|frame| describe(pool, frame))
        .unwrap_or(current)
}
