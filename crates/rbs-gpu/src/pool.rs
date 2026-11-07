//! Reusable GPU textures addressed by small, generation-checked ids, with leases.
//!
//! Hot path (`docs/architecture/hot-paths.md`): `acquire` of a free texture, `release`,
//! `get` and `lease` do not allocate. Creating a new texture allocates and happens only
//! when no free texture of the same description exists.
//!
//! Ids are [`rbs_core::media::TextureId`]. Consumers on other threads (for example
//! encoders) keep a slot alive with a [`Lease`]: the pool does not reuse a slot while
//! any lease on it exists, even after the owner released its own handle.
//! Spec: `docs/specs/gpu-compositor.md#texture-pool`.
//!
//! This is a marked low-level module (principle P11): it implements the `unsafe`
//! `LeaseOwner` contract of `rbs_core::media`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rbs_core::media::{Lease, LeaseOwner, TextureId, VideoFormat, VideoFrame};
use rbs_core::time::Timestamp;

use crate::context::GpuContext;
use crate::error::GpuError;

/// Slots a pool holds when created with [`TexturePool::new`].
pub const DEFAULT_POOL_SLOTS: u32 = 256;

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
    /// Second plane of a multi-planar texture (NV12 chroma); `view` is then plane 0.
    plane1: Option<wgpu::TextureView>,
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

    /// The default view of the whole texture, or of plane 0 for a multi-planar format.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// The view of one plane: plane 0 is [`Self::view`]; plane 1 exists only for
    /// multi-planar formats such as NV12.
    pub fn plane_view(&self, plane: u32) -> Option<&wgpu::TextureView> {
        match plane {
            0 => Some(&self.view),
            1 => self.plane1.as_ref(),
            _ => None,
        }
    }

    /// The bind group of this texture and the context's linear clamp sampler for
    /// [`GpuContext::sampled_layout`], when the texture is sampleable and filterable.
    /// Filter nodes bind pool textures with it, as the compositor does (ADR-0062).
    pub fn sampled_bind_group(&self) -> Option<&wgpu::BindGroup> {
        self.sampled.as_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No owner handle and no leases; on a free list (or empty after `trim`).
    Free,
    /// Handed out by `acquire`; the owner has not released it.
    InUse,
    /// The owner released it but leases are still open; reused once they are dropped.
    Retired,
}

struct Slot {
    generation: u32,
    state: State,
    texture: Option<PooledTexture>,
    /// Wraps a texture the pool did not create (native import). Dropped instead of
    /// reused when it becomes free.
    external: bool,
    /// Given back with [`TexturePool::forget_texture`]: dropped instead of reused when
    /// it becomes free.
    forgotten: bool,
}

/// Free slot indices for one texture description.
struct FreeList {
    desc: TextureDesc,
    slots: Vec<u32>,
    /// Textures of this description that hold GPU memory (free, in use or retired).
    total: usize,
    /// The pool's tick (`trim_idle`) when `acquire` or `touch` last asked for this
    /// description.
    last_used: u64,
}

/// Lease counts, shared with consumers on other threads.
///
/// Tokens are the slot index. A slot is reused only when its count is zero and its owner
/// handle was released, so no lease from an earlier use of the slot can exist and the
/// token needs no generation.
struct Leases {
    counts: Box<[AtomicU32]>,
}

// SAFETY: a slot's texture is dropped or reused only by the owning `TexturePool`, and
// only when the slot's count is zero (checked with `Acquire` in `collect`/`release`,
// paired with the `Release` decrement here). `retain` is only reachable through a
// `Lease` clone, which already holds a reference, so a count never rises from zero
// behind the pool's back.
unsafe impl LeaseOwner for Leases {
    fn retain(&self, token: u64) {
        if let Some(count) = self.counts.get(token as usize) {
            // Relaxed: the caller already holds a reference, so nothing is published.
            count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn release(&self, token: u64) {
        if let Some(count) = self.counts.get(token as usize) {
            // Release: the consumer's use of the slot happens before the pool reuses it.
            count.fetch_sub(1, Ordering::Release);
        }
    }
}

/// A pool of textures owned and mutated by one render thread.
///
/// A new pool owns no GPU memory (P13); it holds one counter per slot of its capacity.
pub struct TexturePool {
    slots: Vec<Slot>,
    capacity: u32,
    free: Vec<FreeList>,
    /// Slots whose texture was trimmed and can hold a new one.
    empty: Vec<u32>,
    /// Slots released by their owner while leases were open.
    retired: Vec<u32>,
    leases: Arc<Leases>,
    /// The tick `trim_idle` was last called with.
    now: u64,
}

impl Default for TexturePool {
    fn default() -> Self {
        Self::new()
    }
}

impl TexturePool {
    /// An empty pool with [`DEFAULT_POOL_SLOTS`] slots.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_POOL_SLOTS)
    }

    /// An empty pool that can hold at most `capacity` textures.
    pub fn with_capacity(capacity: u32) -> Self {
        Self {
            slots: Vec::with_capacity(capacity as usize),
            capacity,
            free: Vec::new(),
            empty: Vec::with_capacity(capacity as usize),
            retired: Vec::with_capacity(capacity as usize),
            leases: Arc::new(Leases {
                counts: (0..capacity).map(|_| AtomicU32::new(0)).collect(),
            }),
            now: 0,
        }
    }

    /// Returns a texture matching `desc`, reusing a free one when possible.
    ///
    /// Reuse is O(distinct descriptions + retired slots) and does not allocate.
    pub fn acquire(&mut self, ctx: &GpuContext, desc: &TextureDesc) -> Result<TextureId, GpuError> {
        self.collect();
        let now = self.now;
        if let Some(index) = self
            .free
            .iter_mut()
            .find(|f| f.desc == *desc)
            .and_then(|f| {
                f.last_used = now;
                f.slots.pop()
            })
        {
            let slot = &mut self.slots[index as usize];
            slot.state = State::InUse;
            return Ok(TextureId::new(index, slot.generation));
        }
        self.create(ctx, desc)
    }

    /// Moves retired slots whose leases are all gone to their free lists.
    fn collect(&mut self) {
        let mut i = 0;
        while i < self.retired.len() {
            let index = self.retired[i];
            // Acquire: pairs with the `Release` decrement of the last lease.
            if self.leases.counts[index as usize].load(Ordering::Acquire) == 0 {
                self.retired.swap_remove(i);
                self.free_slot(index);
            } else {
                i += 1;
            }
        }
    }

    fn free_slot(&mut self, index: u32) {
        let slot = &mut self.slots[index as usize];
        slot.state = State::Free;
        slot.generation = slot.generation.wrapping_add(1);
        if slot.external || slot.forgotten {
            // Not reusable: drop the wrapper and recycle the slot. Capacity of `empty`
            // is the pool capacity, so this push does not allocate.
            let desc = slot.texture.take().map(|t| t.desc);
            let owned = !slot.external;
            slot.external = false;
            slot.forgotten = false;
            self.empty.push(index);
            if let (true, Some(desc)) = (owned, desc) {
                self.uncount(&desc);
            }
            return;
        }
        let Some(desc) = slot.texture.as_ref().map(|t| t.desc) else {
            return;
        };
        let Some(at) = self.free.iter().position(|f| f.desc == desc) else {
            return;
        };
        // Capacity was reserved in `create`, so this push does not allocate.
        self.free[at].slots.push(index);
    }

    /// One texture of `desc` the pool created is gone: count it out of its list and
    /// remove the list when it holds none.
    fn uncount(&mut self, desc: &TextureDesc) {
        if let Some(at) = self.free.iter().position(|f| f.desc == *desc) {
            let list = &mut self.free[at];
            list.total = list.total.saturating_sub(1);
            if list.total == 0 {
                self.free.swap_remove(at);
            }
        }
    }

    /// Drops every free texture of `self.free[at]`; removes the list when it holds no
    /// texture any more. Does not allocate.
    fn drop_free(&mut self, at: usize) {
        let list = &mut self.free[at];
        list.total -= list.slots.len();
        for index in list.slots.drain(..) {
            if let Some(slot) = self.slots.get_mut(index as usize) {
                // Capacity of `empty` is the pool capacity, so this push does not
                // allocate; dropping the texture frees its GPU memory.
                slot.texture = None;
                self.empty.push(index);
            }
        }
        if list.total == 0 {
            self.free.swap_remove(at);
        }
    }

    /// Makes room for one texture when every slot holds one: drops the free textures
    /// of the description acquired least recently, other than `keep` and never one
    /// asked for in the current tick (so two busy descriptions never take turns
    /// evicting each other). Returns whether a slot is now empty.
    fn evict(&mut self, keep: &TextureDesc) -> bool {
        while self.empty.is_empty() {
            let victim = self
                .free
                .iter()
                .enumerate()
                .filter(|(_, f)| f.desc != *keep && !f.slots.is_empty() && f.last_used < self.now)
                .min_by_key(|(_, f)| f.last_used)
                .map(|(i, _)| i);
            match victim {
                Some(at) => self.drop_free(at),
                None => return false,
            }
        }
        true
    }

    fn create(&mut self, ctx: &GpuContext, desc: &TextureDesc) -> Result<TextureId, GpuError> {
        let max = ctx.max_texture_dimension();
        if desc.width == 0 || desc.height == 0 || desc.width > max || desc.height > max {
            return Err(GpuError::InvalidSize {
                width: desc.width,
                height: desc.height,
            });
        }
        if self.empty.is_empty() && self.slots.len() >= self.capacity as usize && !self.evict(desc)
        {
            return Err(GpuError::PoolFull {
                capacity: self.capacity,
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
        let index = self.place(wrap(ctx, texture, *desc), false);
        // Reserve room in the free list now so releasing never grows it on the hot path.
        let list = match self.free.iter_mut().position(|f| f.desc == *desc) {
            Some(i) => &mut self.free[i],
            None => {
                self.free.push(FreeList {
                    desc: *desc,
                    slots: Vec::new(),
                    total: 0,
                    last_used: self.now,
                });
                let last = self.free.len() - 1;
                &mut self.free[last]
            }
        };
        list.total += 1;
        let missing = list.total - list.slots.len();
        list.slots.reserve(missing);
        Ok(TextureId::new(index, self.slots[index as usize].generation))
    }

    /// Puts a texture into an empty or new slot, marked InUse. Capacity was checked.
    fn place(&mut self, pooled: PooledTexture, external: bool) -> u32 {
        match self.empty.pop() {
            Some(index) => {
                let slot = &mut self.slots[index as usize];
                slot.state = State::InUse;
                slot.texture = Some(pooled);
                slot.external = external;
                slot.forgotten = false;
                index
            }
            None => {
                // Bounded by `capacity`, which is a `u32`.
                let index = self.slots.len() as u32;
                self.slots.push(Slot {
                    generation: 0,
                    state: State::InUse,
                    texture: Some(pooled),
                    external,
                    forgotten: false,
                });
                index
            }
        }
    }

    /// Adds a texture the pool did not create (an imported native surface) as an InUse
    /// slot. When released and unleased, the slot drops the texture instead of reusing it.
    /// Allocates; call when a surface is first seen, not per frame.
    pub fn insert_external(
        &mut self,
        ctx: &GpuContext,
        texture: wgpu::Texture,
        desc: TextureDesc,
    ) -> Result<TextureId, GpuError> {
        self.collect();
        if self.empty.is_empty() && self.slots.len() >= self.capacity as usize && !self.evict(&desc)
        {
            return Err(GpuError::PoolFull {
                capacity: self.capacity,
            });
        }
        let index = self.place(wrap(ctx, texture, desc), true);
        Ok(TextureId::new(index, self.slots[index as usize].generation))
    }

    /// Open leases on the slot of `id` (0 for stale ids). For importers deciding when a
    /// producer's surface is no longer read.
    pub fn lease_count(&self, id: TextureId) -> u32 {
        if self.get(id).is_none() {
            return 0;
        }
        // Acquire: pairs with the `Release` decrement of the last lease.
        self.leases.counts[id.slot() as usize].load(Ordering::Acquire)
    }

    /// Gives up the owner's handle. The slot is reused once every lease on it is dropped;
    /// until then `id` stays valid for lease holders. A second release is an error.
    pub fn release(&mut self, id: TextureId) -> Result<(), GpuError> {
        let slot = self
            .slots
            .get(id.slot() as usize)
            .ok_or(GpuError::UnknownTexture(id))?;
        if slot.state != State::InUse || slot.generation != id.generation() {
            return Err(GpuError::StaleTexture(id));
        }
        // Acquire: pairs with the `Release` decrement of the last lease.
        if self.leases.counts[id.slot() as usize].load(Ordering::Acquire) == 0 {
            self.free_slot(id.slot());
        } else {
            self.slots[id.slot() as usize].state = State::Retired;
            // Capacity is the pool capacity, so this push does not allocate.
            self.retired.push(id.slot());
        }
        Ok(())
    }

    /// A lease that keeps the slot of `id` from being reused. Does not allocate.
    pub fn lease(&self, id: TextureId) -> Result<Lease, GpuError> {
        if self.get(id).is_none() {
            return Err(GpuError::StaleTexture(id));
        }
        self.leases.retain(u64::from(id.slot()));
        let owner: Arc<dyn LeaseOwner> = self.leases.clone();
        // SAFETY: one reference to this slot was counted just above and is handed over.
        Ok(unsafe { Lease::adopt(owner, u64::from(id.slot())) })
    }

    /// A video frame for `id` that holds a lease on its slot.
    pub fn frame(
        &self,
        id: TextureId,
        format: VideoFormat,
        timestamp: Timestamp,
    ) -> Result<VideoFrame, GpuError> {
        let lease = self.lease(id)?;
        Ok(VideoFrame::texture(format, timestamp, id, Some(lease)))
    }

    /// The texture behind `id`, or `None` when the id is stale or unknown.
    pub fn get(&self, id: TextureId) -> Option<&PooledTexture> {
        let slot = self.slots.get(id.slot() as usize)?;
        if slot.state != State::Free && slot.generation == id.generation() {
            slot.texture.as_ref()
        } else {
            None
        }
    }

    /// Drops every free texture, releasing its GPU memory. Not for real-time threads.
    pub fn trim(&mut self) {
        self.collect();
        for at in (0..self.free.len()).rev() {
            self.drop_free(at);
        }
    }

    /// Ends a tick of the pool's own clock and drops the free textures of every
    /// description that [`TexturePool::acquire`] or [`TexturePool::touch`] has not asked
    /// for in the last `idle_ticks` calls (ADR-0062 decision 5); textures in use or
    /// retired are kept, and so are descriptions asked for since. Retired textures whose
    /// readers let go are freed first. The clock counts calls, not tick indices, so a
    /// tick index that jumps after a stall never makes everything look idle at once;
    /// `acquire` and `touch` stamp descriptions with it until the next call.
    ///
    /// Called by the render runtime at the end of every tick: one pass over the retired
    /// list and one comparison per description; it frees GPU memory only on the call a
    /// description turns idle (P13). Dropping a texture is the stated exception of
    /// ADR-0062 decision 5, not a per-tick cost.
    pub fn trim_idle(&mut self, idle_ticks: u64) {
        self.now = self.now.wrapping_add(1);
        let now = self.now;
        self.collect();
        let idle =
            |f: &FreeList| !f.slots.is_empty() && now.saturating_sub(f.last_used) > idle_ticks;
        if !self.free.iter().any(idle) {
            return;
        }
        for at in (0..self.free.len()).rev() {
            if idle(&self.free[at]) {
                self.drop_free(at);
            }
        }
    }

    /// Marks `desc` as used now, so [`TexturePool::trim_idle`] keeps its free textures:
    /// for a node that skips a tick but will acquire again. O(descriptions); does not
    /// allocate.
    pub fn touch(&mut self, desc: &TextureDesc) {
        let now = self.now;
        if let Some(list) = self.free.iter_mut().find(|f| f.desc == *desc) {
            list.last_used = now;
        }
    }

    /// Drops up to `count` free textures of `desc` a node no longer needs (the spares
    /// of a size its input left). Textures are interchangeable, so a node gives back
    /// the free ones it kept; see [`TexturePool::forget_texture`] for one still read.
    /// Does not allocate.
    pub fn forget(&mut self, desc: &TextureDesc, count: usize) {
        self.collect();
        let Some(at) = self.free.iter().position(|f| f.desc == *desc) else {
            return;
        };
        let list = &mut self.free[at];
        let drop = list.slots.len().min(count);
        list.total -= drop;
        for _ in 0..drop {
            if let Some(index) = list.slots.pop() {
                if let Some(slot) = self.slots.get_mut(index as usize) {
                    // Capacity of `empty` is the pool capacity: no allocation.
                    slot.texture = None;
                    self.empty.push(index);
                }
            }
        }
        if list.total == 0 {
            self.free.swap_remove(at);
        }
    }

    /// Gives back the texture of `id`, which its owner released and readers may still
    /// hold (a filter's last output): dropped at once if free, else when its last lease
    /// goes, instead of returning to the free list. Other textures of its description
    /// are not touched. A stale id does nothing. Does not allocate.
    pub fn forget_texture(&mut self, id: TextureId) {
        self.collect();
        let Some(slot) = self.slots.get_mut(id.slot() as usize) else {
            return;
        };
        if slot.texture.is_none() || slot.external {
            return;
        }
        match slot.state {
            State::InUse | State::Retired if slot.generation == id.generation() => {
                slot.forgotten = true;
            }
            State::Free if slot.generation == id.generation().wrapping_add(1) => {
                // Freed since the owner released it, and not reused: drop it now.
                let desc = slot.texture.as_ref().map(|t| t.desc);
                if let Some(desc) = desc {
                    if let Some(at) = self.free.iter().position(|f| f.desc == desc) {
                        if let Some(pos) = self.free[at].slots.iter().position(|&i| i == id.slot())
                        {
                            self.free[at].slots.swap_remove(pos);
                            self.slots[id.slot() as usize].texture = None;
                            self.empty.push(id.slot());
                            self.uncount(&desc);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Descriptions the pool tracks (with at least one texture). For tests and
    /// diagnostics.
    pub fn descriptions(&self) -> usize {
        self.free.len()
    }

    /// Number of textures that hold GPU memory (free, in use or retired).
    pub fn allocated(&self) -> usize {
        self.slots.iter().filter(|s| s.texture.is_some()).count()
    }

    /// Number of textures not free: handed out, or released with leases still open.
    pub fn in_use(&self) -> usize {
        self.slots.iter().filter(|s| s.state != State::Free).count()
    }
}

/// Builds the pooled wrapper: default view (or one view per plane) and, when sampleable,
/// the sampled bind group.
fn wrap(ctx: &GpuContext, texture: wgpu::Texture, desc: TextureDesc) -> PooledTexture {
    if desc.format.is_multi_planar_format() {
        let plane = |aspect| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                aspect,
                format: desc.format.aspect_specific_format(aspect),
                ..Default::default()
            })
        };
        return PooledTexture {
            desc,
            view: plane(wgpu::TextureAspect::Plane0),
            plane1: Some(plane(wgpu::TextureAspect::Plane1)),
            texture,
            sampled: None,
        };
    }
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let filterable = desc.format.sample_type(None, Some(ctx.device().features()))
        == Some(wgpu::TextureSampleType::Float { filterable: true });
    let sampled =
        (desc.usage.contains(wgpu::TextureUsages::TEXTURE_BINDING) && filterable).then(|| {
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
    PooledTexture {
        desc,
        texture,
        view,
        plane1: None,
        sampled,
    }
}
