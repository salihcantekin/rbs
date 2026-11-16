//! Motion of a picture for a sync offset measurement (ADR-0126 decision 3; spec
//! `docs/specs/gpu-compositor.md`, "Motion probe").
//!
//! [`MotionProbe::submit`] runs one compute pass on a new picture: the picture is
//! averaged over a [`MOTION_GRID_W`] x [`MOTION_GRID_H`] grid, compared with the grid of
//! the previous call, and the sum of absolute luma differences is copied, 4 bytes, into
//! one of [`MOTION_RING`] mappable staging slots that are mapped without waiting. Each
//! slot keeps the time it was submitted for. [`MotionProbe::take`] hands out completed
//! values in submission order. Nothing blocks: a full ring drops the picture (counted).
//!
//! It exists only while a measurement runs. Hot path while it does: one dispatch and one
//! 4-byte copy per new picture; RBS code does not allocate once built, wgpu allocates
//! internally while recording a command buffer and for the map callback (as for
//! [`crate::YuvDownload`]). Each value read back counts in
//! `GpuMetrics::frames_cpu_copied` (the bounded P4 exception of ADR-0126).

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use rbs_core::media::{FramePayload, VideoFrame};

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::TexturePool;

/// Grid columns.
pub const MOTION_GRID_W: u32 = 64;
/// Grid rows.
pub const MOTION_GRID_H: u32 = 36;
/// Staging slots in flight.
pub const MOTION_RING: usize = 4;

const CELLS: u64 = MOTION_GRID_W as u64 * MOTION_GRID_H as u64;
const FIXED: f32 = 65_536.0;

const IDLE: u8 = 0;
const MAPPING: u8 = 1;
const MAPPED: u8 = 2;
const FAILED: u8 = 3;

/// Counters of one probe.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MotionStats {
    /// Pictures dispatched.
    pub submitted: u64,
    /// Values handed out by `take` (the first picture gives none: nothing to compare).
    pub delivered: u64,
    /// Pictures dropped because every slot was in flight.
    pub busy: u64,
    /// Readbacks that failed to map.
    pub failed: u64,
}

struct Slot {
    buffer: wgpu::Buffer,
    state: Arc<AtomicU8>,
    time_ns: u64,
    first: bool,
}

/// One picture's motion at a time, read back without blocking.
pub struct MotionProbe {
    pipeline: wgpu::ComputePipeline,
    state_group: wgpu::BindGroup,
    sum: wgpu::Buffer,
    slots: Vec<Slot>,
    /// The slot the next `submit` uses.
    next: usize,
    /// The oldest slot `take` has not handed out.
    oldest: usize,
    in_flight: usize,
    seen_first: bool,
    stats: MotionStats,
}

impl MotionProbe {
    /// Builds the pass, the grid of the previous picture and the staging slots. Not
    /// real time.
    pub fn new(ctx: &GpuContext) -> Result<Self, GpuError> {
        let device = ctx.device();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rbs-gpu motion"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/motion.wgsl").into()),
        });
        let storage = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: false },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let state_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rbs-gpu motion state"),
            entries: &[storage(0), storage(1)],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-gpu motion"),
            bind_group_layouts: &[Some(ctx.sampled_layout()), Some(&state_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("rbs-gpu motion"),
            layout: Some(&layout),
            module: &module,
            entry_point: Some("cs_motion"),
            compilation_options: Default::default(),
            cache: None,
        });
        let previous = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rbs-gpu motion grid"),
            size: CELLS * 4,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let sum = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rbs-gpu motion sum"),
            size: 4,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let state_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rbs-gpu motion state"),
            layout: &state_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: previous.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: sum.as_entire_binding(),
                },
            ],
        });
        let slots = (0..MOTION_RING)
            .map(|_| Slot {
                buffer: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("rbs-gpu motion staging"),
                    size: 4,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
                state: Arc::new(AtomicU8::new(IDLE)),
                time_ns: 0,
                first: false,
            })
            .collect();
        Ok(Self {
            pipeline,
            state_group,
            sum,
            slots,
            next: 0,
            oldest: 0,
            in_flight: 0,
            seen_first: false,
            stats: MotionStats::default(),
        })
    }

    /// Counters.
    pub fn stats(&self) -> MotionStats {
        self.stats
    }

    /// Slots in flight or waiting to be taken.
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Dispatches the motion of `picture` (a pool texture frame, sampleable) against
    /// the previous picture, stamped `time_ns`. `Ok(false)` when every slot is in
    /// flight: the picture is dropped (counted in `busy`) and the next picture is
    /// compared with the last one dispatched. Never waits.
    pub fn submit(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        picture: &VideoFrame,
        time_ns: u64,
    ) -> Result<bool, GpuError> {
        let FramePayload::Texture(id) = *picture.payload() else {
            return Err(GpuError::Unsupported(
                "motion probe needs a pool texture frame",
            ));
        };
        if self.in_flight == self.slots.len() {
            self.stats.busy += 1;
            return Ok(false);
        }
        let tex = pool.get(id).ok_or(GpuError::StaleTexture(id))?;
        let picture_group = tex
            .sampled_bind_group()
            .ok_or(GpuError::TargetMismatch(id))?;
        let mut encoder = ctx
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rbs-gpu motion"),
            });
        encoder.clear_buffer(&self.sum, 0, None);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("rbs-gpu motion"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, picture_group, &[]);
            pass.set_bind_group(1, &self.state_group, &[]);
            pass.dispatch_workgroups(MOTION_GRID_W.div_ceil(8), MOTION_GRID_H.div_ceil(4), 1);
        }
        let slot = &mut self.slots[self.next];
        encoder.copy_buffer_to_buffer(&self.sum, 0, &slot.buffer, 0, 4);
        ctx.queue().submit(Some(encoder.finish()));

        slot.time_ns = time_ns;
        slot.first = !self.seen_first;
        self.seen_first = true;
        slot.state.store(MAPPING, Ordering::Release);
        let state = Arc::clone(&slot.state);
        slot.buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let next = if result.is_ok() { MAPPED } else { FAILED };
                state.store(next, Ordering::Release);
            });
        self.next = (self.next + 1) % self.slots.len();
        self.in_flight += 1;
        self.stats.submitted += 1;
        Ok(true)
    }

    /// Hands completed values to `emit(time_ns, motion)` in submission order, `motion`
    /// being the mean absolute luma difference per cell in `0..=1`. Stops at the first
    /// slot not completed yet. Call after `GpuContext::poll`. Never waits; a slot that
    /// failed to map is counted and skipped.
    pub fn take(&mut self, ctx: &GpuContext, mut emit: impl FnMut(u64, f32)) {
        while self.in_flight > 0 {
            let slot = &self.slots[self.oldest];
            match slot.state.load(Ordering::Acquire) {
                MAPPED => {
                    let raw = match slot.buffer.slice(..).get_mapped_range() {
                        Ok(mapped) => {
                            let mut b = [0u8; 4];
                            b.copy_from_slice(&mapped[..4]);
                            Some(u32::from_le_bytes(b))
                        }
                        Err(_) => None,
                    };
                    slot.buffer.unmap();
                    match raw {
                        Some(raw) if !slot.first => {
                            #[allow(clippy::cast_precision_loss)] // a sum of 2304 cells
                            let motion = raw as f32 / FIXED / CELLS as f32;
                            ctx.metrics().count_cpu_copy();
                            self.stats.delivered += 1;
                            emit(slot.time_ns, motion);
                        }
                        Some(_) => {}
                        None => self.stats.failed += 1,
                    }
                }
                FAILED => self.stats.failed += 1,
                _ => return,
            }
            slot.state.store(IDLE, Ordering::Release);
            self.oldest = (self.oldest + 1) % self.slots.len();
            self.in_flight -= 1;
        }
    }
}
