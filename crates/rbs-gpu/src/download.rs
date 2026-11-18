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

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use bytes::BytesMut;
use rbs_core::media::{
    Bytes, ColorSpace, FramePayload, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::TexturePool;
use crate::yuv::{ScaleKernel, YuvConverter, YuvFormat};

/// Staging buffers in flight between the GPU and the CPU. At 60 fps a frame normally
/// completes within one or two ticks; four leave room for a slow readback. Also the
/// entries (readbacks and repeats) one download keeps in flight.
pub const DOWNLOAD_RING: usize = 4;

/// Frame buffers handed to consumers and recycled. A consumer chain (render output slot,
/// the queue to the encoder, the encoder's input) holds about three at once.
pub const DOWNLOAD_BUFFERS: usize = 6;

/// Staging buffers and frame buffers a download adds to a shared readback it joins
/// ([`DownloadShare`]): its consumers mostly hold the same frames as the others', so
/// fewer than a readback of its own. Not real-time: allocated when it joins.
const JOIN_SLOTS: usize = 2;
const JOIN_BUFFERS: usize = DOWNLOAD_BUFFERS / 2;

/// Staging slot states. `IDLE`, `MAPPING`, `MAPPED` and `FAILED` are written by the
/// map callback too; `READ` only by the render thread.
const IDLE: u8 = 0;
const MAPPING: u8 = 1;
const MAPPED: u8 = 2;
const FAILED: u8 = 3;
/// Copied into a frame buffer (or lost while copying) and unmapped; kept until every
/// download that queued it has taken it.
const READ: u8 = 5;

/// Counters of one download.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DownloadStats {
    /// Frames queued for readback: converted by this download, or joined (`joined`).
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
    /// built. A repeat with no picture before it counts here too.
    pub failed: u64,
    /// Frames delivered by a repeat entry: the picture before it again, with a new
    /// timestamp, without GPU work or a copy (included in `delivered`).
    pub repeated: u64,
    /// Frames queued without a conversion of their own: another download of the same
    /// [`DownloadShare`] already converted that canvas picture, and both take the one
    /// readback and the one CPU copy (R-01; included in `converted`).
    pub joined: u64,
}

/// Why a read slot holds no frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lost {
    NoBuffer,
    Failed,
}

/// One staging buffer and what was read from it.
struct Slot {
    staging: wgpu::Buffer,
    state: Arc<AtomicU8>,
    /// The canvas picture converted into it: `(timestamp, sequence)` of the canvas.
    picture: (Timestamp, u64),
    /// Downloads whose entries still refer to it. Free again at 0.
    users: u32,
    /// After `READ`: the frame copied, shared with the users still to take it.
    frame: Option<VideoFrame>,
    lost: Option<Lost>,
}

/// What a download's shape is: equal shapes can share one readback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    format: YuvFormat,
    src: (u32, u32),
    size: (u32, u32),
    color: ColorSpace,
    open_shadows: bool,
}

/// The GPU side of a download: the conversion, the staging ring and the frame buffers.
/// Owned by one download, or shared by several of one [`DownloadShare`] in one render
/// partition (only the render thread locks it, so the lock is never contended).
struct Readback {
    shape: Shape,
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
    /// Where the search for a free slot starts: slots are used round the ring.
    head: usize,
    buffers: Vec<Bytes>,
}

/// One entry of a download, in the order it was queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Free,
    /// A readback in slot `n` of the readback.
    Readback(usize),
    /// The picture of the entry before it again (render on change, R-03).
    Repeat,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    kind: Kind,
    timestamp: Timestamp,
    sequence: u64,
}

const FREE: Entry = Entry {
    kind: Kind::Free,
    timestamp: Timestamp::ZERO,
    sequence: 0,
};

/// A readback of its own, or one shared through a [`DownloadShare`].
enum Store {
    Own(Box<Readback>),
    Shared(Arc<Mutex<Readback>>),
}

enum Guard<'a> {
    Own(&'a mut Readback),
    Shared(MutexGuard<'a, Readback>),
}

impl std::ops::Deref for Guard<'_> {
    type Target = Readback;
    fn deref(&self) -> &Readback {
        match self {
            Guard::Own(r) => r,
            Guard::Shared(r) => r,
        }
    }
}

impl std::ops::DerefMut for Guard<'_> {
    fn deref_mut(&mut self) -> &mut Readback {
        match self {
            Guard::Own(r) => r,
            Guard::Shared(r) => r,
        }
    }
}

impl Store {
    fn lock(&mut self) -> Result<Guard<'_>, GpuError> {
        Ok(match self {
            Store::Own(r) => Guard::Own(r),
            Store::Shared(r) => Guard::Shared(
                r.lock()
                    .map_err(|_| GpuError::Readback("shared download poisoned".into()))?,
            ),
        })
    }
}

/// Lets downloads of the same shape read one canvas picture back once (perf audit R-01,
/// ADR-0143 R4): the first download made with a share creates the readback, and each
/// later one of the same format, sizes, colour and grade joins it while any of them
/// lives. A download of another shape makes a readback of its own and becomes the one
/// later downloads join. Each download keeps its own entries, stamps, repeats and
/// counters; only the conversion, the readback and the CPU copy of a picture both
/// queued are done once.
#[derive(Default)]
pub struct DownloadShare {
    readback: Mutex<Weak<Mutex<Readback>>>,
}

impl std::fmt::Debug for DownloadShare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadShare").finish_non_exhaustive()
    }
}

impl DownloadShare {
    /// A share no download uses yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Downloads alive that use this share's readback now.
    pub fn users(&self) -> usize {
        self.readback.lock().map(|w| w.strong_count()).unwrap_or(0)
    }
}

impl Readback {
    #[allow(clippy::too_many_arguments)]
    fn new(ctx: &GpuContext, shape: Shape, buffers: usize) -> Result<Self, GpuError> {
        let Shape {
            format,
            src: (src_width, src_height),
            size: (width, height),
            color,
            open_shadows,
        } = shape;
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
        if usize::try_from(len).is_err() {
            return Err(GpuError::InvalidSize { width, height });
        }
        let mut readback = Self {
            shape,
            converter,
            format: VideoFormat::new(Size::new(width, height), format.pixel_format(), color),
            luma: luma_tex.create_view(&wgpu::TextureViewDescriptor::default()),
            luma_tex,
            chroma: chroma_tex.create_view(&wgpu::TextureViewDescriptor::default()),
            chroma_tex,
            stride,
            chroma_offset,
            len,
            slots: Vec::new(),
            head: 0,
            buffers: Vec::new(),
        };
        readback.grow(ctx, DOWNLOAD_RING, buffers);
        Ok(readback)
    }

    /// Adds `slots` staging buffers and `buffers` frame buffers. Not real-time.
    fn grow(&mut self, ctx: &GpuContext, slots: usize, buffers: usize) {
        for _ in 0..slots {
            self.slots.push(Slot {
                staging: ctx.device().create_buffer(&wgpu::BufferDescriptor {
                    label: Some("rbs-gpu download staging"),
                    size: self.len,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
                state: Arc::new(AtomicU8::new(IDLE)),
                picture: (Timestamp::ZERO, 0),
                users: 0,
                frame: None,
                lost: None,
            });
        }
        // `new` checked that the length fits in `usize`.
        let size = usize::try_from(self.len).unwrap_or(0);
        self.buffers
            .extend((0..buffers).map(|_| shared_buffer(size)));
    }

    fn state(&self, index: usize) -> u8 {
        self.slots[index].state.load(Ordering::Acquire)
    }

    /// A slot already holding `picture` (in flight or read, not lost), to join.
    fn holding(&self, picture: (Timestamp, u64)) -> Option<usize> {
        self.slots.iter().position(|s| {
            s.users > 0
                && s.picture == picture
                && s.lost.is_none()
                && s.state.load(Ordering::Acquire) != FAILED
        })
    }

    /// Converts `canvas` into a free staging buffer and starts mapping it; `None` when
    /// every staging buffer is in use. Hot path; never waits.
    fn convert(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        canvas: &VideoFrame,
        id: crate::TextureId,
    ) -> Result<Option<usize>, GpuError> {
        let n = self.slots.len();
        let Some(index) = (0..n)
            .map(|i| (self.head + i) % n)
            .find(|&i| self.slots[i].users == 0 && self.state(i) == IDLE)
        else {
            return Ok(None);
        };
        let source = self.converter.source(pool, id)?;
        let mut encoder = self.converter.record(ctx, source, &self.luma, &self.chroma);
        let slot = &self.slots[index];
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

        let slot = &mut self.slots[index];
        slot.picture = (canvas.timestamp(), canvas.sequence());
        slot.users = 1;
        slot.state.store(MAPPING, Ordering::Release);
        let state = Arc::clone(&slot.state);
        slot.staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let next = if result.is_ok() { MAPPED } else { FAILED };
                state.store(next, Ordering::Release);
            });
        self.head = (index + 1) % n;
        Ok(Some(index))
    }

    /// One user of slot `index` is done with it; the last one unmaps it and frees it.
    fn release(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        slot.users = slot.users.saturating_sub(1);
        if slot.users > 0 {
            return;
        }
        if slot.state.load(Ordering::Acquire) == MAPPED {
            slot.staging.unmap();
        }
        slot.frame = None;
        slot.lost = None;
        slot.state.store(IDLE, Ordering::Release);
    }

    /// The frame of completed slot `index` for one of its users: copied from the
    /// staging buffer by the first user that takes it (then unmapped), shared after.
    /// A loss is counted in `stats` for every user.
    fn frame(
        &mut self,
        ctx: &GpuContext,
        index: usize,
        stats: &mut DownloadStats,
    ) -> Option<VideoFrame> {
        if self.state(index) == MAPPED {
            let read = self.read(index);
            if read.is_ok() {
                ctx.metrics().count_cpu_copy();
            }
            let slot = &mut self.slots[index];
            slot.staging.unmap();
            slot.state.store(READ, Ordering::Release);
            match read {
                Ok(frame) => slot.frame = Some(frame),
                Err(lost) => slot.lost = Some(lost),
            }
        }
        let slot = &mut self.slots[index];
        if let Some(lost) = slot.lost {
            match lost {
                Lost::NoBuffer => stats.no_buffer += 1,
                Lost::Failed => stats.failed += 1,
            }
            return None;
        }
        // The last user takes the frame; earlier ones share it.
        if slot.users <= 1 {
            slot.frame.take()
        } else {
            slot.frame.as_ref().and_then(VideoFrame::share)
        }
    }

    /// Copies mapped slot `index` into a free frame buffer.
    fn read(&mut self, index: usize) -> Result<VideoFrame, Lost> {
        let free = self
            .buffers
            .iter()
            .position(Bytes::is_unique)
            .ok_or(Lost::NoBuffer)?;
        // Unique, so this keeps the allocation (no copy, no allocation).
        let mut buffer: BytesMut = match std::mem::take(&mut self.buffers[free]).try_into_mut() {
            Ok(b) => b,
            Err(shared) => {
                self.buffers[free] = shared;
                return Err(Lost::NoBuffer);
            }
        };
        let slot = &self.slots[index];
        match slot.staging.slice(..).get_mapped_range() {
            Ok(mapped) => buffer.copy_from_slice(&mapped),
            Err(_) => {
                // Put the (still unique) buffer back; the frame is lost.
                self.buffers[free] = buffer.freeze();
                return Err(Lost::Failed);
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
        // Cannot fail for the layout `new` computed; counted, never silent.
        let system = SystemFrame::new(self.format, bytes, &planes).map_err(|_| Lost::Failed)?;
        Ok(VideoFrame::system(slot.picture.0, system, None).with_sequence(slot.picture.1))
    }
}

/// Converts canvases to NV12 or P010 frames in system memory, without blocking.
pub struct YuvDownload {
    store: Store,
    format: VideoFormat,
    kernel: ScaleKernel,
    stride: u32,
    len: u64,
    /// This download's readbacks and repeats in the order queued, a ring of
    /// [`DOWNLOAD_RING`].
    entries: [Entry; DOWNLOAD_RING],
    /// Next entry to queue into.
    head: usize,
    /// Oldest entry that may be in flight.
    tail: usize,
    /// The frame delivered last, which a repeat entry delivers again. It shares the
    /// buffer the consumer already holds, so it pins no extra buffer until a newer
    /// frame replaces it.
    last: Option<VideoFrame>,
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
        Self::with_color(ctx, format, width, height, ColorSpace::BT709_LIMITED)
    }

    /// [`Self::new`] for `color`: [`ColorSpace::BT709_LIMITED`] or [`ColorSpace::SRGB_VIDEO`].
    pub fn with_color(
        ctx: &GpuContext,
        format: YuvFormat,
        width: u32,
        height: u32,
        color: ColorSpace,
    ) -> Result<Self, GpuError> {
        Self::scaled_color(
            ctx,
            format,
            width,
            height,
            width,
            height,
            DOWNLOAD_BUFFERS,
            color,
        )
    }

    /// [`Self::with_color`] with the open-shadows grade. Frames stay tagged with `color`.
    pub fn with_tone(
        ctx: &GpuContext,
        format: YuvFormat,
        width: u32,
        height: u32,
        color: ColorSpace,
        open_shadows: bool,
    ) -> Result<Self, GpuError> {
        Self::scaled_tone(
            ctx,
            format,
            width,
            height,
            width,
            height,
            DOWNLOAD_BUFFERS,
            color,
            open_shadows,
        )
    }

    /// A download of `src_width x src_height` canvases to `width x height` frames
    /// (fitted with the aspect ratio kept, [`YuvConverter::scaled`]) with `buffers` frame
    /// buffers (at least 2): enough for every frame the consumers may hold at once plus
    /// one, so recycling never allocates (ADR-0066 decision 3).
    pub fn scaled(
        ctx: &GpuContext,
        format: YuvFormat,
        src_width: u32,
        src_height: u32,
        width: u32,
        height: u32,
        buffers: usize,
    ) -> Result<Self, GpuError> {
        Self::scaled_color(
            ctx,
            format,
            src_width,
            src_height,
            width,
            height,
            buffers,
            ColorSpace::BT709_LIMITED,
        )
    }

    /// [`Self::scaled`] for `color`.
    #[allow(clippy::too_many_arguments)]
    pub fn scaled_color(
        ctx: &GpuContext,
        format: YuvFormat,
        src_width: u32,
        src_height: u32,
        width: u32,
        height: u32,
        buffers: usize,
        color: ColorSpace,
    ) -> Result<Self, GpuError> {
        Self::scaled_tone(
            ctx, format, src_width, src_height, width, height, buffers, color, false,
        )
    }

    /// [`Self::scaled_color`] with the open-shadows grade. The delivered frames stay
    /// tagged with `color`. The grade applies only to Rec. 709 limited range.
    #[allow(clippy::too_many_arguments)]
    pub fn scaled_tone(
        ctx: &GpuContext,
        format: YuvFormat,
        src_width: u32,
        src_height: u32,
        width: u32,
        height: u32,
        buffers: usize,
        color: ColorSpace,
        open_shadows: bool,
    ) -> Result<Self, GpuError> {
        let shape = Shape {
            format,
            src: (src_width, src_height),
            size: (width, height),
            color,
            open_shadows,
        };
        Self::build(ctx, shape, buffers, None)
    }

    /// [`Self::scaled_tone`] through `share`: joins the readback of a live download of
    /// the same shape made with `share` (adding [`DOWNLOAD_BUFFERS`]` / 2` frame buffers
    /// and two staging buffers to it), else creates one that later downloads join.
    #[allow(clippy::too_many_arguments)]
    pub fn scaled_tone_shared(
        ctx: &GpuContext,
        share: &DownloadShare,
        format: YuvFormat,
        src_width: u32,
        src_height: u32,
        width: u32,
        height: u32,
        buffers: usize,
        color: ColorSpace,
        open_shadows: bool,
    ) -> Result<Self, GpuError> {
        let shape = Shape {
            format,
            src: (src_width, src_height),
            size: (width, height),
            color,
            open_shadows,
        };
        Self::build(ctx, shape, buffers, Some(share))
    }

    fn build(
        ctx: &GpuContext,
        shape: Shape,
        buffers: usize,
        share: Option<&DownloadShare>,
    ) -> Result<Self, GpuError> {
        if buffers < 2 {
            return Err(GpuError::Unsupported(
                "a download needs at least 2 frame buffers",
            ));
        }
        fn poisoned<T>(_: T) -> GpuError {
            GpuError::Readback("download share poisoned".into())
        }
        let store = match share {
            None => Store::Own(Box::new(Readback::new(ctx, shape, buffers)?)),
            Some(share) => {
                let mut slot = share.readback.lock().map_err(poisoned)?;
                let joined = match slot.upgrade() {
                    Some(running) => {
                        let mut r = running.lock().map_err(poisoned)?;
                        if r.shape == shape {
                            r.grow(ctx, JOIN_SLOTS, JOIN_BUFFERS.min(buffers));
                            drop(r);
                            Some(running)
                        } else {
                            None
                        }
                    }
                    None => None,
                };
                let readback = match joined {
                    Some(r) => r,
                    None => {
                        let r = Arc::new(Mutex::new(Readback::new(ctx, shape, buffers)?));
                        *slot = Arc::downgrade(&r);
                        r
                    }
                };
                Store::Shared(readback)
            }
        };
        let mut download = Self {
            store,
            format: VideoFormat::new(
                Size::new(shape.size.0, shape.size.1),
                shape.format.pixel_format(),
                shape.color,
            ),
            kernel: ScaleKernel::Bilinear,
            stride: 0,
            len: 0,
            entries: [FREE; DOWNLOAD_RING],
            head: 0,
            tail: 0,
            last: None,
            stats: DownloadStats::default(),
        };
        let r = download.store.lock()?;
        let (kernel, stride, len, format) = (r.converter.kernel(), r.stride, r.len, r.format);
        drop(r);
        download.kernel = kernel;
        download.stride = stride;
        download.len = len;
        download.format = format;
        Ok(download)
    }

    /// The format of the frames this download delivers: the output size, NV12 or P010,
    /// and the colour the converter encoded (BT.709 limited range by default).
    pub fn format(&self) -> &VideoFormat {
        &self.format
    }

    /// The scale kernel this download's converter was built with (ADR-0102).
    pub fn kernel(&self) -> ScaleKernel {
        self.kernel
    }

    /// Bytes per row of both planes in delivered frames.
    pub fn stride(&self) -> usize {
        self.stride as usize
    }

    /// Counters.
    pub fn stats(&self) -> DownloadStats {
        self.stats
    }

    /// Readbacks and repeat entries in flight (not taken yet).
    pub fn in_flight(&self) -> usize {
        self.entries.iter().filter(|e| e.kind != Kind::Free).count()
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
        self.submit_at(ctx, pool, canvas, canvas.timestamp(), canvas.sequence())
    }

    /// [`Self::submit`], with the frame read back stamped `timestamp` and `sequence`
    /// instead of the canvas's: a picture that did not change is passed on again as a
    /// new frame (ADR-0086, a separate file at the canvas frame rate). With a
    /// [`DownloadShare`], a canvas picture another download of the share already
    /// queued is joined instead of converted again (`joined`). Hot path.
    pub fn submit_at(
        &mut self,
        ctx: &GpuContext,
        pool: &TexturePool,
        canvas: &VideoFrame,
        timestamp: Timestamp,
        sequence: u64,
    ) -> Result<bool, GpuError> {
        let FramePayload::Texture(id) = *canvas.payload() else {
            return Err(GpuError::Unsupported("download needs a pool texture frame"));
        };
        if self.entries[self.head].kind != Kind::Free {
            self.stats.busy += 1;
            return Ok(false);
        }
        let mut r = self.store.lock()?;
        let picture = (canvas.timestamp(), canvas.sequence());
        let index = match r.holding(picture) {
            Some(index) => {
                r.slots[index].users += 1;
                self.stats.joined += 1;
                index
            }
            None => match r.convert(ctx, pool, canvas, id)? {
                Some(index) => index,
                None => {
                    drop(r);
                    self.stats.busy += 1;
                    return Ok(false);
                }
            },
        };
        drop(r);
        self.entries[self.head] = Entry {
            kind: Kind::Readback(index),
            timestamp,
            sequence,
        };
        self.head = (self.head + 1) % DOWNLOAD_RING;
        self.stats.converted += 1;
        Ok(true)
    }

    /// Queues the picture of the entry before this one again, stamped `timestamp` and
    /// `sequence`, for a canvas that did not change (render on change, R-03; ADR-0143
    /// R5). No GPU work, no copy: once taken it is the previous frame's buffer again.
    /// Its place in the ring keeps the output in order behind readbacks in flight.
    /// Returns `false` when every slot is in flight (counted in `busy`).
    pub fn submit_repeat(&mut self, timestamp: Timestamp, sequence: u64) -> bool {
        let entry = &mut self.entries[self.head];
        if entry.kind != Kind::Free {
            self.stats.busy += 1;
            return false;
        }
        *entry = Entry {
            kind: Kind::Repeat,
            timestamp,
            sequence,
        };
        self.head = (self.head + 1) % DOWNLOAD_RING;
        true
    }

    /// The newest frame whose readback has completed, if any. Older completed frames are
    /// dropped (`stale`). Call after `GpuContext::poll` (the render runtime polls at the
    /// end of each tick). Hot path; never waits.
    pub fn take_ready(&mut self, ctx: &GpuContext) -> Result<Option<VideoFrame>, GpuError> {
        self.take::<true>(ctx)
    }

    /// The oldest frame whose readback has completed, if any; newer completed frames
    /// stay for later calls. For the end of a stream, where every frame is passed on in
    /// order and none is dropped as stale (ADR-0045). Hot path; never waits.
    pub fn take_oldest(&mut self, ctx: &GpuContext) -> Result<Option<VideoFrame>, GpuError> {
        self.take::<false>(ctx)
    }

    /// `take_ready` (`NEWEST`) or `take_oldest`.
    fn take<const NEWEST: bool>(
        &mut self,
        ctx: &GpuContext,
    ) -> Result<Option<VideoFrame>, GpuError> {
        if self.entries[self.tail].kind == Kind::Free {
            // Nothing in flight: no lock taken.
            return Ok(None);
        }
        let mut r = self.store.lock()?;
        // The newest completed entry's stamp, and the newest readback at or before it,
        // whose picture that entry shows (a repeat shows the picture before it). A repeat
        // is never merged into another entry: each is one frame of a constant frame rate
        // output, so a take returns at most one repeat, and a repeat behind a readback
        // waits for the next take.
        let mut stamp = None;
        let mut picture = None;
        let mut failed = false;
        // Readbacks complete in submission order; walk from the oldest while they are done.
        loop {
            let entry = self.entries[self.tail];
            match entry.kind {
                Kind::Free => break,
                Kind::Readback(index) => match r.state(index) {
                    MAPPED | READ => {
                        if stamp.replace((entry.timestamp, entry.sequence)).is_some() {
                            self.stats.stale += 1;
                        }
                        if let Some(older) = picture.replace(index) {
                            r.release(older);
                        }
                        if !NEWEST {
                            self.entries[self.tail] = FREE;
                            self.tail = (self.tail + 1) % DOWNLOAD_RING;
                            break;
                        }
                    }
                    FAILED => {
                        r.release(index);
                        self.stats.failed += 1;
                        failed = true;
                        // A repeat behind it must not show the picture before the lost one.
                        self.last = None;
                    }
                    _ => break,
                },
                Kind::Repeat => {
                    if stamp.is_some() {
                        // Left for the next take.
                        break;
                    }
                    stamp = Some((entry.timestamp, entry.sequence));
                    self.entries[self.tail] = FREE;
                    self.tail = (self.tail + 1) % DOWNLOAD_RING;
                    break;
                }
            }
            self.entries[self.tail] = FREE;
            self.tail = (self.tail + 1) % DOWNLOAD_RING;
        }
        let Some((timestamp, sequence)) = stamp else {
            drop(r);
            return if failed {
                Err(GpuError::Readback("staging buffer map failed".into()))
            } else {
                Ok(None)
            };
        };
        let frame = match picture {
            Some(index) => {
                let frame = r.frame(ctx, index, &mut self.stats);
                r.release(index);
                // The frame to repeat, sharing the consumer's buffer.
                self.last = frame.as_ref().and_then(VideoFrame::share);
                frame
            }
            None => {
                let again = self.last.as_ref().and_then(VideoFrame::share);
                if again.is_some() {
                    self.stats.repeated += 1;
                } else {
                    // No picture before this repeat (the readback before it failed).
                    self.stats.failed += 1;
                }
                again
            }
        };
        drop(r);
        let frame = frame.map(|f| f.with_timestamp(timestamp).with_sequence(sequence));
        if frame.is_some() {
            self.stats.delivered += 1;
        }
        Ok(frame)
    }

    /// Bytes of one delivered frame.
    pub fn frame_len(&self) -> u64 {
        self.len
    }
}

impl Drop for YuvDownload {
    /// Hands back the slots this download's entries still hold, so the downloads that
    /// share its readback can use them again.
    fn drop(&mut self) {
        let Store::Shared(shared) = &self.store else {
            return;
        };
        let Ok(mut r) = shared.lock() else {
            return;
        };
        for entry in &self.entries {
            if let Kind::Readback(index) = entry.kind {
                r.release(index);
            }
        }
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
