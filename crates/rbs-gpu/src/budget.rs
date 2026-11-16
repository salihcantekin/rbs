//! A per-partition limit on the bytes source nodes upload in one tick, with a fairness
//! gate (ADR-0129 decision 5, spec `gpu-compositor.md`, "Upload budget").
//!
//! The state is tick-stamped and rolled over lazily by the first node that asks in a
//! new tick, so `end_tick` does nothing for it and a partition with no such node does
//! no work (P13). Fixed-size scalars only: no allocation.

/// Bytes the nodes of one partition may upload in one tick beyond the first upload.
pub const UPLOAD_BUDGET_BYTES: u64 = 16 * 1024 * 1024;

/// Upload budget state of one render partition ([`crate::node::RenderLocal`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UploadBudget {
    /// The tick this state is for.
    stamp: Option<u64>,
    /// Bytes uploaded in that tick.
    bytes: u64,
    /// Uploads in that tick.
    uploads: u32,
    /// While set, only nodes that first waited on this tick may upload.
    gate: Option<u64>,
    /// The earliest first-waited tick reported during the stamped tick.
    next_gate: Option<u64>,
}

impl UploadBudget {
    fn roll(&mut self, tick: u64) {
        if self.stamp != Some(tick) {
            self.gate = self.next_gate.take();
            self.bytes = 0;
            self.uploads = 0;
            self.stamp = Some(tick);
        }
    }

    /// Asks to upload `bytes` in tick `tick`. `waiting_since` is the tick the node first
    /// waited on for this picture, if it was deferred before. Returns whether the node
    /// may upload now; when it may not, the node keeps its picture, remembers the tick it
    /// first waited on and asks again next tick.
    ///
    /// The first upload of a tick is allowed whatever its size; further uploads fit in
    /// [`UPLOAD_BUDGET_BYTES`]. While some node has been waiting, only the nodes that
    /// first waited on the earliest such tick may upload, the first upload included, so
    /// a picture waits at most about `ceil(waiting bytes / budget) + 1` ticks.
    pub fn try_take(&mut self, tick: u64, bytes: u64, waiting_since: Option<u64>) -> bool {
        self.roll(tick);
        let in_group = self.gate.is_none_or(|g| waiting_since == Some(g));
        let fits = self.uploads == 0 || self.bytes.saturating_add(bytes) <= UPLOAD_BUDGET_BYTES;
        if in_group && fits {
            self.bytes = self.bytes.saturating_add(bytes);
            self.uploads += 1;
            return true;
        }
        let first = waiting_since.unwrap_or(tick);
        self.next_gate = Some(self.next_gate.map_or(first, |g| g.min(first)));
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn the_first_upload_of_a_tick_is_always_allowed() {
        let mut b = UploadBudget::default();
        assert!(b.try_take(1, 32 * MIB, None));
        assert!(!b.try_take(1, MIB, None));
        assert!(b.try_take(2, 32 * MIB, Some(1)));
    }

    #[test]
    fn further_uploads_fit_in_the_budget() {
        let mut b = UploadBudget::default();
        assert!(b.try_take(1, 8 * MIB, None));
        assert!(b.try_take(1, 8 * MIB, None));
        assert!(!b.try_take(1, MIB, None));
    }

    /// Twenty 1080p clocks change on the same tick: all upload within the bound.
    #[test]
    fn twenty_clocks_finish_within_the_bound() {
        let size = 1920 * 1080 * 4;
        let mut waiting: Vec<Option<u64>> = vec![None; 20];
        let mut done = [false; 20];
        let mut tick = 10;
        while done.iter().any(|d| !d) {
            for (i, d) in done.iter_mut().enumerate() {
                if !*d && b_take(&mut waiting, i, tick, size) {
                    *d = true;
                }
            }
            tick += 1;
            assert!(tick < 10 + 20, "not done by tick {tick}");
        }
        // 20 x 7.9 MiB = 158 MiB; 16 MiB per tick plus one: about 10 ticks.
        assert!(tick - 10 <= 12, "took {} ticks", tick - 10);

        fn b_take(waiting: &mut [Option<u64>], i: usize, tick: u64, size: u64) -> bool {
            thread_local!(static B: std::cell::Cell<UploadBudget> = std::cell::Cell::new(UploadBudget::default()));
            B.with(|b| {
                let mut v = b.get();
                let ok = v.try_take(tick, size, waiting[i]);
                b.set(v);
                if !ok && waiting[i].is_none() {
                    waiting[i] = Some(tick);
                }
                ok
            })
        }
    }

    /// A node late in schedule order is not starved by earlier nodes that change every
    /// tick.
    #[test]
    fn a_late_node_is_not_starved() {
        let mut b = UploadBudget::default();
        let big = 12 * MIB;
        let mut late_waiting = None;
        let mut uploaded_at = None;
        for tick in 1..20 {
            // Two early nodes with a new picture every tick.
            b.try_take(tick, big, None);
            b.try_take(tick, big, None);
            if b.try_take(tick, big, late_waiting) {
                uploaded_at = Some(tick);
                break;
            }
            late_waiting.get_or_insert(tick);
        }
        assert_eq!(uploaded_at, Some(2));
    }

    /// A gate whose nodes left the schedule shuts uploads for one tick at most.
    #[test]
    fn a_gate_of_removed_nodes_clears_on_the_next_roll() {
        let mut b = UploadBudget::default();
        assert!(b.try_take(1, 20 * MIB, None));
        assert!(!b.try_take(1, 20 * MIB, None)); // waits from tick 1, then is removed
        assert!(!b.try_take(2, MIB, None)); // tick 2: gate is tick 1, this node waits
        assert!(b.try_take(3, MIB, Some(2))); // tick 3: gate is the waiting node's tick
    }
}
