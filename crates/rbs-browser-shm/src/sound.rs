//! Studio-allocated shared memory for page sound (ADR-0109 decision 6).
//!
//! The studio creates the section before it spawns the helper and names it in
//! `configure`. The helper (the engine's audio thread) appends packets of interleaved
//! `f32` samples, each with the OS monotonic time of its first frame
//! ([`crate::os_monotonic_nanos`]), its rate and its channel count. The studio's
//! supervisor thread reads them in order and stamps them with master time at that
//! edge. Neither side ever waits: a packet that does not fit is dropped and counted
//! by the helper.
//!
//! ```text
//! offset  size  field
//! 0       4     magic "RBSA" (0x4153_4252, little-endian u32)
//! 4       4     layout version (1)
//! 8       4     sample capacity (SAMPLES)
//! 12      4     packet capacity (PACKETS)
//! 16      8     samples written by the helper (release)
//! 24      8     samples read by the studio (release)
//! 32      8     packets written by the helper (release, after the entry and samples)
//! 40      8     packets read by the studio (release)
//! 48      8     packets the helper dropped (the ring was full)
//! 56      72    reserved (0)
//! 128     ...   PACKETS entries of 32 bytes:
//!               u64 first sample | u64 OS time ns | u32 samples | u32 rate
//!               | u32 channels | u32 reserved
//! ...     ...   SAMPLES f32 samples, a ring indexed by sample count
//! ```
//!
//! The helper is untrusted: every entry is checked against the reader's own position
//! and the ring's size before a byte is copied, and samples are copied out with a raw
//! copy, never through a reference into memory the helper may write.

use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::os;

/// `RBSA`.
pub const SOUND_MAGIC: u32 = 0x4153_4252;
/// Layout version.
pub const SOUND_VERSION: u32 = 1;
/// Samples the ring holds: about 680 ms of 48 kHz stereo.
pub const SAMPLES: usize = 1 << 16;
/// Packets the ring holds.
pub const PACKETS: usize = 128;
/// Most channels in one packet.
pub const MAX_SOUND_CHANNELS: usize = 8;
/// Header size in bytes.
const SOUND_HEADER: usize = 128;
/// Bytes of one packet entry.
const ENTRY: usize = 32;

const fn section_len() -> usize {
    SOUND_HEADER + PACKETS * ENTRY + SAMPLES * 4
}

/// One packet as the studio reads it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SoundPacket<'a> {
    /// OS monotonic time of the first frame ([`crate::os_monotonic_nanos`]).
    pub os_nanos: u64,
    /// Frames per second.
    pub rate: u32,
    /// Channels, interleaved in `samples`.
    pub channels: u16,
    /// Interleaved samples, a whole number of frames.
    pub samples: &'a [f32],
}

/// One mapped sound section. The studio [`SoundRing::create`]s it; the helper
/// [`SoundRing::open`]s it by name.
pub struct SoundRing {
    map: os::Mapping,
}

impl std::fmt::Debug for SoundRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoundRing")
            .field("name", &self.map.name())
            .finish()
    }
}

impl SoundRing {
    /// Creates a new section with a unique name (studio side), before the helper is
    /// spawned.
    pub fn create() -> io::Result<Self> {
        let map = os::Mapping::create(section_len())?;
        let ring = Self { map };
        ring.word(8).store(SAMPLES as u32, Ordering::Relaxed);
        ring.word(12).store(PACKETS as u32, Ordering::Relaxed);
        ring.word(4).store(SOUND_VERSION, Ordering::Relaxed);
        ring.word(0).store(SOUND_MAGIC, Ordering::Release);
        Ok(ring)
    }

    /// Opens a section the studio created (helper side) and checks its header.
    pub fn open(name: &str) -> io::Result<Self> {
        let map = os::Mapping::open(name, section_len())?;
        let ring = Self { map };
        let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
        if ring.word(0).load(Ordering::Acquire) != SOUND_MAGIC {
            return Err(bad("sound section magic"));
        }
        if ring.word(4).load(Ordering::Relaxed) != SOUND_VERSION
            || ring.word(8).load(Ordering::Relaxed) != SAMPLES as u32
            || ring.word(12).load(Ordering::Relaxed) != PACKETS as u32
        {
            return Err(bad("sound section layout"));
        }
        Ok(ring)
    }

    /// The name the helper opens.
    pub fn name(&self) -> &str {
        self.map.name()
    }

    /// Packets the helper dropped because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.dword(48).load(Ordering::Relaxed)
    }

    /// Helper side: appends one packet of `frames` frames given as one plane per
    /// channel, with the OS time of its first frame. Returns false (and counts the
    /// drop) when it does not fit or is malformed. Never waits or allocates.
    pub fn write_planar(&self, os_nanos: u64, rate: u32, planes: &[&[f32]], frames: usize) -> bool {
        let channels = planes.len();
        let fits_planes = planes.iter().all(|plane| plane.len() >= frames);
        let Some(count) = frames.checked_mul(channels) else {
            return self.drop_packet();
        };
        if channels == 0 || channels > MAX_SOUND_CHANNELS || !fits_planes || frames == 0 {
            return self.drop_packet();
        }
        let written = self.dword(16).load(Ordering::Relaxed);
        let read = self.dword(24).load(Ordering::Acquire);
        let packets = self.dword(32).load(Ordering::Relaxed);
        let packets_read = self.dword(40).load(Ordering::Acquire);
        let used = written.saturating_sub(read);
        if used.saturating_add(count as u64) > SAMPLES as u64
            || packets.saturating_sub(packets_read) >= PACKETS as u64
        {
            return self.drop_packet();
        }
        let base = self.samples_ptr();
        let mut at = (written % SAMPLES as u64) as usize;
        for frame in 0..frames {
            for plane in planes {
                // SAFETY: `at < SAMPLES`, so the write is inside the sample area. The
                // range `written..written + count` is free: the studio has read
                // everything before `read` and `used + count <= SAMPLES`, so the
                // studio does not read these samples until `written` moves past them.
                unsafe { base.add(at).write(plane[frame]) };
                at += 1;
                if at == SAMPLES {
                    at = 0;
                }
            }
        }
        let entry = SOUND_HEADER + (packets % PACKETS as u64) as usize * ENTRY;
        self.entry_dword(entry).store(written, Ordering::Relaxed);
        self.entry_dword(entry + 8)
            .store(os_nanos, Ordering::Relaxed);
        self.entry_word(entry + 16)
            .store(count as u32, Ordering::Relaxed);
        self.entry_word(entry + 20).store(rate, Ordering::Relaxed);
        self.entry_word(entry + 24)
            .store(channels as u32, Ordering::Relaxed);
        self.dword(16)
            .store(written + count as u64, Ordering::Release);
        self.dword(32).store(packets + 1, Ordering::Release);
        true
    }

    /// Studio side: calls `each` for every packet written since the last call, in
    /// order, with its samples copied into `scratch`. A malformed packet (the helper
    /// is untrusted) ends the read and skips everything written so far. Never waits;
    /// allocates only to grow `scratch`.
    pub fn read(&self, scratch: &mut Vec<f32>, mut each: impl FnMut(SoundPacket<'_>)) {
        let packets = self.dword(32).load(Ordering::Acquire);
        let written = self.dword(16).load(Ordering::Acquire);
        let mut packets_read = self.dword(40).load(Ordering::Relaxed);
        let mut read = self.dword(24).load(Ordering::Relaxed);
        if packets.saturating_sub(packets_read) > PACKETS as u64
            || written.saturating_sub(read) > SAMPLES as u64
        {
            // A helper that broke the protocol: skip to its present.
            self.skip(written, packets);
            return;
        }
        while packets_read < packets {
            let entry = SOUND_HEADER + (packets_read % PACKETS as u64) as usize * ENTRY;
            let first = self.entry_dword(entry).load(Ordering::Relaxed);
            let os_nanos = self.entry_dword(entry + 8).load(Ordering::Relaxed);
            let count = self.entry_word(entry + 16).load(Ordering::Relaxed) as usize;
            let rate = self.entry_word(entry + 20).load(Ordering::Relaxed);
            let channels = self.entry_word(entry + 24).load(Ordering::Relaxed) as usize;
            let valid = first == read
                && (1..=MAX_SOUND_CHANNELS).contains(&channels)
                && count > 0
                && count.is_multiple_of(channels)
                && read.saturating_add(count as u64) <= written;
            if !valid {
                self.skip(written, packets);
                return;
            }
            if scratch.len() < count {
                scratch.resize(count, 0.0);
            }
            let start = (read % SAMPLES as u64) as usize;
            let head = count.min(SAMPLES - start);
            let base = self.samples_ptr();
            // SAFETY: both ranges are inside the sample area (`start < SAMPLES`,
            // `head <= SAMPLES - start`, `count - head <= SAMPLES`), and `scratch`
            // holds at least `count` samples and is ours. The helper wrote these
            // samples before it published `written` (release, read with acquire)
            // and does not write them again until `read` passes them. A raw copy
            // never forms a reference to memory the helper may write.
            unsafe {
                std::ptr::copy_nonoverlapping(base.add(start), scratch.as_mut_ptr(), head);
                std::ptr::copy_nonoverlapping(base, scratch.as_mut_ptr().add(head), count - head);
            }
            read += count as u64;
            packets_read += 1;
            each(SoundPacket {
                os_nanos,
                rate,
                channels: channels as u16,
                samples: &scratch[..count],
            });
        }
        self.dword(24).store(read, Ordering::Release);
        self.dword(40).store(packets_read, Ordering::Release);
    }

    fn skip(&self, written: u64, packets: u64) {
        self.dword(24).store(written, Ordering::Release);
        self.dword(40).store(packets, Ordering::Release);
    }

    fn drop_packet(&self) -> bool {
        self.dword(48).fetch_add(1, Ordering::Relaxed);
        false
    }

    fn samples_ptr(&self) -> *mut f32 {
        // SAFETY: the sample area starts inside the mapping, 4-byte aligned (the
        // mapping is page-aligned and the offset is a multiple of 4).
        unsafe {
            self.map
                .as_ptr()
                .add(SOUND_HEADER + PACKETS * ENTRY)
                .cast::<f32>()
        }
    }

    fn word(&self, offset: usize) -> &AtomicU32 {
        debug_assert!(offset + 4 <= SOUND_HEADER && offset.is_multiple_of(4));
        self.entry_word(offset)
    }

    fn dword(&self, offset: usize) -> &AtomicU64 {
        debug_assert!(offset + 8 <= SOUND_HEADER && offset.is_multiple_of(8));
        self.entry_dword(offset)
    }

    fn entry_word(&self, offset: usize) -> &AtomicU32 {
        debug_assert!(offset + 4 <= SOUND_HEADER + PACKETS * ENTRY);
        // SAFETY: `offset` is a multiple of 4 inside the header or the entry table,
        // both in the page-aligned mapping, and both processes touch these words only
        // through atomics, so an `AtomicU32` view is valid for the mapping's life.
        unsafe { &*self.map.as_ptr().add(offset).cast::<AtomicU32>() }
    }

    fn entry_dword(&self, offset: usize) -> &AtomicU64 {
        debug_assert!(offset + 8 <= SOUND_HEADER + PACKETS * ENTRY);
        // SAFETY: as `entry_word`, with 8-byte alignment of `offset`.
        unsafe { &*self.map.as_ptr().add(offset).cast::<AtomicU64>() }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
    use super::*;

    fn collect(ring: &SoundRing, scratch: &mut Vec<f32>) -> Vec<(u64, u32, u16, Vec<f32>)> {
        let mut out = Vec::new();
        ring.read(scratch, |p| {
            out.push((p.os_nanos, p.rate, p.channels, p.samples.to_vec()));
        });
        out
    }

    #[test]
    fn packets_cross_in_order_interleaved() {
        let studio = SoundRing::create().unwrap();
        let helper = SoundRing::open(studio.name()).unwrap();
        let (l, r) = ([1.0, 2.0, 3.0], [-1.0, -2.0, -3.0]);
        assert!(helper.write_planar(100, 48_000, &[&l, &r], 3));
        assert!(helper.write_planar(200, 48_000, &[&l[..1]], 1));
        let mut scratch = Vec::new();
        let got = collect(&studio, &mut scratch);
        assert_eq!(
            got,
            vec![
                (100, 48_000, 2, vec![1.0, -1.0, 2.0, -2.0, 3.0, -3.0]),
                (200, 48_000, 1, vec![1.0]),
            ]
        );
        assert!(collect(&studio, &mut scratch).is_empty());
    }

    #[test]
    fn a_full_ring_drops_new_packets_and_wraps_after_reads() {
        let studio = SoundRing::create().unwrap();
        let helper = SoundRing::open(studio.name()).unwrap();
        let plane = vec![0.5f32; 1000];
        let mut accepted = 0;
        while helper.write_planar(1, 48_000, &[&plane, &plane], 1000) {
            accepted += 1;
        }
        assert_eq!(accepted, SAMPLES / 2000);
        assert_eq!(studio.dropped(), 1);
        let mut scratch = Vec::new();
        assert_eq!(collect(&studio, &mut scratch).len(), accepted);
        // Room again, across the wrap.
        for i in 0..accepted * 3 {
            assert!(helper.write_planar(i as u64, 48_000, &[&plane, &plane], 1000));
            let got = collect(&studio, &mut scratch);
            assert_eq!(got.len(), 1);
            assert!(got[0].3.iter().all(|s| *s == 0.5));
        }
    }

    #[test]
    fn a_malformed_entry_skips_to_the_present() {
        let studio = SoundRing::create().unwrap();
        let helper = SoundRing::open(studio.name()).unwrap();
        assert!(helper.write_planar(1, 48_000, &[&[0.1, 0.2]], 2));
        // A helper that lies about a packet's length.
        helper
            .entry_word(SOUND_HEADER + 16)
            .store(9_999, Ordering::Relaxed);
        let mut scratch = Vec::new();
        assert!(collect(&studio, &mut scratch).is_empty());
        assert!(helper.write_planar(2, 48_000, &[&[0.3]], 1));
        assert_eq!(
            collect(&studio, &mut scratch),
            vec![(2, 48_000, 1, vec![0.3])]
        );
    }

    #[test]
    fn too_many_channels_or_a_short_plane_is_dropped() {
        let studio = SoundRing::create().unwrap();
        let plane = [0.0f32; 4];
        let planes = [&plane[..]; MAX_SOUND_CHANNELS + 1];
        assert!(!studio.write_planar(1, 48_000, &planes, 4));
        assert!(!studio.write_planar(1, 48_000, &[&plane[..2]], 4));
        assert_eq!(studio.dropped(), 2);
    }
}
