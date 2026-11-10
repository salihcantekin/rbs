//! The replay buffer's packet ring and clip cutting (ADR-0088, spec
//! `docs/specs/recording.md`, "Replay buffer").
//!
//! The ring holds **encoded packets** in arrival order, never raw frames. Payloads are
//! `Bytes`, so a clip is a list of reference-count increments and no data is copied.
//! This module is pure logic with no threads and no clock: the ring node (a later
//! change) feeds it from `Node::process`, a short-lived writer task consumes clips.
//!
//! Real-time contract of [`PacketRing::push`]: the ring's storage is reserved in
//! [`PacketRing::new`]; `push` never grows it. When the ring is full, or a limit is
//! exceeded, the **oldest** packets are evicted (never the newest). Evicted entries are
//! moved into a caller-provided vector so a reaper thread frees them; if that vector
//! has no room left the entry is dropped in place and counted in
//! [`PacketRing::inline_frees`].

use std::collections::VecDeque;
use std::time::Duration;

use rbs_core::media::{EncodedPacket, MediaError, StreamIndex};
use rbs_core::time::Timestamp;

/// What bounds the buffer, as chosen by the user (all saved settings, ADR-0088 decision 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayLimits {
    /// Wanted length of the past, 10 seconds to 30 minutes in the settings UI.
    pub length: Duration,
    /// Most payload bytes the ring may hold.
    pub memory_limit: usize,
    /// Most packets the ring may hold: the storage reserved at arm time.
    pub capacity: usize,
}

/// Why the ring holds less than the wanted length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldLimit {
    /// The memory limit (or the reserved packet capacity) was reached first.
    Memory,
}

/// One packet of one stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingEntry {
    /// Index of the stream in the muxer's stream list.
    pub stream: StreamIndex,
    /// The packet; its payload is shared, not copied.
    pub packet: EncodedPacket,
}

/// What the ring holds now, for `replay.status` and the Settings card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Held {
    /// Length of the usable past: from the oldest video keyframe to the newest packet.
    pub duration: Duration,
    /// Payload bytes held.
    pub bytes: usize,
    /// Packets held.
    pub packets: usize,
    /// `Some(Memory)` when memory, not the length setting, is why `duration` is short.
    pub limited_by: Option<HoldLimit>,
}

/// Bounds of a cut clip, for the file name, the report and the `replay.saved` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipBounds {
    /// Master-clock time of the first sample (a video keyframe).
    pub start: Timestamp,
    /// Master-clock time of the newest packet in the clip.
    pub end: Timestamp,
}

impl ClipBounds {
    /// Length of the clip. May exceed the request by up to one keyframe interval.
    pub fn duration(&self) -> Duration {
        self.end.saturating_since(self.start)
    }
}

/// Why a clip cannot be cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipError {
    /// The ring holds no video keyframe yet, so nothing decodable can be saved.
    NoKeyframe,
}

impl std::fmt::Display for ClipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKeyframe => f.write_str("the replay buffer holds no video keyframe yet"),
        }
    }
}

impl std::error::Error for ClipError {}

/// Packets cut from the ring, ready for the writer task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    /// The cut, in arrival order.
    pub entries: Vec<RingEntry>,
    /// Bounds on the master clock.
    pub bounds: ClipBounds,
}

impl Clip {
    /// The entries with timestamps shifted so the earliest sample is 0 (ADR-0088
    /// decision 4). Runs on the writer task, never in `process`. Fails only if an
    /// adapter produced an inconsistent packet, which `EncodedPacket::new` rejects.
    pub fn rebased(&self) -> Result<Vec<RingEntry>, MediaError> {
        let origin = self.bounds.start;
        let shift =
            |t: Timestamp| Timestamp::from_nanos(t.as_nanos().saturating_sub(origin.as_nanos()));
        self.entries
            .iter()
            .map(|e| {
                let p = &e.packet;
                let mut out = EncodedPacket::new(
                    p.codec(),
                    p.bitstream(),
                    shift(p.pts()),
                    shift(p.dts()),
                    p.keyframe(),
                    p.data().clone(),
                )?;
                if let Some(d) = p.duration() {
                    out = out.with_duration(d);
                }
                Ok(RingEntry {
                    stream: e.stream,
                    packet: out,
                })
            })
            .collect()
    }
}

/// The rolling packet buffer.
#[derive(Debug)]
pub struct PacketRing {
    entries: VecDeque<RingEntry>,
    /// Sequence numbers (see `base`) of the video keyframes held, oldest first.
    keys: VecDeque<u64>,
    /// Sequence number of `entries[0]`; entry `i` has number `base + i`.
    base: u64,
    limits: ReplayLimits,
    video: StreamIndex,
    bytes: usize,
    limited_by: Option<HoldLimit>,
    inline_frees: u64,
}

impl PacketRing {
    /// Creates a ring and reserves all its storage. `video` is the stream whose
    /// keyframes start a clip.
    pub fn new(limits: ReplayLimits, video: StreamIndex) -> Self {
        Self {
            // One extra slot: a push evicts after inserting, never grows.
            entries: VecDeque::with_capacity(limits.capacity.saturating_add(1)),
            keys: VecDeque::with_capacity(limits.capacity.saturating_add(1)),
            base: 0,
            limits,
            video,
            bytes: 0,
            limited_by: None,
            inline_frees: 0,
        }
    }

    /// **Hot.** Adds one packet. Evicts the oldest packets while a limit is exceeded and
    /// moves them to `evicted` (reserved by the caller). Does not allocate.
    pub fn push(
        &mut self,
        stream: StreamIndex,
        packet: EncodedPacket,
        evicted: &mut Vec<RingEntry>,
    ) {
        self.bytes += packet.data().len();
        let newest = packet.dts();
        if stream == self.video && packet.keyframe() {
            self.keys.push_back(self.base + self.entries.len() as u64);
        }
        self.entries.push_back(RingEntry { stream, packet });
        loop {
            let over_memory =
                self.bytes > self.limits.memory_limit || self.entries.len() > self.limits.capacity;
            let over_length = self
                .entries
                .front()
                .is_some_and(|f| newest.saturating_since(f.packet.dts()) > self.limits.length);
            if !(over_memory || over_length) || self.entries.len() <= 1 {
                break;
            }
            if over_memory {
                self.limited_by = Some(HoldLimit::Memory);
            }
            if let Some(old) = self.entries.pop_front() {
                self.bytes -= old.packet.data().len();
                if self.keys.front() == Some(&self.base) {
                    self.keys.pop_front();
                }
                self.base += 1;
                if evicted.len() < evicted.capacity() {
                    evicted.push(old);
                } else {
                    self.inline_frees += 1;
                    drop(old);
                }
            }
        }
        // Once the length setting is satisfied without memory pressure the label clears.
        if self.limited_by.is_some()
            && self.bytes < self.limits.memory_limit - self.limits.memory_limit / 10
        {
            self.limited_by = None;
        }
    }

    /// Entries dropped inside `push` because the reaper vector was full.
    pub fn inline_frees(&self) -> u64 {
        self.inline_frees
    }

    /// What the ring holds now.
    pub fn held(&self) -> Held {
        let duration = match (self.first_keyframe(), self.entries.back()) {
            (Some(k), Some(last)) => last
                .packet
                .dts()
                .saturating_since(self.entries[k].packet.dts()),
            _ => Duration::ZERO,
        };
        let limited_by = if duration < self.limits.length {
            self.limited_by
        } else {
            None
        };
        Held {
            duration,
            bytes: self.bytes,
            packets: self.entries.len(),
            limited_by,
        }
    }

    /// Drops everything (disarm or settings change). Payloads move to `evicted` for the
    /// reaper while it has room.
    pub fn clear(&mut self, evicted: &mut Vec<RingEntry>) {
        self.base += self.entries.len() as u64;
        self.keys.clear();
        while let Some(old) = self.entries.pop_front() {
            if evicted.len() < evicted.capacity() {
                evicted.push(old);
            }
        }
        self.bytes = 0;
        self.limited_by = None;
    }

    /// Index in `entries` of the oldest video keyframe.
    fn first_keyframe(&self) -> Option<usize> {
        self.keys.front().map(|k| (*k - self.base) as usize)
    }

    /// Cuts a clip covering the last `seconds` (the whole ring when `None`). The clip
    /// starts at the latest video keyframe at or before that point, or at the oldest
    /// keyframe if the ring is shorter, so it is never shorter than asked unless the
    /// ring is. It ends at the newest packet. Audio is cut at the same instant as the
    /// video. `out` is reserved by the caller; packets are cloned (payload shared).
    pub fn cut(
        &self,
        seconds: Option<Duration>,
        out: &mut Vec<RingEntry>,
    ) -> Result<ClipBounds, ClipError> {
        let newest = self
            .entries
            .back()
            .ok_or(ClipError::NoKeyframe)?
            .packet
            .dts();
        let target = seconds.map(|s| newest - s);
        let mut start_idx = self.first_keyframe();
        if let Some(t) = target {
            for k in &self.keys {
                let i = (*k - self.base) as usize;
                if self.entries[i].packet.dts() > t {
                    break;
                }
                start_idx = Some(i);
            }
        }
        let start_idx = start_idx.ok_or(ClipError::NoKeyframe)?;
        let start = self.entries[start_idx].packet.dts();
        out.clear();
        for (i, e) in self.entries.iter().enumerate() {
            let keep = if e.stream == self.video {
                i >= start_idx
            } else {
                e.packet.pts() >= start
            };
            if keep {
                out.push(e.clone());
            }
        }
        Ok(ClipBounds { start, end: newest })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rbs_core::media::{BitstreamFormat, CodecId};

    const V: StreamIndex = StreamIndex(0);
    const A: StreamIndex = StreamIndex(1);

    fn ms(n: u64) -> Timestamp {
        Timestamp::from_nanos(n * 1_000_000)
    }

    fn pkt(codec: CodecId, t: u64, key: bool, size: usize) -> EncodedPacket {
        EncodedPacket::new(
            codec,
            BitstreamFormat::AnnexB,
            ms(t),
            ms(t),
            key,
            Bytes::from(vec![0_u8; size]),
        )
        .unwrap()
    }

    fn video(t: u64, key: bool, size: usize) -> EncodedPacket {
        pkt(CodecId::H264, t, key, size)
    }

    fn audio(t: u64) -> EncodedPacket {
        pkt(CodecId::Aac, t, true, 10)
    }

    fn ring(length_ms: u64, memory: usize, capacity: usize) -> PacketRing {
        PacketRing::new(
            ReplayLimits {
                length: Duration::from_millis(length_ms),
                memory_limit: memory,
                capacity,
            },
            V,
        )
    }

    /// 1 s keyframe interval, 100 ms video packets, audio every 100 ms, `n` video packets.
    fn fill(r: &mut PacketRing, n: u64) {
        let mut ev = Vec::with_capacity(8);
        for i in 0..n {
            r.push(V, video(i * 100, i % 10 == 0, 100), &mut ev);
            r.push(A, audio(i * 100), &mut ev);
            ev.clear();
        }
    }

    #[test]
    fn evicts_oldest_beyond_length() {
        let mut r = ring(2_000, usize::MAX, 10_000);
        fill(&mut r, 50); // 5 s
        let h = r.held();
        assert!(h.duration <= Duration::from_millis(2_000));
        assert_eq!(h.limited_by, None);
    }

    #[test]
    fn memory_limit_reports_why_it_holds_less() {
        let mut r = ring(60_000, 3_000, 10_000);
        fill(&mut r, 100);
        let h = r.held();
        assert!(h.bytes <= 3_000);
        assert!(h.duration < Duration::from_secs(60));
        assert_eq!(h.limited_by, Some(HoldLimit::Memory));
    }

    #[test]
    fn capacity_never_grows_and_newest_is_kept() {
        let mut r = ring(60_000, usize::MAX, 20);
        fill(&mut r, 100);
        assert_eq!(r.held().packets, 20);
        let mut out = Vec::new();
        let b = r.cut(None, &mut out).unwrap();
        assert_eq!(b.end, ms(9_900));
    }

    #[test]
    fn evictions_go_to_the_reaper_without_inline_frees() {
        let mut r = ring(500, usize::MAX, 10_000);
        let mut ev = Vec::with_capacity(64);
        for i in 0..30 {
            r.push(V, video(i * 100, i % 10 == 0, 10), &mut ev);
        }
        assert!(!ev.is_empty());
        assert_eq!(r.inline_frees(), 0);
        // A full reaper vector falls back to freeing in place, and says so.
        let mut tiny: Vec<RingEntry> = Vec::new();
        for i in 30..40 {
            r.push(V, video(i * 100, false, 10), &mut tiny);
        }
        assert!(r.inline_frees() > 0);
    }

    #[test]
    fn clip_starts_at_keyframe_at_or_before_request_and_is_never_shorter() {
        let mut r = ring(60_000, usize::MAX, 10_000);
        fill(&mut r, 100); // newest 9.9 s, keyframes every 1 s
        let mut out = Vec::new();
        let b = r.cut(Some(Duration::from_millis(2_500)), &mut out).unwrap();
        assert_eq!(b.start, ms(7_000)); // 9.9 - 2.5 = 7.4 -> keyframe at 7.0
        assert!(b.duration() >= Duration::from_millis(2_500));
        assert!(
            out.iter()
                .find(|e| e.stream == V)
                .unwrap()
                .packet
                .keyframe()
        );
    }

    #[test]
    fn audio_is_cut_at_the_same_instant() {
        let mut r = ring(60_000, usize::MAX, 10_000);
        fill(&mut r, 100);
        let mut out = Vec::new();
        let b = r.cut(Some(Duration::from_millis(2_500)), &mut out).unwrap();
        let first_audio = out.iter().find(|e| e.stream == A).unwrap();
        assert_eq!(first_audio.packet.pts(), b.start);
        assert!(
            out.iter()
                .filter(|e| e.stream == A)
                .all(|e| e.packet.pts() >= b.start)
        );
    }

    #[test]
    fn short_ring_gives_the_oldest_keyframe() {
        let mut r = ring(60_000, usize::MAX, 10_000);
        fill(&mut r, 15);
        let mut out = Vec::new();
        let b = r.cut(Some(Duration::from_secs(60)), &mut out).unwrap();
        assert_eq!(b.start, ms(0));
    }

    #[test]
    fn no_keyframe_is_an_error() {
        let mut r = ring(60_000, usize::MAX, 10_000);
        let mut ev = Vec::with_capacity(4);
        r.push(V, video(0, false, 10), &mut ev);
        let mut out = Vec::new();
        assert_eq!(r.cut(None, &mut out), Err(ClipError::NoKeyframe));
        assert_eq!(r.held().duration, Duration::ZERO);
    }

    #[test]
    fn rebase_starts_at_zero_and_shares_payloads() {
        let mut r = ring(60_000, usize::MAX, 10_000);
        fill(&mut r, 100);
        let mut out = Vec::new();
        let b = r.cut(Some(Duration::from_secs(1)), &mut out).unwrap();
        let clip = Clip {
            entries: out,
            bounds: b,
        };
        let rebased = clip.rebased().unwrap();
        assert_eq!(rebased.len(), clip.entries.len());
        let min = rebased.iter().map(|e| e.packet.dts()).min().unwrap();
        assert_eq!(min, Timestamp::ZERO);
        assert_eq!(
            rebased[0].packet.data().as_ptr(),
            clip.entries[0].packet.data().as_ptr()
        );
    }

    #[test]
    fn push_does_not_allocate_once_reserved() {
        let mut r = ring(500, 1_000_000, 64);
        let mut ev: Vec<RingEntry> = Vec::with_capacity(8);
        let mut warm: Vec<EncodedPacket> =
            (0..100).map(|i| video(i * 100, i % 10 == 0, 10)).collect();
        let mut timed: Vec<EncodedPacket> = (100..200)
            .map(|i| video(i * 100, i % 10 == 0, 10))
            .collect();
        // Warm up so the ring is full and every code path ran once.
        for p in warm.drain(..) {
            r.push(V, p, &mut ev);
            ev.clear();
        }
        let info = allocation_counter::measure(|| {
            for p in timed.drain(..) {
                r.push(V, p, &mut ev);
                ev.clear();
            }
        });
        assert_eq!(info.count_total, 0, "push allocated: {info:?}");
    }
}
