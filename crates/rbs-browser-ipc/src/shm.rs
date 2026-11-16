//! Studio-allocated shared memory for browser frames (ADR-0109 decisions 3 and 8).
//!
//! Low-level module: the only `unsafe` in this crate. It maps one named shared memory
//! section that the studio creates and the helper opens. The section holds a header
//! and three frame slots that form a triple buffer: the writer (helper) and the reader
//! (studio) each own one slot at all times, and they exchange the third through one
//! atomic word, so neither side ever waits and a slot is never written while it is read.
//!
//! ```text
//! offset  size  field
//! 0       4     magic "RBSB" (0x4253_4252, little-endian u32)
//! 4       4     layout version (1)
//! 8       4     width
//! 12      4     height
//! 16      4     pixel layout: 1 = Rgba8 straight alpha, 2 = Bgra8 premultiplied alpha
//! 20      4     slot count (3)
//! 24      4     exchange word: bits 0..2 = middle slot, bit 2 = a new frame is in it
//! 28      4     reserved (0)
//! 32      8     frames written by the helper
//! 40      8     heartbeat: the helper adds one at least every 500 ms
//! 48      80    reserved (0)
//! 128     ...   three slots of width * height * 4 bytes, sRGB
//! ```
//!
//! The helper is untrusted: the reader copies bytes out with a raw copy and never
//! forms a reference to memory the helper may write. A corrupt exchange word gives a
//! slot index that is clamped, never an out-of-range read.

use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Header size in bytes; slots start here.
pub const HEADER: usize = 128;
/// Number of frame slots.
pub const SLOTS: u32 = 3;
/// `RBSB`.
pub const MAGIC: u32 = 0x4253_4252;
/// Layout version.
pub const VERSION: u32 = 1;
const DIRTY: u32 = 0b100;
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
        self.dword(32).load(Ordering::Relaxed)
    }

    /// The helper's heartbeat counter.
    pub fn heartbeat(&self) -> u64 {
        self.dword(40).load(Ordering::Acquire)
    }

    /// Helper side: one heartbeat.
    pub fn beat(&self) {
        self.dword(40).fetch_add(1, Ordering::Release);
    }

    /// Helper side: fills the writer's slot with `fill` and publishes it. Never waits.
    pub fn publish_with(&self, writer: &mut RingWriter, fill: impl FnOnce(&mut [u8])) {
        let index = (writer.back.min(SLOTS - 1)) as usize;
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
        self.dword(32).fetch_add(1, Ordering::Relaxed);
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
                    0o600 as libc::mode_t,
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
    use windows::core::HSTRING;

    static NEXT: AtomicU32 = AtomicU32::new(0);

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

#[cfg(test)]
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
