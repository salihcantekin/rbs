//! Studio-allocated shared memory for browser frames (ADR-0109 decisions 3 and 8).
//!
//! Low-level crate: OS shared memory, and the only `unsafe` of the browser IPC. It
//! maps one named shared memory section that the studio creates and the helper
//! opens. It sits beside `rbs-browser-ipc`, which stays free of OS bindings. The
//! section holds a header
//! and three frame slots that form a triple buffer: the writer (helper) and the reader
//! (studio) each own one slot at all times, and they exchange the third through one
//! atomic word, so neither side ever waits and a slot is never written while it is read.
//!
//! ```text
//! offset  size  field
//! 0       4     magic "RBSB" (0x4253_4252, little-endian u32)
//! 4       4     layout version (2)
//! 8       4     width
//! 12      4     height
//! 16      4     pixel layout: 1 = Rgba8 straight alpha, 2 = Bgra8 premultiplied alpha
//! 20      4     slot count (3)
//! 24      4     exchange word: bits 0..2 = middle slot, bit 2 = a new frame is in it
//! 28      4     reserved (0)
//! 32      8     frames written by the helper
//! 40      8     heartbeat: the helper adds one at least every 500 ms
//! 48      24    paint time of slots 0, 1 and 2: OS monotonic nanoseconds
//!               ([`os_monotonic_nanos`]) the helper took when it started to fill the
//!               slot; 0 = unknown (ADR-0109 decision 8)
//! 72      56    reserved (0)
//! 128     256   GPU block (version 2, ADR-0157), below
//! 384     ...   three slots of width * height * 4 bytes, sRGB
//! ```
//!
//! The GPU block, at offset 128, describes the studio's shared textures when the
//! helper copies CEF's accelerated paint into them (Windows, ADR-0157):
//!
//! ```text
//! offset  size  field
//! 128     4     GPU slot count (0 = no GPU path, at most 8)
//! 132     4     slot of the latest GPU publish
//! 136     8     GPU publish sequence (0 until the first)
//! 144     32    state word of GPU slots 0..8: bit 31 = the helper writes it,
//!               bits 0..30 = studio leases
//! 176     64    fence value of GPU slots 0..8
//! 240     64    paint time of GPU slots 0..8 (OS monotonic nanoseconds)
//! 304     80    reserved (0)
//! ```
//!
//! The GPU protocol (ADR-0157 decision 4): the helper claims a slot only with a
//! compare-and-swap from 0 to `WRITING`, never the slot published last, and clears its
//! bit with `fetch_and`; the studio leases with `fetch_add`, backs off when it saw
//! `WRITING`, and checks the publish sequence again after its lease is held.
//!
//! The helper is untrusted: the reader copies bytes out with a raw copy and never
//! forms a reference to memory the helper may write. A corrupt exchange word gives a
//! slot index that is clamped, never an out-of-range read.

use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

mod gpu_slots;
pub mod sound;

use gpu_slots::{FRAMES, GPU_COUNT};

pub use sound::{MAX_SOUND_CHANNELS, OwnedSoundPacket, SoundPacket, SoundRing};

/// The OS monotonic clock in nanoseconds (`CLOCK_MONOTONIC` on Unix,
/// `QueryPerformanceCounter` on Windows). Every process on the machine reads the same
/// clock, so a helper stamps page sound with it and the studio maps it to master time
/// at the edge (ADR-0004, ADR-0109 decision 6). Not related to `Instant`'s origin.
pub fn os_monotonic_nanos() -> u64 {
    os::monotonic_nanos()
}

/// Header size in bytes (layout version 2); slots start here.
pub const HEADER: usize = 384;
/// Number of frame slots.
pub const SLOTS: u32 = 3;
/// `RBSB`.
pub const MAGIC: u32 = 0x4253_4252;
/// Layout version.
pub const VERSION: u32 = 2;
/// Most GPU slots a section describes.
pub const MAX_GPU_SLOTS: u32 = 8;
/// GPU slots a page gets by default (ADR-0157 decision 5).
pub const DEFAULT_GPU_SLOTS: u32 = 4;
/// State-word bit: the helper is writing this GPU slot.
pub const WRITING: u32 = 1 << 31;
const DIRTY: u32 = 0b100;
/// Header offset of slot 0's paint time; slots 1 and 2 follow.
const PAINTED: usize = 48;
const INDEX: u32 = 0b011;

/// Pixel layout of the slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotLayout {
    /// Straight-alpha `Rgba8` (the test double).
    Rgba8Straight,
    /// Premultiplied `Bgra8` (what CEF paints).
    Bgra8Premultiplied,
}

impl SlotLayout {
    fn code(self) -> u32 {
        match self {
            SlotLayout::Rgba8Straight => 1,
            SlotLayout::Bgra8Premultiplied => 2,
        }
    }

    fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(SlotLayout::Rgba8Straight),
            2 => Some(SlotLayout::Bgra8Premultiplied),
            _ => None,
        }
    }
}

/// Bytes of one slot, or `None` when the size overflows.
pub fn slot_bytes(width: u32, height: u32) -> Option<usize> {
    (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)
}

/// Bytes of the whole section, or `None` when the size overflows.
pub fn section_bytes(width: u32, height: u32) -> Option<usize> {
    slot_bytes(width, height)?
        .checked_mul(SLOTS as usize)?
        .checked_add(HEADER)
}

/// A GPU picture the studio holds a lease on ([`FrameRing::take_gpu`]). The caller
/// hands the lease to a frame or gives it back with [`FrameRing::lease_sub`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuPicture {
    /// GPU slot.
    pub slot: u32,
    /// Fence value the helper signals when its copy into the slot is done.
    pub fence: u64,
    /// Paint time (OS monotonic nanoseconds; 0 when unknown).
    pub painted: u64,
    /// Publish sequence it was taken at.
    pub seq: u64,
}

/// One mapped frame section. The studio [`FrameRing::create`]s it; the helper
/// [`FrameRing::open`]s it by name.
pub struct FrameRing {
    map: os::Mapping,
    width: u32,
    height: u32,
    layout: SlotLayout,
    slot: usize,
}

impl std::fmt::Debug for FrameRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameRing")
            .field("name", &self.map.name())
            .field("width", &self.width)
            .field("height", &self.height)
            .field("layout", &self.layout)
            .finish()
    }
}

impl FrameRing {
    /// Creates a new section with a unique name (studio side). Allocated before the
    /// helper is spawned; the studio owns it (ADR-0109 decision 8).
    pub fn create(width: u32, height: u32, layout: SlotLayout) -> io::Result<Self> {
        Self::create_with_gpu(width, height, layout, 0)
    }

    /// As [`Self::create`], with `gpu_slots` GPU slots described (at most
    /// [`MAX_GPU_SLOTS`]; 0 = no GPU path).
    pub fn create_with_gpu(
        width: u32,
        height: u32,
        layout: SlotLayout,
        gpu_slots: u32,
    ) -> io::Result<Self> {
        let total = section_bytes(width, height)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
        let slot = slot_bytes(width, height).unwrap_or(0);
        let map = os::Mapping::create(total)?;
        let ring = Self {
            map,
            width,
            height,
            layout,
            slot,
        };
        ring.word(GPU_COUNT)
            .store(gpu_slots.min(MAX_GPU_SLOTS), Ordering::Relaxed);
        ring.word(0).store(MAGIC, Ordering::Relaxed);
        ring.word(4).store(VERSION, Ordering::Relaxed);
        ring.word(8).store(width, Ordering::Relaxed);
        ring.word(12).store(height, Ordering::Relaxed);
        ring.word(16).store(layout.code(), Ordering::Relaxed);
        ring.word(20).store(SLOTS, Ordering::Relaxed);
        // Writer starts on slot 0, the middle is slot 1, the reader holds slot 2.
        ring.word(24).store(1, Ordering::Release);
        Ok(ring)
    }

    /// Opens a section the studio created (helper side) and checks its header against
    /// the size the helper was told.
    pub fn open(name: &str, width: u32, height: u32) -> io::Result<Self> {
        let total = section_bytes(width, height)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
        let map = os::Mapping::open(name, total)?;
        let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
        let probe = Self {
            map,
            width,
            height,
            layout: SlotLayout::Rgba8Straight,
            slot: slot_bytes(width, height).unwrap_or(0),
        };
        if probe.word(0).load(Ordering::Acquire) != MAGIC {
            return Err(bad("frame section magic"));
        }
        // The studio and the runner ship together: another layout is refused, and the
        // helper exits before it paints (ADR-0157 decision 10).
        if probe.word(4).load(Ordering::Relaxed) != VERSION {
            return Err(bad("frame section version"));
        }
        if probe.word(8).load(Ordering::Relaxed) != width
            || probe.word(12).load(Ordering::Relaxed) != height
            || probe.word(20).load(Ordering::Relaxed) != SLOTS
        {
            return Err(bad("frame section size"));
        }
        let layout = SlotLayout::from_code(probe.word(16).load(Ordering::Relaxed))
            .ok_or_else(|| bad("frame section layout"))?;
        Ok(Self { layout, ..probe })
    }

    /// The name the helper opens.
    pub fn name(&self) -> &str {
        self.map.name()
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Pixel layout of the slots.
    pub fn layout(&self) -> SlotLayout {
        self.layout
    }

    /// Writer side: a fresh writer that owns slot 0.
    pub fn writer(&self) -> RingWriter {
        RingWriter { back: 0 }
    }

    /// Reader side: a fresh reader that owns slot 2.
    pub fn reader(&self) -> RingReader {
        RingReader { front: 2 }
    }

    /// Frames the helper has published.
    pub fn frames_written(&self) -> u64 {
        self.dword(FRAMES).load(Ordering::Relaxed)
    }

    /// The helper's heartbeat counter.
    pub fn heartbeat(&self) -> u64 {
        self.dword(40).load(Ordering::Acquire)
    }

    /// Helper side: one heartbeat.
    pub fn beat(&self) {
        self.dword(40).fetch_add(1, Ordering::Release);
    }

    /// Helper side: fills the writer's slot with `fill` and publishes it, stamped with
    /// the OS monotonic time at the call (the paint time). Never waits.
    pub fn publish_with(&self, writer: &mut RingWriter, fill: impl FnOnce(&mut [u8])) {
        let index = (writer.back.min(SLOTS - 1)) as usize;
        // The writer owns this slot's stamp as it owns the slot; the exchange below
        // publishes both.
        self.dword(PAINTED + 8 * index)
            .store(os_monotonic_nanos(), Ordering::Relaxed);
        let start = HEADER + index * self.slot;
        // SAFETY: `start + slot` is inside the mapping (`section_bytes` sized it and
        // `index < SLOTS`). The triple buffer gives the writer exclusive ownership of
        // its back slot: the reader only touches the slot it swapped out, and a well-
        // behaved peer never writes there. The studio is the peer here and follows
        // the protocol, so no other reference to these bytes exists while `fill` runs.
        let slot =
            unsafe { std::slice::from_raw_parts_mut(self.map.as_ptr().add(start), self.slot) };
        fill(slot);
        let old = self.word(24).swap(writer.back | DIRTY, Ordering::AcqRel);
        writer.back = old & INDEX;
        self.dword(FRAMES).fetch_add(1, Ordering::Relaxed);
    }

    /// Studio side: when a new frame was published since the last call, copies it into
    /// `dst` (resized to one slot) and returns true. Never waits, never allocates once
    /// `dst` has the slot's length.
    pub fn take_latest(&self, reader: &mut RingReader, dst: &mut Vec<u8>) -> bool {
        let word = self.word(24);
        if word.load(Ordering::Acquire) & DIRTY == 0 {
            return false;
        }
        let old = word.swap(reader.front, Ordering::AcqRel);
        reader.front = (old & INDEX).min(SLOTS - 1);
        if dst.len() != self.slot {
            dst.resize(self.slot, 0);
        }
        let start = HEADER + reader.front as usize * self.slot;
        // SAFETY: the source range is inside the mapping (see `publish_with`); `dst`
        // has exactly `slot` bytes and is ours. A raw copy reads the bytes without
        // forming a reference to memory an untrusted helper could write at the same
        // time; at worst a misbehaving helper gives us a torn picture, never UB in
        // our own address space beyond those bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.map.as_ptr().add(start),
                dst.as_mut_ptr(),
                self.slot,
            );
        }
        true
    }
}

/// The GPU slot protocol on the mapped header (`gpu_slots`, ADR-0157 decision 4).
#[cfg(not(all(loom, test)))]
impl gpu_slots::Words for FrameRing {
    fn word(&self, offset: usize) -> &AtomicU32 {
        FrameRing::word(self, offset)
    }

    fn dword(&self, offset: usize) -> &AtomicU64 {
        FrameRing::dword(self, offset)
    }
}

#[cfg(not(all(loom, test)))]
impl FrameRing {
    /// Studio side, before the helper opens the section: describe `count` GPU slots
    /// (at most [`MAX_GPU_SLOTS`]).
    pub fn set_gpu_slots(&self, count: u32) {
        gpu_slots::set_slots(self, count);
    }

    /// GPU slots this section describes; 0 without a GPU path.
    pub fn gpu_slots(&self) -> u32 {
        gpu_slots::slots(self)
    }

    #[cfg(test)]
    fn state(&self, slot: u32) -> &AtomicU32 {
        gpu_slots::state(self, slot)
    }

    /// Helper side: claims a free GPU slot (`compare_exchange(0, WRITING)`), never
    /// the slot published last. `None` when none is free. Never waits.
    pub fn claim_gpu(&self) -> Option<u32> {
        gpu_slots::claim(self)
    }

    /// Helper side: gives a claimed slot up without publishing it.
    pub fn abandon_gpu(&self, slot: u32) {
        gpu_slots::abandon(self, slot);
    }

    /// Helper side: the copy into the claimed `slot` signals `fence` when done.
    /// Publishes the slot and clears its `WRITING` bit. Never waits.
    pub fn publish_gpu(&self, slot: u32, fence: u64) {
        gpu_slots::publish(self, slot, fence, os_monotonic_nanos());
    }

    /// Studio side: leases the GPU picture published since `seen` and returns it,
    /// with `seen` moved to its sequence. `None` when nothing new is there, or when
    /// the slot was being written or a newer picture came while it leased (then
    /// `seen` stays, and the next call tries again). Never waits.
    pub fn take_gpu(&self, seen: &mut u64) -> Option<GpuPicture> {
        let picture = gpu_slots::lease_latest(self, Some(*seen))?;
        *seen = picture.seq;
        Some(picture)
    }

    /// Studio side: leases the latest published GPU picture even when it was taken
    /// before (a page shown again). `None` before the first publish or on a race.
    pub fn lease_latest_gpu(&self) -> Option<GpuPicture> {
        gpu_slots::lease_latest(self, None)
    }

    /// Studio leases on a GPU slot.
    pub fn lease(&self, slot: u32) -> u32 {
        gpu_slots::lease(self, slot)
    }

    /// One more lease on a slot the studio already leases (a clone of a frame).
    pub fn lease_add(&self, slot: u32) {
        gpu_slots::lease_add(self, slot);
    }

    /// Gives one lease back. The helper may claim the slot once none is left.
    pub fn lease_sub(&self, slot: u32) {
        gpu_slots::lease_sub(self, slot);
    }
}

impl FrameRing {
    /// Studio side: the paint time (OS monotonic nanoseconds) of the frame the last
    /// [`Self::take_latest`] copied, or 0 when the helper did not stamp it. Untrusted:
    /// the caller maps it through its edge clock, which never places it after now.
    pub fn painted(&self, reader: &RingReader) -> u64 {
        let index = reader.front.min(SLOTS - 1) as usize;
        self.dword(PAINTED + 8 * index).load(Ordering::Relaxed)
    }

    fn word(&self, offset: usize) -> &AtomicU32 {
        debug_assert!(offset + 4 <= HEADER && offset.is_multiple_of(4));
        // SAFETY: the mapping is page-aligned and at least `HEADER` bytes, `offset`
        // is a multiple of 4 inside the header, and both processes only touch header
        // words through atomics, so an `AtomicU32` view is valid for the mapping's life
        // (tied to `&self`).
        unsafe { &*self.map.as_ptr().add(offset).cast::<AtomicU32>() }
    }

    fn dword(&self, offset: usize) -> &AtomicU64 {
        debug_assert!(offset + 8 <= HEADER && offset.is_multiple_of(8));
        // SAFETY: as `word`, with 8-byte alignment of `offset`.
        unsafe { &*self.map.as_ptr().add(offset).cast::<AtomicU64>() }
    }
}

/// The writer's own slot. Kept by the helper.
#[derive(Debug)]
pub struct RingWriter {
    back: u32,
}

/// The reader's own slot. Kept by the studio.
#[derive(Debug)]
pub struct RingReader {
    front: u32,
}

#[cfg(unix)]
mod os {
    use std::ffi::CString;
    use std::io;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);

    pub fn monotonic_nanos() -> u64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec; `CLOCK_MONOTONIC` exists on
        // every supported Unix.
        let ok = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } == 0;
        if !ok {
            return 0;
        }
        let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
        let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
        secs.saturating_mul(1_000_000_000).saturating_add(nanos)
    }

    pub struct Mapping {
        ptr: *mut u8,
        len: usize,
        name: String,
        owner: bool,
    }

    // SAFETY: the mapping is plain shared memory with no thread affinity; access to
    // its bytes goes through atomics or the triple-buffer protocol above.
    unsafe impl Send for Mapping {}
    // SAFETY: as above; `&Mapping` only exposes the base pointer and the name.
    unsafe impl Sync for Mapping {}

    impl Mapping {
        pub fn create(len: usize) -> io::Result<Self> {
            let name = format!(
                "/rbsb.{}.{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let c = CString::new(name.clone()).map_err(io::Error::other)?;
            // SAFETY: `c` is a valid NUL-terminated name; flags create a new object
            // readable and writable by this user only.
            let fd = unsafe {
                libc::shm_open(
                    c.as_ptr(),
                    libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                    // `shm_open` is variadic on macOS, where `mode_t` is u16: pass a full
                    // `c_uint` (equal to `mode_t` on Linux).
                    0o600 as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let sized = libc::off_t::try_from(len).map_err(io::Error::other);
            // SAFETY: `fd` is the descriptor just opened.
            let truncated = sized.map(|len| unsafe { libc::ftruncate(fd, len) });
            let mapped = match truncated {
                Ok(0) => map(fd, len),
                Ok(_) => Err(io::Error::last_os_error()),
                Err(error) => Err(error),
            };
            // SAFETY: `fd` is ours; the mapping (if any) stays valid after close.
            unsafe { libc::close(fd) };
            match mapped {
                Ok(ptr) => Ok(Self {
                    ptr,
                    len,
                    name,
                    owner: true,
                }),
                Err(error) => {
                    // SAFETY: `c` names the object created above.
                    unsafe { libc::shm_unlink(c.as_ptr()) };
                    Err(error)
                }
            }
        }

        pub fn open(name: &str, len: usize) -> io::Result<Self> {
            let c = CString::new(name).map_err(io::Error::other)?;
            // SAFETY: valid NUL-terminated name; open an existing object read-write.
            let fd = unsafe { libc::shm_open(c.as_ptr(), libc::O_RDWR, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `stat` is plain data filled by `fstat` on our descriptor.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: `fd` is open and `stat` is a valid out pointer.
            let ok = unsafe { libc::fstat(fd, &mut stat) } == 0;
            let big_enough = ok && usize::try_from(stat.st_size).is_ok_and(|size| size >= len);
            let mapped = if big_enough {
                map(fd, len)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame section is smaller than its size",
                ))
            };
            // SAFETY: `fd` is ours.
            unsafe { libc::close(fd) };
            Ok(Self {
                ptr: mapped?,
                len,
                name: name.to_owned(),
                owner: false,
            })
        }

        pub fn as_ptr(&self) -> *mut u8 {
            self.ptr
        }

        pub fn name(&self) -> &str {
            &self.name
        }
    }

    fn map(fd: libc::c_int, len: usize) -> io::Result<*mut u8> {
        // SAFETY: `fd` refers to a shared memory object of at least `len` bytes; a
        // shared read-write mapping of it has no other preconditions.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            Err(io::Error::last_os_error())
        } else {
            Ok(ptr.cast())
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: `ptr`/`len` are the mapping made in `map`; no references into it
            // outlive `self` (they borrow `FrameRing`).
            unsafe { libc::munmap(self.ptr.cast(), self.len) };
            if self.owner {
                if let Ok(c) = CString::new(self.name.clone()) {
                    // SAFETY: valid name of the object this process created.
                    unsafe { libc::shm_unlink(c.as_ptr()) };
                }
            }
        }
    }
}

#[cfg(windows)]
mod os {
    use std::io;
    use std::sync::atomic::{AtomicU32, Ordering};

    use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
    };
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    use windows::core::HSTRING;

    static NEXT: AtomicU32 = AtomicU32::new(0);

    pub fn monotonic_nanos() -> u64 {
        let (mut count, mut freq) = (0i64, 0i64);
        // SAFETY: both pointers are valid, writable `i64`s; neither call fails on
        // Windows XP or later.
        let ok = unsafe {
            QueryPerformanceCounter(&mut count).is_ok()
                && QueryPerformanceFrequency(&mut freq).is_ok()
        };
        if !ok || freq <= 0 || count < 0 {
            return 0;
        }
        let (count, freq) = (count as u128, freq as u128);
        u64::try_from(count * 1_000_000_000 / freq).unwrap_or(u64::MAX)
    }

    pub struct Mapping {
        handle: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
        name: String,
    }

    // SAFETY: a section handle and its view have no thread affinity.
    unsafe impl Send for Mapping {}
    // SAFETY: `&Mapping` only exposes the view's base pointer and the name.
    unsafe impl Sync for Mapping {}

    impl Mapping {
        pub fn create(len: usize) -> io::Result<Self> {
            let name = format!(
                "Local\\rbsb.{}.{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let size = len as u64;
            // SAFETY: a pagefile-backed section (`INVALID_HANDLE_VALUE`) with a fresh
            // name; default security limits it to this user's session.
            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    None,
                    PAGE_READWRITE,
                    (size >> 32) as u32,
                    size as u32,
                    &HSTRING::from(name.as_str()),
                )
            }
            .map_err(io::Error::other)?;
            Self::view(handle, len, name)
        }

        pub fn open(name: &str, len: usize) -> io::Result<Self> {
            // SAFETY: opens an existing named section for read-write.
            let handle =
                unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, &HSTRING::from(name)) }
                    .map_err(io::Error::other)?;
            Self::view(handle, len, name.to_owned())
        }

        fn view(handle: HANDLE, len: usize, name: String) -> io::Result<Self> {
            // SAFETY: `handle` is a section of at least `len` bytes (created with that
            // size, or checked against the header by the caller).
            let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, len) };
            if view.Value.is_null() {
                let error = io::Error::last_os_error();
                // SAFETY: `handle` is ours.
                let _ = unsafe { CloseHandle(handle) };
                return Err(error);
            }
            Ok(Self { handle, view, name })
        }

        pub fn as_ptr(&self) -> *mut u8 {
            self.view.Value.cast()
        }

        pub fn name(&self) -> &str {
            &self.name
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: the view and handle were made in `view`; nothing borrows them
            // past `self`.
            unsafe {
                let _ = UnmapViewOfFile(self.view);
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
    use super::*;

    #[test]
    fn a_published_frame_reaches_the_reader_once() {
        let studio = FrameRing::create(2, 2, SlotLayout::Rgba8Straight).unwrap();
        let helper = FrameRing::open(studio.name(), 2, 2).unwrap();
        let mut writer = helper.writer();
        let mut reader = studio.reader();
        let mut buf = Vec::new();
        assert!(!studio.take_latest(&mut reader, &mut buf), "nothing yet");
        helper.publish_with(&mut writer, |px| px.fill(7));
        assert!(studio.take_latest(&mut reader, &mut buf));
        assert_eq!(buf, vec![7; 16]);
        assert!(!studio.take_latest(&mut reader, &mut buf), "taken once");
        assert_eq!(studio.frames_written(), 1);
    }

    #[test]
    fn the_reader_gets_the_newest_of_several_frames() {
        let studio = FrameRing::create(2, 2, SlotLayout::Rgba8Straight).unwrap();
        let helper = FrameRing::open(studio.name(), 2, 2).unwrap();
        let mut writer = helper.writer();
        let mut reader = studio.reader();
        for value in 1..=5u8 {
            helper.publish_with(&mut writer, |px| px.fill(value));
        }
        let mut buf = Vec::new();
        assert!(studio.take_latest(&mut reader, &mut buf));
        assert_eq!(buf[0], 5);
    }

    #[test]
    fn each_frame_carries_its_own_paint_time() {
        let studio = FrameRing::create(2, 2, SlotLayout::Rgba8Straight).unwrap();
        let helper = FrameRing::open(studio.name(), 2, 2).unwrap();
        let mut writer = helper.writer();
        let mut reader = studio.reader();
        let mut buf = Vec::new();
        assert_eq!(studio.painted(&reader), 0, "unknown before a frame");
        let before = os_monotonic_nanos();
        helper.publish_with(&mut writer, |px| px.fill(1));
        let after = os_monotonic_nanos();
        assert!(studio.take_latest(&mut reader, &mut buf));
        let first = studio.painted(&reader);
        assert!(
            (before..=after).contains(&first),
            "{before} {first} {after}"
        );
        helper.publish_with(&mut writer, |px| px.fill(2));
        helper.publish_with(&mut writer, |px| px.fill(3));
        assert!(studio.take_latest(&mut reader, &mut buf));
        assert_eq!(buf[0], 3);
        assert!(studio.painted(&reader) >= first);
        // The slot the reader holds keeps its stamp while the writer paints others.
        let held = studio.painted(&reader);
        helper.publish_with(&mut writer, |px| px.fill(4));
        assert_eq!(studio.painted(&reader), held);
    }

    #[test]
    fn writer_and_reader_never_share_a_slot() {
        let studio = FrameRing::create(2, 2, SlotLayout::Rgba8Straight).unwrap();
        let mut writer = studio.writer();
        let mut reader = studio.reader();
        let mut buf = Vec::new();
        for round in 0..50u32 {
            studio.publish_with(&mut writer, |_| {});
            if round % 3 == 0 {
                studio.take_latest(&mut reader, &mut buf);
            }
            let middle = studio.word(24).load(Ordering::Acquire) & INDEX;
            let mut owned = [writer.back, reader.front, middle];
            owned.sort_unstable();
            assert_eq!(owned, [0, 1, 2], "round {round}");
        }
    }

    #[test]
    fn a_wrong_size_or_name_is_refused() {
        let studio = FrameRing::create(4, 2, SlotLayout::Bgra8Premultiplied).unwrap();
        assert!(FrameRing::open(studio.name(), 2, 2).is_err());
        assert!(FrameRing::open("/rbsb.missing.0", 2, 2).is_err());
        let helper = FrameRing::open(studio.name(), 4, 2).unwrap();
        assert_eq!(helper.layout(), SlotLayout::Bgra8Premultiplied);
    }

    fn gpu_ring(slots: u32) -> FrameRing {
        FrameRing::create_with_gpu(2, 2, SlotLayout::Bgra8Premultiplied, slots).unwrap()
    }

    #[test]
    fn a_gpu_publish_is_leased_once_and_names_its_slot_and_fence() {
        let studio = gpu_ring(4);
        let helper = FrameRing::open(studio.name(), 2, 2).unwrap();
        assert_eq!(helper.gpu_slots(), 4);
        let mut seen = 0;
        assert!(studio.take_gpu(&mut seen).is_none());
        let slot = helper.claim_gpu().unwrap();
        helper.publish_gpu(slot, 9);
        let picture = studio.take_gpu(&mut seen).unwrap();
        assert_eq!((picture.slot, picture.fence), (slot, 9));
        assert_eq!(studio.lease(slot), 1);
        assert!(studio.take_gpu(&mut seen).is_none(), "taken once");
        studio.lease_sub(slot);
        assert_eq!(studio.lease(slot), 0);
    }

    #[test]
    fn the_helper_never_claims_a_leased_writing_or_latest_slot() {
        let ring = gpu_ring(4);
        let a = ring.claim_gpu().unwrap();
        ring.publish_gpu(a, 1);
        // `a` is the latest publish: never claimed, even with no lease.
        let mut claimed = Vec::new();
        while let Some(slot) = ring.claim_gpu() {
            claimed.push(slot);
        }
        assert_eq!(claimed.len(), 3);
        assert!(!claimed.contains(&a));
        // Each claimed slot is WRITING: a lease on it backs off.
        ring.abandon_gpu(claimed[0]);
        let mut seen = 0;
        let leased = ring.take_gpu(&mut seen).unwrap();
        assert_eq!(leased.slot, a);
        // A leased slot is not claimed after it stops being the latest.
        ring.publish_gpu(claimed[1], 2);
        assert_ne!(ring.claim_gpu(), Some(a));
    }

    #[test]
    fn a_lease_on_a_slot_being_written_backs_off_and_keeps_the_publish_unseen() {
        let ring = gpu_ring(4);
        let slot = ring.claim_gpu().unwrap();
        ring.publish_gpu(slot, 1);
        // A helper that dies between storing the index and clearing its bit leaves
        // WRITING set on the latest slot.
        ring.state(slot).fetch_or(WRITING, Ordering::Relaxed);
        let mut seen = 0;
        assert!(ring.take_gpu(&mut seen).is_none());
        assert_eq!(seen, 0, "not seen: tried again next pass");
        assert_eq!(ring.lease(slot), 0, "the tentative lease was given back");
        ring.state(slot).fetch_and(!WRITING, Ordering::Relaxed);
        assert_eq!(ring.take_gpu(&mut seen).map(|p| p.slot), Some(slot));
        assert_eq!(seen, 1);
    }

    #[test]
    fn a_shown_again_page_leases_its_latest_picture_once_more() {
        let ring = gpu_ring(4);
        let slot = ring.claim_gpu().unwrap();
        ring.publish_gpu(slot, 5);
        let mut seen = 0;
        let first = ring.take_gpu(&mut seen).unwrap();
        let again = ring.lease_latest_gpu().unwrap();
        assert_eq!((again.slot, again.fence), (first.slot, 5));
        assert_eq!(ring.lease(slot), 2);
    }

    #[test]
    fn a_section_without_gpu_slots_has_no_gpu_path() {
        let ring = FrameRing::create(2, 2, SlotLayout::Rgba8Straight).unwrap();
        assert_eq!(ring.gpu_slots(), 0);
        assert_eq!(ring.claim_gpu(), None);
        ring.publish_gpu(0, 1);
        assert!(ring.take_gpu(&mut 0).is_none());
        assert_eq!(gpu_ring(99).gpu_slots(), MAX_GPU_SLOTS);
    }

    #[test]
    fn another_layout_version_is_refused() {
        let studio = gpu_ring(4);
        studio.word(4).store(1, Ordering::Relaxed);
        assert!(FrameRing::open(studio.name(), 2, 2).is_err());
    }

    /// The helper claims, "writes" (fills the slot's marker with its fence value)
    /// and publishes on one thread while the studio leases, checks the marker and
    /// releases on another: a leased slot never changes under its lease, and the
    /// studio sees the newest publishes (ADR-0157 decisions 4 and 11).
    #[test]
    fn a_leased_slot_is_never_written_under_racing_claims_and_leases() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        // The studio side decides when the run ends (after it took enough pictures),
        // so the result does not depend on how the two threads are scheduled.
        const TAKE: u32 = 2_000;
        let ring = Arc::new(gpu_ring(4));
        let markers: Arc<Vec<AtomicU64>> = Arc::new((0..4).map(|_| AtomicU64::new(0)).collect());
        let stop = Arc::new(AtomicBool::new(false));
        let helper = {
            let (ring, markers, stop) =
                (Arc::clone(&ring), Arc::clone(&markers), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut fence = 0u64;
                while !stop.load(Ordering::Acquire) {
                    let Some(slot) = ring.claim_gpu() else {
                        std::hint::spin_loop();
                        continue;
                    };
                    fence += 1;
                    markers[slot as usize].store(fence, Ordering::Relaxed);
                    ring.publish_gpu(slot, fence);
                }
                fence
            })
        };
        // Hang guard only: the loop ends on the count, not on time.
        let guard = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut seen = 0;
        let mut held: Vec<GpuPicture> = Vec::new();
        let mut last = 0;
        let mut taken = 0u32;
        while taken < TAKE {
            assert!(
                std::time::Instant::now() < guard,
                "hung after {taken} pictures"
            );
            if let Some(p) = ring.take_gpu(&mut seen) {
                assert_eq!(markers[p.slot as usize].load(Ordering::Relaxed), p.fence);
                assert!(p.fence > last, "{} after {last}", p.fence);
                last = p.fence;
                taken += 1;
                held.push(p);
            }
            // Hold up to two pictures, like the importer's on-air and hand-back.
            if held.len() > 2 {
                let p = held.remove(0);
                assert_eq!(markers[p.slot as usize].load(Ordering::Relaxed), p.fence);
                ring.lease_sub(p.slot);
            }
        }
        stop.store(true, Ordering::Release);
        let final_fence = helper.join().unwrap();
        for p in held {
            assert_eq!(markers[p.slot as usize].load(Ordering::Relaxed), p.fence);
            ring.lease_sub(p.slot);
        }
        // The newest publish is still there to take.
        if last < final_fence {
            let p = ring.take_gpu(&mut seen).unwrap();
            assert_eq!(p.fence, final_fence);
        }
    }

    #[test]
    fn the_heartbeat_counts_beats() {
        let studio = FrameRing::create(2, 2, SlotLayout::Rgba8Straight).unwrap();
        let helper = FrameRing::open(studio.name(), 2, 2).unwrap();
        assert_eq!(studio.heartbeat(), 0);
        helper.beat();
        helper.beat();
        assert_eq!(studio.heartbeat(), 2);
    }

    #[test]
    fn oversized_frames_are_refused_before_mapping() {
        assert!(section_bytes(u32::MAX, u32::MAX).is_none() || cfg!(target_pointer_width = "64"));
        assert!(FrameRing::create(u32::MAX, u32::MAX, SlotLayout::Rgba8Straight).is_err());
    }
}
