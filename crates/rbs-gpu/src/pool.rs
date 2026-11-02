//! Reusable GPU textures addressed by small, generation-checked ids.
//!
//! Hot path (`docs/architecture/hot-paths.md`): `acquire` of a free texture, `release`
//! and `get` do not allocate. Creating a new texture allocates and happens only when
//! no free texture of the same description exists.

use crate::context::GpuContext;
use crate::error::GpuError;

/// An opaque handle to a pooled texture: slot index plus generation.
///
/// Releasing a texture bumps its slot generation, so every copy of the old id
/// becomes stale and is detected in O(1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureId {
    index: u32,
    generation: u32,
}

impl TextureId {
    /// The slot index.
    pub fn index(self) -> u32 {
        self.index
    }

    /// The slot generation this id was handed out with.
    pub fn generation(self) -> u32 {
        self.generation
    }
}

/// What a pooled texture looks like. Textures are reused only for an equal description.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureDesc {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Texel format.
    pub format: wgpu::TextureFormat,
    /// How the texture may be used.
    pub usage: wgpu::TextureUsages,
}

impl TextureDesc {
    /// A canvas render target: `Rgba16Float`, renderable, sampleable and copyable.
    pub fn canvas(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            format: crate::CANVAS_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
        }
    }

    /// A source texture that is filled by a copy and sampled by the compositor.
    pub fn source(width: u32, height: u32, format: wgpu::TextureFormat) -> Self {
        Self {
            width,
            height,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        }
    }
}

/// A texture owned by the pool, with its default view and, when sampleable, a bind group
/// for the compositor's sampled-texture layout.
#[derive(Debug)]
pub struct PooledTexture {
    desc: TextureDesc,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    sampled: Option<wgpu::BindGroup>,
}

impl PooledTexture {
    /// The description the texture was created with.
    pub fn desc(&self) -> &TextureDesc {
        &self.desc
    }

    /// The wgpu texture.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// The default view of the whole texture.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub(crate) fn sampled_bind_group(&self) -> Option<&wgpu::BindGroup> {
        self.sampled.as_ref()
    }
}

struct Slot {
    generation: u32,
    in_use: bool,
    texture: Option<PooledTexture>,
}

/// Free slot indices for one texture description.
struct FreeList {
    desc: TextureDesc,
    slots: Vec<u32>,
    /// Textures of this description that hold GPU memory (free or in use).
    total: usize,
}

/// A pool of textures owned by one render thread (`Send`, not shared).
///
/// A new pool owns no GPU memory (P13).
#[derive(Default)]
pub struct TexturePool {
    slots: Vec<Slot>,
    free: Vec<FreeList>,
    /// Slots whose texture was trimmed and can hold a new one.
    empty: Vec<u32>,
}

impl TexturePool {
    /// An empty pool.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a texture matching `desc`, reusing a free one when possible.
    ///
    /// Reuse is O(number of distinct descriptions) and does not allocate.
    pub fn acquire(&mut self, ctx: &GpuContext, desc: &TextureDesc) -> Result<TextureId, GpuError> {
        if let Some(index) = self
            .free
            .iter_mut()
            .find(|f| f.desc == *desc)
            .and_then(|f| f.slots.pop())
        {
            let slot = &mut self.slots[index as usize];
            slot.in_use = true;
            return Ok(TextureId {
                index,
                generation: slot.generation,
            });
        }
        self.create(ctx, desc)
    }

    fn create(&mut self, ctx: &GpuContext, desc: &TextureDesc) -> Result<TextureId, GpuError> {
        let max = ctx.max_texture_dimension();
        if desc.width == 0 || desc.height == 0 || desc.width > max || desc.height > max {
            return Err(GpuError::InvalidSize {
                width: desc.width,
                height: desc.height,
            });
        }
        let texture = ctx.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("rbs-gpu pooled"),
            size: wgpu::Extent3d {
                width: desc.width,
                height: desc.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: desc.format,
            usage: desc.usage,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let filterable = desc.format.sample_type(None, Some(ctx.device().features()))
            == Some(wgpu::TextureSampleType::Float { filterable: true });
        let sampled = (desc.usage.contains(wgpu::TextureUsages::TEXTURE_BINDING) && filterable)
            .then(|| {
                ctx.device().create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("rbs-gpu sampled"),
                    layout: &ctx.sampled_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&ctx.linear_sampler),
                        },
                    ],
                })
            });
        let pooled = PooledTexture {
            desc: *desc,
            texture,
            view,
            sampled,
        };

        let index = match self.empty.pop() {
            Some(index) => {
                let slot = &mut self.slots[index as usize];
                slot.in_use = true;
                slot.texture = Some(pooled);
                index
            }
            None => {
                let index = u32::try_from(self.slots.len()).map_err(|_| GpuError::InvalidSize {
                    width: desc.width,
                    height: desc.height,
                })?;
                self.slots.push(Slot {
                    generation: 0,
                    in_use: true,
                    texture: Some(pooled),
                });
                index
            }
        };
        // Reserve room in the free list now so `release` never grows it on the hot path.
        let list = match self.free.iter_mut().position(|f| f.desc == *desc) {
            Some(i) => &mut self.free[i],
            None => {
                self.free.push(FreeList {
                    desc: *desc,
                    slots: Vec::new(),
                    total: 0,
                });
                let last = self.free.len() - 1;
                &mut self.free[last]
            }
        };
        list.total += 1;
        let missing = list.total - list.slots.len();
        list.slots.reserve(missing);
        Ok(TextureId {
            index,
            generation: self.slots[index as usize].generation,
        })
    }

    /// Returns the texture to the pool. Every copy of `id` becomes stale.
    pub fn release(&mut self, id: TextureId) -> Result<(), GpuError> {
        let slot = self
            .slots
            .get_mut(id.index as usize)
            .ok_or(GpuError::UnknownTexture(id))?;
        if !slot.in_use || slot.generation != id.generation {
            return Err(GpuError::StaleTexture(id));
        }
        let Some(desc) = slot.texture.as_ref().map(|t| t.desc) else {
            return Err(GpuError::StaleTexture(id));
        };
        slot.in_use = false;
        slot.generation = slot.generation.wrapping_add(1);
        if let Some(list) = self.free.iter_mut().find(|f| f.desc == desc) {
            // Capacity was reserved in `create`, so this push does not allocate.
            list.slots.push(id.index);
        }
        Ok(())
    }

    /// The texture behind `id`, or `None` when the id is stale or unknown.
    pub fn get(&self, id: TextureId) -> Option<&PooledTexture> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.in_use && slot.generation == id.generation {
            slot.texture.as_ref()
        } else {
            None
        }
    }

    /// Drops every free texture, releasing its GPU memory. Not for real-time threads.
    pub fn trim(&mut self) {
        for list in &mut self.free {
            list.total -= list.slots.len();
            for index in list.slots.drain(..) {
                if let Some(slot) = self.slots.get_mut(index as usize) {
                    slot.texture = None;
                    self.empty.push(index);
                }
            }
        }
    }

    /// Number of textures that hold GPU memory (in use or free).
    pub fn allocated(&self) -> usize {
        self.slots.iter().filter(|s| s.texture.is_some()).count()
    }

    /// Number of textures handed out and not yet released.
    pub fn in_use(&self) -> usize {
        self.slots.iter().filter(|s| s.in_use).count()
    }
}
