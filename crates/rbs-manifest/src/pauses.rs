//! Pauses of a recording (ADR-0124 decision 10).
//!
//! A paused recording continues its files without a gap, so every time the manifest
//! stores is in *file time*: master time from `t0` with the pauses before it taken
//! out. A [`PauseSpans`] maps master time to file time; [`Pause`] is one
//! `[[pauses]]` row.

/// One pause of a recording (`[[pauses]]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pause {
    /// The join, where the files go on, in file time from `t0`.
    pub at_file_ns: u64,
    /// When the pause began, in master time from `t0`.
    pub from_ns: u64,
    /// When it ended, in master time from `t0`; the end of the recording for a
    /// recording stopped while paused.
    pub to_ns: u64,
    /// How far the sound of a file cut at its output (an encoder shared with a stream)
    /// may be off at the join, always on the same side; absent when the recording
    /// owned its encoders.
    pub join_error_ns: Option<u64>,
}

impl Pause {
    /// The time it left out.
    pub fn length_ns(&self) -> u64 {
        self.to_ns.saturating_sub(self.from_ns)
    }
}

/// The pauses of one running recording, in master time from `t0`, in order. The last
/// one may be open (no end yet).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PauseSpans {
    /// Beginning, end (none while open) and join error of each pause.
    spans: Vec<(u64, Option<u64>, Option<u64>)>,
}

impl PauseSpans {
    /// No pause.
    pub fn new() -> Self {
        Self::default()
    }

    /// A pause beginning at `from_ns` (from `t0`). Ignored while one is open or when
    /// it begins before the end of the last one.
    pub fn pause(&mut self, from_ns: u64) {
        self.pause_cut(from_ns, None);
    }

    /// A pause beginning at `from_ns` whose join may be off by up to
    /// `join_error_ns` (a cut at a shared output, ADR-0124 decision 5); `None` as
    /// [`PauseSpans::pause`].
    pub fn pause_cut(&mut self, from_ns: u64, join_error_ns: Option<u64>) {
        match self.spans.last() {
            Some((_, None, _)) => {}
            Some((_, Some(end), _)) if from_ns < *end => {}
            _ => self.spans.push((from_ns, None, join_error_ns)),
        }
    }

    /// Ends the open pause at `to_ns` (from `t0`). At or before its beginning the
    /// pause is cancelled and leaves no trace. Ignored when none is open.
    pub fn resume(&mut self, to_ns: u64) {
        let Some((from, end @ None, _)) = self.spans.last_mut() else {
            return;
        };
        if to_ns <= *from {
            self.spans.pop();
        } else {
            *end = Some(to_ns);
        }
    }

    /// Whether a pause is open.
    pub fn is_paused(&self) -> bool {
        matches!(self.spans.last(), Some((_, None, _)))
    }

    /// `t_ns` (master time from `t0`) in file time, and whether it lies inside a
    /// pause, in which case it is that pause's join.
    pub fn to_file(&self, t_ns: u64) -> (u64, bool) {
        let mut left_out = 0u64;
        for &(from, end, _) in &self.spans {
            if t_ns < from {
                break;
            }
            match end {
                Some(to) if t_ns >= to => left_out = left_out.saturating_add(to - from),
                _ => return (from.saturating_sub(left_out), true),
            }
        }
        (t_ns.saturating_sub(left_out), false)
    }

    /// The join, in file time, of the last pause that ended at or before `t_ns`
    /// (master time from `t0`); 0 when none did.
    pub fn last_join_before(&self, t_ns: u64) -> u64 {
        let mut left_out = 0u64;
        let mut join = 0u64;
        for &(from, end, _) in &self.spans {
            match end {
                Some(to) if to <= t_ns => {
                    join = from.saturating_sub(left_out);
                    left_out = left_out.saturating_add(to - from);
                }
                _ => break,
            }
        }
        join
    }

    /// The `[[pauses]]` rows, an open pause ending at `end_ns` (from `t0`).
    pub fn rows(&self, end_ns: u64) -> Vec<Pause> {
        let mut left_out = 0u64;
        let mut rows = Vec::with_capacity(self.spans.len());
        for &(from, end, join_error_ns) in &self.spans {
            let to = end.unwrap_or(end_ns).max(from);
            rows.push(Pause {
                at_file_ns: from.saturating_sub(left_out),
                from_ns: from,
                to_ns: to,
                join_error_ns,
            });
            left_out = left_out.saturating_add(to - from);
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shared_cut_keeps_its_join_error_in_its_row() {
        let mut p = PauseSpans::new();
        p.pause_cut(10, Some(21_333_333));
        p.resume(20);
        p.pause(30);
        let rows = p.rows(40);
        assert_eq!(rows[0].join_error_ns, Some(21_333_333));
        assert_eq!(rows[1].join_error_ns, None);
        assert_eq!(rows[1].at_file_ns, 20);
    }

    #[test]
    fn master_time_maps_to_file_time_around_two_pauses() {
        let mut p = PauseSpans::new();
        p.pause(10);
        p.resume(15);
        p.pause(20);
        assert!(p.is_paused());
        assert_eq!(p.to_file(5), (5, false));
        assert_eq!(p.to_file(12), (10, true));
        assert_eq!(p.to_file(15), (10, false));
        assert_eq!(p.to_file(18), (13, false));
        assert_eq!(p.to_file(25), (15, true));
        p.resume(30);
        assert_eq!(p.to_file(31), (16, false));
        assert_eq!(p.last_join_before(14), 0);
        assert_eq!(p.last_join_before(16), 10);
        assert_eq!(p.last_join_before(40), 15);
        let rows = p.rows(100);
        assert_eq!(
            rows.iter()
                .map(|r| (r.at_file_ns, r.from_ns, r.to_ns))
                .collect::<Vec<_>>(),
            [(10, 10, 15), (15, 20, 30)]
        );
    }

    #[test]
    fn a_resume_before_the_pause_began_cancels_it_and_an_open_pause_ends_at_stop() {
        let mut p = PauseSpans::new();
        p.pause(10);
        p.resume(10);
        assert!(!p.is_paused());
        assert!(p.rows(50).is_empty());
        p.pause(20);
        p.pause(25);
        assert_eq!(p.rows(50)[0].to_ns, 50);
        assert_eq!(p.rows(50).len(), 1);
    }
}
