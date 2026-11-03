//! The producer-side staging queue and its drop policy for container input
//! (output-ports spec, "Drop policy for network transports", rules 1, 3, 4 and 5).
//!
//! The transport never parses the container: a `Media` chunk marked `keyframe` is
//! the GOP boundary (an entry point for every stream), other `Media` chunks depend on
//! it, and `Header` and `Trailer` chunks are never dropped. Pure logic over a
//! `VecDeque`, touched only by the session's producer side. The hand-off to the I/O
//! thread (committed items, never dropped) is described to [`Staging::admit`] by its
//! byte count and oldest timestamp, so the limits cover staging plus hand-off.

use std::collections::VecDeque;

/// What a chunk is, for the drop policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// `Header` or `Trailer`: never dropped, and without a media time.
    Keep,
    /// A `Media` chunk marked `keyframe`: an entry point.
    Key,
    /// A `Media` chunk that continues the GOP before it.
    Inter,
}

impl Class {
    fn is_media(self) -> bool {
        self != Class::Keep
    }
}

/// One staged item.
#[derive(Debug)]
pub(crate) struct Entry<T> {
    pub item: T,
    pub class: Class,
    /// Master time in nanoseconds; ignored for [`Class::Keep`].
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
    /// Timestamp of the oldest committed media chunk not yet sent, if any.
    pub oldest_ts: Option<u64>,
}

/// The outcome of [`Staging::admit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Admitted {
    /// Whether the new item was staged.
    pub queued: bool,
    /// Older staged items dropped by this call.
    pub dropped: u32,
    /// A GOP was cut: a chunk of the current GOP was dropped or refused, so the rest
    /// of it is refused until the next keyframe chunk. The session asks for a keyframe.
    pub cut: bool,
}

#[derive(Debug)]
pub(crate) struct Staging<T> {
    queue: VecDeque<Entry<T>>,
    bytes: u64,
    /// The current GOP lost chunks: refuse its remaining ones.
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

    /// Takes the oldest staged item (to commit it to the hand-off).
    pub(crate) fn pop(&mut self) -> Option<Entry<T>> {
        let entry = self.queue.pop_front()?;
        self.bytes -= entry.bytes;
        Some(entry)
    }

    /// Whether the current GOP was cut and its chunks are refused.
    #[cfg(test)]
    pub(crate) fn is_refusing(&self) -> bool {
        self.refusing
    }

    /// The time of the oldest staged media chunk. `Keep` entries are skipped; they
    /// are few (a header at the start, a trailer at the end).
    fn front_ts(&self) -> Option<u64> {
        self.queue.iter().find(|e| e.class.is_media()).map(|e| e.ts)
    }

    fn over(&self, entry: &Entry<T>, limits: Limits, committed: Committed) -> bool {
        over_with(self.bytes, self.front_ts(), entry, limits, committed)
    }

    /// Stages `entry`, applying the drop policy when a limit would be passed:
    ///
    /// - `Keep` entries are always staged (rule 4).
    /// - Rule 1: drop whole GOPs of staged chunks from the oldest, keyframe included,
    ///   while a later keyframe chunk is staged or `entry` is one.
    /// - Then, for an inter chunk: drop the current GOP's staged inter chunks, refuse
    ///   `entry`, and refuse the rest of the GOP until a keyframe chunk.
    /// - A keyframe chunk that still does not fit is refused and cuts its GOP too.
    ///
    /// So delivered media never continues from a missing chunk. Each rule is one pass
    /// over the queue, so a call is linear in the staged items.
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
        if entry.class.is_media() && self.over(&entry, limits, committed) {
            if entry.class == Class::Key && !self.fits_alone(&entry, limits, committed) {
                // Even with every staged chunk dropped it would not fit: keep the
                // staged GOPs and refuse it.
                self.refusing = true;
                result.cut = true;
                return result;
            }
            // Rule 1: whole GOPs, oldest first.
            if let Some(end) = self.old_gops_end(&entry, limits, committed) {
                result.dropped += self.drop_media(..end, |_| true);
            }
            if self.over(&entry, limits, committed) {
                if entry.class == Class::Inter {
                    // Cut the current GOP.
                    let start = self
                        .queue
                        .iter()
                        .rposition(|e| e.class == Class::Key)
                        .unwrap_or(0);
                    result.dropped += self.drop_media(start.., |c| c == Class::Inter);
                }
                self.refusing = true;
                result.cut = true;
                return result;
            }
        }
        self.bytes += entry.bytes;
        self.queue.push_back(entry);
        result.queued = true;
        result
    }

    /// Whether `entry` fits once all staged media is dropped (headers and committed
    /// chunks stay).
    fn fits_alone(&self, entry: &Entry<T>, limits: Limits, committed: Committed) -> bool {
        let keep: u64 = self
            .queue
            .iter()
            .filter(|e| !e.class.is_media())
            .map(|e| e.bytes)
            .sum();
        !over_with(keep, None, entry, limits, committed)
    }

    /// Rule 1 in one scan: the end (exclusive) of the old GOPs to drop, that is the
    /// first GOP boundary (a keyframe chunk with staged media before it) at which
    /// dropping all earlier media is enough, or the last boundary if none is. With a
    /// keyframe `entry`, the end of the queue is a boundary too. `None` without one.
    fn old_gops_end(
        &self,
        entry: &Entry<T>,
        limits: Limits,
        committed: Committed,
    ) -> Option<usize> {
        let mut media_bytes = 0;
        let mut seen_media = false;
        let mut last = None;
        for (i, e) in self.queue.iter().enumerate() {
            if e.class == Class::Key && seen_media {
                last = Some(i);
                if !over_with(
                    self.bytes - media_bytes,
                    Some(e.ts),
                    entry,
                    limits,
                    committed,
                ) {
                    return last;
                }
            }
            if e.class.is_media() {
                seen_media = true;
                media_bytes += e.bytes;
            }
        }
        if entry.class == Class::Key && seen_media {
            return Some(self.queue.len());
        }
        last
    }

    /// Drops the media entries in `range` whose class passes `filter`; returns how
    /// many.
    fn drop_media(
        &mut self,
        range: impl std::ops::RangeBounds<usize>,
        filter: impl Fn(Class) -> bool,
    ) -> u32 {
        let mut index = 0;
        let mut dropped = 0;
        let mut bytes = 0;
        self.queue.retain(|e| {
            let keep = !(range.contains(&index) && e.class.is_media() && filter(e.class));
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

/// Whether staging `entry` passes a limit, with `staged` bytes staged and the oldest
/// staged media chunk at `front`.
fn over_with<T>(
    staged: u64,
    front: Option<u64>,
    entry: &Entry<T>,
    limits: Limits,
    committed: Committed,
) -> bool {
    let held = staged + committed.bytes;
    // One item larger than the limit still goes into an empty queue.
    if held > 0 && held + entry.bytes > limits.max_bytes {
        return true;
    }
    let oldest = match (committed.oldest_ts, front) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    oldest.is_some_and(|oldest| entry.ts.saturating_sub(oldest) > limits.max_duration_ns)
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
    fn drops_whole_old_gops_first_and_keeps_headers() {
        let mut s = Staging::with_capacity(16);
        let none = Committed::default();
        // Header 0; GOP A: 1 key, 2 inter; GOP B: 3 key, 4 inter.
        for (n, c) in [
            (0, Class::Keep),
            (1, Class::Key),
            (2, Class::Inter),
            (3, Class::Key),
            (4, Class::Inter),
        ] {
            assert!(s.admit(entry(n, c, 10), limits(50), none).queued);
        }
        let r = s.admit(entry(5, Class::Inter, 10), limits(50), none);
        assert_eq!((r.queued, r.dropped, r.cut), (true, 2, false));
        assert_eq!(items(&s), vec![0, 3, 4, 5], "GOP A gone, header kept");
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
        assert_eq!(items(&s), vec![0], "keyframe kept, inter chunks dropped");
        assert!(s.is_refusing());
        // Even with room again, the GOP stays cut; headers still pass.
        let r = s.admit(entry(4, Class::Inter, 1), limits(1000), none);
        assert!(!r.queued && !r.cut);
        assert!(s.admit(entry(5, Class::Keep, 1), limits(1000), none).queued);
        assert!(s.admit(entry(6, Class::Key, 1), limits(1000), none).queued);
        assert!(
            s.admit(entry(7, Class::Inter, 1), limits(1000), none)
                .queued
        );
        assert_eq!(items(&s), vec![0, 5, 6, 7]);
    }

    #[test]
    fn incoming_keyframe_replaces_all_staged_media() {
        let mut s = Staging::with_capacity(16);
        let none = Committed::default();
        for (n, c) in [(0, Class::Inter), (1, Class::Keep), (2, Class::Inter)] {
            s.admit(entry(n, c, 10), limits(30), none);
        }
        let r = s.admit(entry(3, Class::Key, 10), limits(30), none);
        assert_eq!((r.queued, r.dropped, r.cut), (true, 2, false));
        assert_eq!(items(&s), vec![1, 3]);
    }

    #[test]
    fn headers_are_never_dropped_or_refused() {
        let mut s = Staging::with_capacity(16);
        let full = Committed {
            bytes: 100,
            oldest_ts: Some(0),
        };
        assert!(s.admit(entry(0, Class::Keep, 50), limits(10), full).queued);
        // A keyframe that cannot fit is refused and cuts its GOP.
        let r = s.admit(entry(1, Class::Key, 10), limits(10), full);
        assert_eq!((r.queued, r.cut, r.dropped), (false, true, 0));
        assert!(
            !s.admit(
                entry(2, Class::Inter, 1),
                limits(1000),
                Committed::default()
            )
            .queued
        );
    }

    #[test]
    fn a_keyframe_that_cannot_fit_does_not_cost_the_staged_gop() {
        let mut s = Staging::with_capacity(16);
        let none = Committed::default();
        for (n, c) in [(0, Class::Key), (1, Class::Inter)] {
            s.admit(entry(n, c, 10), limits(60), none);
        }
        // Committed chunks hold 50 of 60 bytes: a 20-byte keyframe can never fit.
        let busy = Committed {
            bytes: 50,
            oldest_ts: Some(0),
        };
        let r = s.admit(entry(2, Class::Key, 20), limits(60), busy);
        assert_eq!((r.queued, r.dropped, r.cut), (false, 0, true));
        assert_eq!(items(&s), vec![0, 1]);
    }

    #[test]
    fn duration_limit_ignores_headers_and_counts_committed_chunks() {
        let limits = Limits {
            max_bytes: u64::MAX,
            max_duration_ns: 25 * MS,
        };
        // A header staged at time 0 does not age the queue.
        let mut s = Staging::with_capacity(16);
        let none = Committed::default();
        assert!(s.admit(entry(0, Class::Keep, 1), limits, none).queued);
        for n in 10..=12 {
            let class = if n == 10 { Class::Key } else { Class::Inter };
            assert!(s.admit(entry(n, class, 1), limits, none).queued, "{n}");
        }
        assert!(!s.admit(entry(13, Class::Inter, 1), limits, none).queued);
        let mut s = Staging::with_capacity(16);
        let committed = Committed {
            bytes: 1,
            oldest_ts: Some(0),
        };
        assert!(s.admit(entry(1, Class::Key, 1), limits, committed).queued);
        assert!(s.admit(entry(2, Class::Inter, 1), limits, committed).queued);
        // 30 ms after the committed chunk: over, the current GOP is cut.
        let r = s.admit(entry(3, Class::Inter, 1), limits, committed);
        assert_eq!((r.queued, r.cut), (false, true));
    }

    /// Random streams through random limits: delivered media never continues a GOP
    /// that lost a chunk, and every header is delivered.
    #[test]
    fn delivered_chunks_never_miss_a_reference() {
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
                let class = match (n, rand() % 12) {
                    (0, _) | (_, 0) => Class::Keep,
                    (_, 1) => Class::Key,
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
            let headers = generated.iter().filter(|c| **c == Class::Keep).count();
            let delivered_headers = delivered
                .iter()
                .filter(|n| generated[**n as usize] == Class::Keep)
                .count();
            assert_eq!(headers, delivered_headers, "headers are never dropped");
            // Before each delivered inter chunk, every generated media chunk back to
            // the previous delivered media chunk must be absent.
            let mut previous: Option<u64> = None;
            for &n in &delivered {
                let class = generated[n as usize];
                if class == Class::Inter {
                    let lost = previous
                        .is_none_or(|p| (p + 1..n).any(|m| generated[m as usize].is_media()));
                    let starts_stream =
                        previous.is_none() && (0..n).all(|m| !generated[m as usize].is_media());
                    assert!(
                        !lost || starts_stream,
                        "inter chunk {n} follows lost media after {previous:?}"
                    );
                }
                if class.is_media() {
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
