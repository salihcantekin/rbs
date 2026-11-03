//! The producer-side staging queue and its drop policy (output-ports spec, "Drop
//! policy for network transports", rules 1, 2 and 5 for packet input).
//!
//! Pure logic over a `VecDeque`, touched only by the session's producer side, so
//! dropping "the oldest" needs no synchronisation. The hand-off to the I/O thread
//! (committed items, never dropped) is described to [`Staging::admit`] by its byte
//! count and oldest timestamp, so the limits cover staging plus hand-off.

use std::collections::VecDeque;

/// What an item is, for the drop policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// A video keyframe: a decoding entry point.
    Key,
    /// A video frame that depends on earlier frames of its GOP.
    Inter,
    /// Audio: independent packets.
    Audio,
}

impl Class {
    fn is_video(self) -> bool {
        self != Class::Audio
    }
}

/// One staged item.
#[derive(Debug)]
pub(crate) struct Entry<T> {
    pub item: T,
    pub class: Class,
    /// Master time (decode order) in nanoseconds.
    pub ts: u64,
    pub bytes: u64,
}

/// Queue bounds, in the units of [`Entry`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub max_bytes: u64,
    pub max_duration_ns: u64,
}

/// What the hand-off holds: committed items, counted against the limits.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Committed {
    pub bytes: u64,
    /// Timestamp of the oldest committed item not yet sent, if any.
    pub oldest_ts: Option<u64>,
}

/// The outcome of [`Staging::admit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Admitted {
    /// Whether the new item was staged.
    pub queued: bool,
    /// Older staged items dropped by this call.
    pub dropped: u32,
    /// A GOP was cut: video of the current GOP was dropped or refused, so the rest
    /// of it is refused until the next keyframe. The session asks for a keyframe.
    pub cut: bool,
}

#[derive(Debug)]
pub(crate) struct Staging<T> {
    queue: VecDeque<Entry<T>>,
    bytes: u64,
    /// The current GOP lost frames: refuse its remaining inter frames.
    refusing: bool,
}

impl<T> Staging<T> {
    pub(crate) fn with_capacity(items: usize) -> Self {
        Self {
            queue: VecDeque::with_capacity(items),
            bytes: 0,
            refusing: false,
        }
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub(crate) fn front_ts(&self) -> Option<u64> {
        self.queue.front().map(|e| e.ts)
    }

    /// Takes the oldest staged item (to commit it to the hand-off).
    pub(crate) fn pop(&mut self) -> Option<Entry<T>> {
        let entry = self.queue.pop_front()?;
        self.bytes -= entry.bytes;
        Some(entry)
    }

    /// Whether the current GOP was cut and its inter frames are refused.
    #[cfg(test)]
    pub(crate) fn is_refusing(&self) -> bool {
        self.refusing
    }

    fn over(&self, entry: &Entry<T>, limits: Limits, committed: Committed) -> bool {
        let held = self.bytes + committed.bytes;
        // One item larger than the limit still goes into an empty queue.
        if held > 0 && held + entry.bytes > limits.max_bytes {
            return true;
        }
        let oldest = match (committed.oldest_ts, self.front_ts()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        oldest.is_some_and(|oldest| entry.ts.saturating_sub(oldest) > limits.max_duration_ns)
    }

    /// Stages `entry`, applying the drop policy when a limit would be passed:
    ///
    /// 1. Drop whole GOPs of staged video from the oldest, keyframe included, while a
    ///    later keyframe is staged or `entry` is one.
    /// 2. When only the current GOP is left, drop its staged inter frames, refuse
    ///    `entry` if it is one, and refuse the rest of the GOP until a keyframe.
    /// 3. Only then drop staged audio, oldest first.
    /// 4. If still over, refuse `entry` (a refused keyframe cuts its GOP too).
    ///
    /// So delivered video never continues from a missing reference picture.
    pub(crate) fn admit(
        &mut self,
        entry: Entry<T>,
        limits: Limits,
        committed: Committed,
    ) -> Admitted {
        let mut result = Admitted {
            queued: false,
            dropped: 0,
            cut: false,
        };
        match entry.class {
            Class::Inter if self.refusing => return result,
            Class::Key => self.refusing = false,
            _ => {}
        }
        if self.over(&entry, limits, committed) {
            // Rule 1: whole GOPs, oldest first.
            while self.over(&entry, limits, committed) {
                let Some(end) = self.oldest_gop_end(entry.class == Class::Key) else {
                    break;
                };
                result.dropped += self.drop_video(..end, |_| true);
            }
            // Rule 2: cut the current GOP.
            if entry.class != Class::Key && self.over(&entry, limits, committed) {
                let start = self
                    .queue
                    .iter()
                    .rposition(|e| e.class == Class::Key)
                    .unwrap_or(0);
                result.dropped += self.drop_video(start.., |c| c == Class::Inter);
                if entry.class == Class::Inter || result.dropped > 0 {
                    self.refusing = true;
                    result.cut = true;
                }
                if entry.class == Class::Inter {
                    return result;
                }
            }
            // Rule 3: audio, oldest first.
            while self.over(&entry, limits, committed) {
                let Some(i) = self.queue.iter().position(|e| e.class == Class::Audio) else {
                    break;
                };
                if let Some(e) = self.queue.remove(i) {
                    self.bytes -= e.bytes;
                    result.dropped += 1;
                }
            }
            if self.over(&entry, limits, committed) {
                if entry.class == Class::Key {
                    self.refusing = true;
                    result.cut = true;
                }
                return result;
            }
        }
        self.bytes += entry.bytes;
        self.queue.push_back(entry);
        result.queued = true;
        result
    }

    /// End (exclusive) of the oldest staged GOP: the first keyframe that has staged
    /// video before it. With `incoming_key`, all staged video counts as old GOPs.
    fn oldest_gop_end(&self, incoming_key: bool) -> Option<usize> {
        let mut seen_video = false;
        for (i, e) in self.queue.iter().enumerate() {
            if e.class == Class::Key && seen_video {
                return Some(i);
            }
            seen_video |= e.class.is_video();
        }
        (incoming_key && seen_video).then_some(self.queue.len())
    }

    /// Drops the video entries in `range` whose class passes `filter`; returns how many.
    fn drop_video(
        &mut self,
        range: impl std::ops::RangeBounds<usize>,
        filter: impl Fn(Class) -> bool,
    ) -> u32 {
        let mut index = 0;
        let mut dropped = 0;
        let mut bytes = 0;
        self.queue.retain(|e| {
            let keep = !(range.contains(&index) && e.class.is_video() && filter(e.class));
            index += 1;
            if !keep {
                dropped += 1;
                bytes += e.bytes;
            }
            keep
        });
        self.bytes -= bytes;
        dropped
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn entry(n: u64, class: Class, bytes: u64) -> Entry<u64> {
        Entry {
            item: n,
            class,
            ts: n * 10 * MS,
            bytes,
        }
    }

    fn limits(max_bytes: u64) -> Limits {
        Limits {
            max_bytes,
            max_duration_ns: u64::MAX,
        }
    }

    fn items(s: &Staging<u64>) -> Vec<u64> {
        s.queue.iter().map(|e| e.item).collect()
    }

    #[test]
    fn drops_whole_old_gops_first() {
        let mut s = Staging::with_capacity(16);
        let none = Committed::default();
        // GOP A: 0 key, 1 inter, 2 audio; GOP B: 3 key, 4 inter.
        for (n, c) in [
            (0, Class::Key),
            (1, Class::Inter),
            (2, Class::Audio),
            (3, Class::Key),
            (4, Class::Inter),
        ] {
            assert!(s.admit(entry(n, c, 10), limits(50), none).queued);
        }
        let r = s.admit(entry(5, Class::Inter, 10), limits(50), none);
        assert_eq!((r.queued, r.dropped, r.cut), (true, 2, false));
        assert_eq!(items(&s), vec![2, 3, 4, 5], "GOP A video gone, audio kept");
    }

    #[test]
    fn cuts_the_current_gop_and_refuses_until_a_keyframe() {
        let mut s = Staging::with_capacity(16);
        let none = Committed::default();
        for (n, c) in [(0, Class::Key), (1, Class::Inter), (2, Class::Inter)] {
            s.admit(entry(n, c, 10), limits(30), none);
        }
        let r = s.admit(entry(3, Class::Inter, 10), limits(30), none);
        assert_eq!((r.queued, r.dropped, r.cut), (false, 2, true));
        assert_eq!(items(&s), vec![0], "keyframe kept, inter frames dropped");
        assert!(s.is_refusing());
        // Even with room again, the GOP stays cut.
        let r = s.admit(entry(4, Class::Inter, 1), limits(1000), none);
        assert!(!r.queued && !r.cut);
        // Audio still flows; the next keyframe resumes video.
        assert!(
            s.admit(entry(5, Class::Audio, 1), limits(1000), none)
                .queued
        );
        assert!(s.admit(entry(6, Class::Key, 1), limits(1000), none).queued);
        assert!(
            s.admit(entry(7, Class::Inter, 1), limits(1000), none)
                .queued
        );
        assert_eq!(items(&s), vec![0, 5, 6, 7]);
    }

    #[test]
    fn incoming_keyframe_replaces_all_staged_video() {
        let mut s = Staging::with_capacity(16);
        let none = Committed::default();
        for (n, c) in [(0, Class::Inter), (1, Class::Audio), (2, Class::Inter)] {
            s.admit(entry(n, c, 10), limits(30), none);
        }
        let r = s.admit(entry(3, Class::Key, 10), limits(30), none);
        assert_eq!((r.queued, r.dropped, r.cut), (true, 2, false));
        assert_eq!(items(&s), vec![1, 3]);
    }

    #[test]
    fn audio_goes_only_after_video() {
        let mut s = Staging::with_capacity(16);
        let none = Committed::default();
        for n in 0..3 {
            s.admit(entry(n, Class::Audio, 10), limits(30), none);
        }
        let r = s.admit(entry(3, Class::Audio, 10), limits(30), none);
        assert_eq!((r.queued, r.dropped), (true, 1));
        assert_eq!(items(&s), vec![1, 2, 3]);
        // Committed bytes alone fill the limit: nothing to drop, the item is refused.
        let mut s = Staging::with_capacity(4);
        let full = Committed {
            bytes: 30,
            oldest_ts: Some(0),
        };
        let r = s.admit(entry(0, Class::Key, 10), limits(30), full);
        assert_eq!((r.queued, r.cut), (false, true), "a refused keyframe cuts");
        assert!(
            !s.admit(entry(1, Class::Inter, 1), limits(1000), none)
                .queued
        );
    }

    #[test]
    fn duration_limit_counts_committed_items() {
        let mut s = Staging::with_capacity(16);
        let limits = Limits {
            max_bytes: u64::MAX,
            max_duration_ns: 25 * MS,
        };
        let committed = Committed {
            bytes: 1,
            oldest_ts: Some(0),
        };
        assert!(s.admit(entry(1, Class::Key, 1), limits, committed).queued);
        assert!(s.admit(entry(2, Class::Inter, 1), limits, committed).queued);
        // 30 ms after the committed item: over, the current GOP is cut.
        let r = s.admit(entry(3, Class::Inter, 1), limits, committed);
        assert_eq!((r.queued, r.cut), (false, true));
    }

    /// Random streams through random limits: delivered video never continues a GOP
    /// that lost a frame.
    #[test]
    fn delivered_video_never_misses_a_reference() {
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut drops = 0;
        for _ in 0..300 {
            let mut s = Staging::with_capacity(64);
            let max = 20 + rand() % 200;
            let mut generated = Vec::new();
            let mut delivered = Vec::new();
            for n in 0..400 {
                let class = match (n, rand() % 10) {
                    (0, _) | (_, 0) => Class::Key,
                    (_, 1..=3) => Class::Audio,
                    _ => Class::Inter,
                };
                generated.push(class);
                let r = s.admit(
                    entry(n, class, 1 + rand() % 20),
                    limits(max),
                    Committed::default(),
                );
                drops += r.dropped;
                if rand() % 3 == 0 {
                    delivered.extend(s.pop().map(|e| e.item));
                }
                assert_eq!(s.bytes, s.queue.iter().map(|e| e.bytes).sum::<u64>());
            }
            while let Some(e) = s.pop() {
                delivered.push(e.item);
            }
            // Before each delivered inter frame, every generated video item back to the
            // previous delivered video item must be absent: nothing was lost between.
            let mut previous: Option<u64> = None;
            for &n in &delivered {
                let class = generated[n as usize];
                if class == Class::Inter {
                    let from = previous.map_or(0, |p| p + 1);
                    let lost = (from..n).any(|m| generated[m as usize].is_video());
                    assert!(
                        !lost,
                        "inter frame {n} follows lost video after {previous:?}"
                    );
                }
                if class.is_video() {
                    previous = Some(n);
                }
            }
        }
        assert!(
            drops > 1000,
            "the limits must actually bind ({drops} drops)"
        );
    }
}
