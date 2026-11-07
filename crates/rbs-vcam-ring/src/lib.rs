//! The shared-memory frame ring between RBS (the writer) and a virtual camera consumer in
//! another process (the reader), ADR-0066 decision 2; contract in
//! `docs/specs/virtual-camera.md`, "Shared frame ring".
//!
//! A header and [`SLOTS`] slots are laid over one mapped byte region. Every field two
//! processes share is an atomic; each slot has a state word that only changes by
//! compare-and-swap, so a reader never copies a slot while the writer writes it, and the
//! writer never waits:
//!
//! - The writer takes a free slot, else a slot whose reader stopped (its heartbeat is
//!   older than [`STALE_MS`]), else the oldest ready slot no one reads. With one honest
//!   reader one of these always exists; otherwise the frame is dropped
//!   ([`WriteOutcome::Busy`]).
//! - The reader takes the newest ready slot, copies it out, and frees it. A copy whose
//!   slot was taken back meanwhile is discarded ([`ReadGuard::finish`] returns `false`).
//! - The reader never trusts the header: it keeps the [`Layout`] it computed from the
//!   format it announced and only uses the header to check that the writer agrees.
//!
//! The crate reads no clock: every call that needs the time takes `now_ms`, milliseconds
//! of a monotonic clock both processes share (`GetTickCount64` on Windows). It depends on
//! `std` only, so the consumer (a DLL loaded into a system service) pulls in nothing else.
//!
//! This is a marked low-level module (CLAUDE.md rule 7): every `unsafe` block states its
//! invariant. Memory shared with another process is outside Rust's memory model; the
//! protocol keeps honest parties from touching a slot at the same time, and pixel bytes
//! are only ever copied with raw pointer copies, never borrowed as references, so a
//! dishonest peer can at worst produce wrong pixels, which the state check discards.

use std::fmt;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// `RBSVCAM1`, little endian.
pub const MAGIC: u64 = u64::from_le_bytes(*b"RBSVCAM1");
/// Layout version. A peer with another version sees "no frames".
pub const VERSION: u32 = 1;
/// Number of frame slots.
pub const SLOTS: usize = 3;
/// A heartbeat older than this (milliseconds) means the other side stopped.
pub const STALE_MS: u64 = 1000;
/// Pixel format code of NV12 in the header.
pub const FORMAT_NV12: u32 = 1;
/// Largest width or height a layout accepts.
pub const MAX_DIMENSION: u32 = 16384;

const HEADER_LEN: usize = 128;
const SLOT_HEADER_LEN: usize = 64;
const ALIGN: usize = 64;

const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 8;
const OFF_GENERATION: usize = 12;
const OFF_WIDTH: usize = 16;
const OFF_HEIGHT: usize = 20;
const OFF_FORMAT: usize = 24;
const OFF_STRIDE: usize = 28;
const OFF_SLOT_COUNT: usize = 32;
const OFF_WRITER_HB: usize = 40;
const OFF_READER_HB: usize = 48;

const SLOT_OFF_STATE: usize = 0;
const SLOT_OFF_TIMESTAMP: usize = 8;

const TAG_FREE: u64 = 0;
const TAG_WRITING: u64 = 1;
const TAG_READY: u64 = 2;
const TAG_READING: u64 = 3;
const TAG_MASK: u64 = 0b11;

fn word(seq: u64, tag: u64) -> u64 {
    (seq << 2) | tag
}

fn tag(word: u64) -> u64 {
    word & TAG_MASK
}

fn seq(word: u64) -> u64 {
    word >> 2
}

/// Errors of the ring.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RingError {
    /// A layout with a zero, odd or too large size, or a stride below the width.
    InvalidLayout,
    /// The region is smaller than the layout needs, or not 8-byte aligned.
    BadRegion,
    /// The header has no magic yet (the creator has not initialised it).
    NotInitialized,
    /// The header carries another layout version.
    VersionMismatch {
        /// The version found.
        found: u32,
    },
    /// The header describes another format than the one expected.
    FormatMismatch,
    /// A frame whose length is not the layout's frame length.
    InvalidFrame,
}

impl fmt::Display for RingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLayout => f.write_str("invalid ring layout"),
            Self::BadRegion => f.write_str("region too small or misaligned for the layout"),
            Self::NotInitialized => f.write_str("ring not initialised"),
            Self::VersionMismatch { found } => {
                write!(f, "ring layout version {found}, expected {VERSION}")
            }
            Self::FormatMismatch => f.write_str("ring format differs from the expected one"),
            Self::InvalidFrame => f.write_str("frame length differs from the layout"),
        }
    }
}

impl std::error::Error for RingError {}

/// The size of everything in the ring, computed from the announced format alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    width: u32,
    height: u32,
    stride: u32,
    frame_len: usize,
    slot_stride: usize,
}

impl Layout {
    /// An NV12 layout: a luma plane of `stride * height` bytes followed by an
    /// interleaved chroma plane of `stride * height / 2` bytes. Width and height must be
    /// even, non-zero and at most [`MAX_DIMENSION`]; `stride` at least `width`.
    pub fn nv12(width: u32, height: u32, stride: u32) -> Result<Self, RingError> {
        let ok = width > 0
            && height > 0
            && width.is_multiple_of(2)
            && height.is_multiple_of(2)
            && width <= MAX_DIMENSION
            && height <= MAX_DIMENSION
            && stride >= width
            && stride <= MAX_DIMENSION * 2;
        if !ok {
            return Err(RingError::InvalidLayout);
        }
        // At most 32768 * 16384 * 3 / 2 bytes: fits usize on every supported target.
        let frame_len = (stride as usize) * (height as usize) * 3 / 2;
        let slot_stride = SLOT_HEADER_LEN + frame_len.div_ceil(ALIGN) * ALIGN;
        Ok(Self {
            width,
            height,
            stride,
            frame_len,
            slot_stride,
        })
    }

    /// Frame width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Frame height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Bytes per row of both planes.
    pub fn stride(&self) -> u32 {
        self.stride
    }

    /// Bytes of one frame.
    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    /// Bytes the whole ring needs.
    pub fn total_len(&self) -> usize {
        HEADER_LEN + SLOTS * self.slot_stride
    }

    fn slot_offset(&self, slot: usize) -> usize {
        HEADER_LEN + slot * self.slot_stride
    }
}

/// Keeps an owned backing buffer alive.
struct Owned(#[allow(dead_code)] Box<[AtomicU64]>);

/// A byte region the ring lives in: memory mapped from another process, or an owned
/// buffer for one process (tests, in-process consumers).
pub struct Region {
    ptr: NonNull<u8>,
    len: usize,
    _owner: Option<Arc<Owned>>,
}

// SAFETY: a `Region` is a pointer to memory that stays valid while any clone exists
// (`from_raw`'s contract, or the owned buffer the `Arc` keeps alive). All shared state is
// reached through atomics, and pixel bytes only through raw copies guarded by the slot
// protocol, so it may move to and be used from other threads.
unsafe impl Send for Region {}
// SAFETY: as for `Send`; `&Region` only gives out atomics and raw pointers.
unsafe impl Sync for Region {}

impl Clone for Region {
    fn clone(&self) -> Self {
        Self {
            ptr: self.ptr,
            len: self.len,
            _owner: self._owner.clone(),
        }
    }
}

impl fmt::Debug for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Region").field("len", &self.len).finish()
    }
}

impl Region {
    /// A zeroed buffer of at least `len` bytes owned by the region and its clones.
    pub fn owned(len: usize) -> Self {
        let words: Box<[AtomicU64]> = (0..len.div_ceil(8)).map(|_| AtomicU64::new(0)).collect();
        let len = words.len() * 8;
        let ptr = NonNull::from(&*words).cast::<u8>();
        Self {
            ptr,
            len,
            _owner: Some(Arc::new(Owned(words))),
        }
    }

    /// Wraps memory the caller mapped.
    ///
    /// # Safety
    ///
    /// `ptr` must point to `len` bytes that stay readable and writable, at the same
    /// address, for as long as this region or any clone of it exists, and that are only
    /// accessed by others through this ring's protocol (atomics for shared fields).
    pub unsafe fn from_raw(ptr: NonNull<u8>, len: usize) -> Result<Self, RingError> {
        if !(ptr.as_ptr() as usize).is_multiple_of(8) {
            return Err(RingError::BadRegion);
        }
        Ok(Self {
            ptr,
            len,
            _owner: None,
        })
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True for an empty region.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn fits(&self, layout: &Layout) -> bool {
        self.len >= layout.total_len()
    }

    fn u64_at(&self, offset: usize) -> &AtomicU64 {
        debug_assert!(offset.is_multiple_of(8) && offset + 8 <= self.len);
        // SAFETY: every caller passes an offset inside the region that `fits` checked for
        // the layout (header fields or slot words of a slot below `SLOTS`), 8-aligned
        // because the region is 8-aligned and all offsets are multiples of 8. The memory
        // is only ever accessed atomically at these offsets.
        unsafe { &*self.ptr.as_ptr().add(offset).cast::<AtomicU64>() }
    }

    fn u32_at(&self, offset: usize) -> &AtomicU32 {
        debug_assert!(offset.is_multiple_of(4) && offset + 4 <= self.len);
        // SAFETY: as in `u64_at`, with 4-byte header fields at 4-aligned offsets inside
        // the header, which `fits` covers.
        unsafe { &*self.ptr.as_ptr().add(offset).cast::<AtomicU32>() }
    }

    fn pixels_ptr(&self, layout: &Layout, slot: usize) -> *mut u8 {
        debug_assert!(slot < SLOTS);
        // SAFETY: `slot < SLOTS` and the region fits `layout`, so the slot's pixel bytes
        // `[offset + SLOT_HEADER_LEN, offset + SLOT_HEADER_LEN + frame_len)` lie inside it.
        unsafe {
            self.ptr
                .as_ptr()
                .add(layout.slot_offset(slot) + SLOT_HEADER_LEN)
        }
    }
}

fn state<'a>(region: &'a Region, layout: &Layout, slot: usize) -> &'a AtomicU64 {
    region.u64_at(layout.slot_offset(slot) + SLOT_OFF_STATE)
}

fn timestamp<'a>(region: &'a Region, layout: &Layout, slot: usize) -> &'a AtomicU64 {
    region.u64_at(layout.slot_offset(slot) + SLOT_OFF_TIMESTAMP)
}

/// True when the header describes `layout` with this crate's version.
fn header_matches(region: &Region, layout: &Layout) -> Result<(), RingError> {
    if region.u64_at(OFF_MAGIC).load(Ordering::Acquire) != MAGIC {
        return Err(RingError::NotInitialized);
    }
    let version = region.u32_at(OFF_VERSION).load(Ordering::Relaxed);
    if version != VERSION {
        return Err(RingError::VersionMismatch { found: version });
    }
    let same = region.u32_at(OFF_WIDTH).load(Ordering::Relaxed) == layout.width
        && region.u32_at(OFF_HEIGHT).load(Ordering::Relaxed) == layout.height
        && region.u32_at(OFF_STRIDE).load(Ordering::Relaxed) == layout.stride
        && region.u32_at(OFF_FORMAT).load(Ordering::Relaxed) == FORMAT_NV12
        && region.u32_at(OFF_SLOT_COUNT).load(Ordering::Relaxed) == SLOTS as u32;
    if same {
        Ok(())
    } else {
        Err(RingError::FormatMismatch)
    }
}

/// Writes a fresh header and frees every slot. Run by the creator of the region (the
/// consumer) each time it creates or resets the ring, with a generation different from
/// the last one, so a writer still attached starts over ([`WriteOutcome::Reset`]).
pub fn initialize(region: &Region, layout: &Layout, generation: u32) -> Result<(), RingError> {
    if !region.fits(layout) {
        return Err(RingError::BadRegion);
    }
    region.u64_at(OFF_MAGIC).store(0, Ordering::Release);
    region.u32_at(OFF_VERSION).store(VERSION, Ordering::Relaxed);
    region
        .u32_at(OFF_GENERATION)
        .store(generation, Ordering::Relaxed);
    region
        .u32_at(OFF_WIDTH)
        .store(layout.width, Ordering::Relaxed);
    region
        .u32_at(OFF_HEIGHT)
        .store(layout.height, Ordering::Relaxed);
    region
        .u32_at(OFF_FORMAT)
        .store(FORMAT_NV12, Ordering::Relaxed);
    region
        .u32_at(OFF_STRIDE)
        .store(layout.stride, Ordering::Relaxed);
    region
        .u32_at(OFF_SLOT_COUNT)
        .store(SLOTS as u32, Ordering::Relaxed);
    region.u64_at(OFF_WRITER_HB).store(0, Ordering::Relaxed);
    region.u64_at(OFF_READER_HB).store(0, Ordering::Relaxed);
    for slot in 0..SLOTS {
        state(region, layout, slot).store(word(0, TAG_FREE), Ordering::Relaxed);
        timestamp(region, layout, slot).store(0, Ordering::Relaxed);
    }
    region.u64_at(OFF_MAGIC).store(MAGIC, Ordering::Release);
    Ok(())
}

/// What [`Writer::write`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The frame is published with this sequence number.
    Written {
        /// Sequence number of the frame.
        seq: u64,
    },
    /// No slot could be taken; the frame was dropped.
    Busy,
    /// The creator reset the ring (new generation, or no header): attach again.
    Reset,
}

/// The writing side (RBS). Never waits.
#[derive(Debug)]
pub struct Writer {
    region: Region,
    layout: Layout,
    generation: u32,
    next_seq: u64,
}

impl Writer {
    /// Attaches to an initialised ring that must describe `expected`.
    pub fn attach(region: Region, expected: &Layout) -> Result<Self, RingError> {
        if !region.fits(expected) {
            return Err(RingError::BadRegion);
        }
        header_matches(&region, expected)?;
        let generation = region.u32_at(OFF_GENERATION).load(Ordering::Acquire);
        // There is one writer: a slot left `Writing` belongs to a writer that stopped
        // mid-frame (RBS crashed); free it.
        for s in 0..SLOTS {
            let st = state(&region, expected, s);
            let w = st.load(Ordering::Acquire);
            if tag(w) == TAG_WRITING {
                let _ = st.compare_exchange(
                    w,
                    word(seq(w), TAG_FREE),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
            }
        }
        let newest = (0..SLOTS)
            .map(|s| seq(state(&region, expected, s).load(Ordering::Acquire)))
            .max()
            .unwrap_or(0);
        Ok(Self {
            region,
            layout: *expected,
            generation,
            next_seq: newest.saturating_add(1).max(1),
        })
    }

    /// The layout written.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Stores the writer heartbeat. [`Writer::write`] does it too; call this at least
    /// every 250 ms while sending without frames.
    pub fn heartbeat(&self, now_ms: u64) {
        self.region
            .u64_at(OFF_WRITER_HB)
            .store(now_ms, Ordering::Release);
    }

    fn reset_seen(&self) -> bool {
        self.region.u64_at(OFF_MAGIC).load(Ordering::Acquire) != MAGIC
            || self.region.u32_at(OFF_GENERATION).load(Ordering::Acquire) != self.generation
    }

    fn take_slot(&self, now_ms: u64, seq_new: u64) -> Option<usize> {
        let writing = word(seq_new, TAG_WRITING);
        let words: [u64; SLOTS] =
            std::array::from_fn(|s| state(&self.region, &self.layout, s).load(Ordering::Acquire));
        // 1. A free slot.
        for (s, &w) in words.iter().enumerate() {
            if tag(w) == TAG_FREE && self.cas(s, w, writing) {
                return Some(s);
            }
        }
        // 2. A slot held by a reader that stopped.
        let reader_hb = self.region.u64_at(OFF_READER_HB).load(Ordering::Acquire);
        if now_ms.saturating_sub(reader_hb) > STALE_MS {
            for (s, &w) in words.iter().enumerate() {
                if tag(w) == TAG_READING && self.cas(s, w, writing) {
                    return Some(s);
                }
            }
        }
        // 3. The oldest ready slot.
        let mut ready: [(u64, usize); SLOTS] = [(u64::MAX, usize::MAX); SLOTS];
        for (s, &w) in words.iter().enumerate() {
            if tag(w) == TAG_READY {
                ready[s] = (seq(w), s);
            }
        }
        ready.sort_unstable();
        ready
            .iter()
            .filter(|(_, s)| *s != usize::MAX)
            .map(|&(_, s)| s)
            .find(|&s| self.cas(s, words[s], writing))
    }

    fn cas(&self, slot: usize, current: u64, new: u64) -> bool {
        state(&self.region, &self.layout, slot)
            .compare_exchange(current, new, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// Publishes one frame of exactly [`Layout::frame_len`] bytes with its timestamp.
    /// Never waits.
    pub fn write(
        &mut self,
        now_ms: u64,
        timestamp_ns: i64,
        pixels: &[u8],
    ) -> Result<WriteOutcome, RingError> {
        if pixels.len() != self.layout.frame_len {
            return Err(RingError::InvalidFrame);
        }
        if self.reset_seen() {
            return Ok(WriteOutcome::Reset);
        }
        self.heartbeat(now_ms);
        let seq_new = self.next_seq;
        let Some(slot) = self.take_slot(now_ms, seq_new) else {
            return Ok(WriteOutcome::Busy);
        };
        let dst = self.region.pixels_ptr(&self.layout, slot);
        // SAFETY: `dst` points to `frame_len` bytes inside the region (`pixels_ptr`), and
        // `pixels` has exactly `frame_len` bytes in our own memory, so the ranges do not
        // overlap. The slot is ours (state `Writing`), so no honest reader copies it now.
        unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), dst, pixels.len()) };
        timestamp(&self.region, &self.layout, slot).store(timestamp_ns as u64, Ordering::Relaxed);
        state(&self.region, &self.layout, slot).store(word(seq_new, TAG_READY), Ordering::Release);
        self.next_seq += 1;
        Ok(WriteOutcome::Written { seq: seq_new })
    }
}

/// Why the reader has no frame to show; the consumer shows its still picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoFrames {
    /// The header does not describe the reader's layout, or has another version.
    Header(RingError),
    /// The writer's heartbeat is older than [`STALE_MS`].
    WriterStopped,
}

/// A frame taken by the reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    /// Sequence number.
    pub seq: u64,
    /// The writer's timestamp, nanoseconds.
    pub timestamp_ns: i64,
}

/// The reading side (the consumer). Uses only its own layout.
#[derive(Debug)]
pub struct Reader {
    region: Region,
    layout: Layout,
    generation: u32,
    last_seq: u64,
}

impl Reader {
    /// A reader of `layout` over `region`, which must be large enough for it.
    pub fn new(region: Region, layout: Layout) -> Result<Self, RingError> {
        if !region.fits(&layout) {
            return Err(RingError::BadRegion);
        }
        Ok(Self {
            region,
            layout,
            generation: 0,
            last_seq: 0,
        })
    }

    /// The layout read.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Stores the reader heartbeat. [`Reader::begin`] does it too.
    pub fn heartbeat(&self, now_ms: u64) {
        self.region
            .u64_at(OFF_READER_HB)
            .store(now_ms, Ordering::Release);
    }

    /// Checks the header and the writer heartbeat.
    pub fn status(&self, now_ms: u64) -> Result<(), NoFrames> {
        header_matches(&self.region, &self.layout).map_err(NoFrames::Header)?;
        let hb = self.region.u64_at(OFF_WRITER_HB).load(Ordering::Acquire);
        if now_ms.saturating_sub(hb) > STALE_MS {
            return Err(NoFrames::WriterStopped);
        }
        Ok(())
    }

    /// Takes the newest ready frame newer than the last one read, if any.
    pub fn begin(&mut self, now_ms: u64) -> Result<Option<ReadGuard<'_>>, NoFrames> {
        self.heartbeat(now_ms);
        self.status(now_ms)?;
        let generation = self.region.u32_at(OFF_GENERATION).load(Ordering::Acquire);
        if generation != self.generation {
            self.generation = generation;
            self.last_seq = 0;
        }
        let mut best: Option<(u64, usize, u64)> = None;
        for s in 0..SLOTS {
            let w = state(&self.region, &self.layout, s).load(Ordering::Acquire);
            if tag(w) == TAG_READY && seq(w) > self.last_seq && best.is_none_or(|b| seq(w) > b.0) {
                best = Some((seq(w), s, w));
            }
        }
        let Some((frame_seq, slot, w)) = best else {
            return Ok(None);
        };
        let reading = word(frame_seq, TAG_READING);
        if state(&self.region, &self.layout, slot)
            .compare_exchange(w, reading, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            // The writer took it back; the next call finds a newer one.
            return Ok(None);
        }
        let timestamp_ns =
            timestamp(&self.region, &self.layout, slot).load(Ordering::Relaxed) as i64;
        Ok(Some(ReadGuard {
            reader: self,
            slot,
            info: FrameInfo {
                seq: frame_seq,
                timestamp_ns,
            },
            done: false,
        }))
    }

    /// Copies the newest new frame into `out` (at least [`Layout::frame_len`] bytes).
    /// `Ok(None)`: no new frame, or the copy was discarded because its slot was taken
    /// back.
    pub fn read_newest(
        &mut self,
        now_ms: u64,
        out: &mut [u8],
    ) -> Result<Option<FrameInfo>, NoFrames> {
        let Some(mut guard) = self.begin(now_ms)? else {
            return Ok(None);
        };
        guard.copy_to(out);
        let info = guard.info;
        Ok(guard.finish().then_some(info))
    }
}

/// A slot the reader holds. Dropping it without [`ReadGuard::finish`] makes the frame
/// ready again.
#[derive(Debug)]
pub struct ReadGuard<'a> {
    reader: &'a mut Reader,
    slot: usize,
    info: FrameInfo,
    done: bool,
}

impl ReadGuard<'_> {
    /// The frame's sequence number and timestamp.
    pub fn info(&self) -> FrameInfo {
        self.info
    }

    /// Copies the frame into the start of `out`; copies nothing when `out` is shorter
    /// than [`Layout::frame_len`].
    pub fn copy_to(&mut self, out: &mut [u8]) {
        let len = self.reader.layout.frame_len;
        if out.len() < len {
            return;
        }
        let src = self
            .reader
            .region
            .pixels_ptr(&self.reader.layout, self.slot);
        // SAFETY: `src` points to `len` bytes inside the region (`pixels_ptr` with the
        // reader's own layout, which the region fits), `out` has at least `len` bytes of
        // our own memory, so the ranges do not overlap. The slot is in state `Reading`,
        // so an honest writer does not write it; a copy the writer overlapped after
        // taking the slot back is discarded by `finish`.
        unsafe { std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), len) };
    }

    /// Frees the slot. `false` when the writer took the slot back meanwhile: the copy
    /// may be torn and must be discarded.
    pub fn finish(mut self) -> bool {
        self.done = true;
        let reading = word(self.info.seq, TAG_READING);
        let ok = state(&self.reader.region, &self.reader.layout, self.slot)
            .compare_exchange(
                reading,
                word(self.info.seq, TAG_FREE),
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok();
        if ok {
            self.reader.last_seq = self.info.seq;
        }
        ok
    }
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = state(&self.reader.region, &self.reader.layout, self.slot).compare_exchange(
                word(self.info.seq, TAG_READING),
                word(self.info.seq, TAG_READY),
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
    }
}
