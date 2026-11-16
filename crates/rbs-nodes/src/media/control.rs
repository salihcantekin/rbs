//! The playback control handle of a media source (ADR-0108 decision 10).
//!
//! The control path writes records (play, pause, seek, restart); the worker and both
//! nodes read them. A record names a generation, the master time from which it
//! applies, a `paused` flag and, for a seek, a position. The last [`RECORDS`] records
//! sit in a fixed array under a sequence counter: a writer makes the counter odd,
//! writes, and makes it even again. A real-time reader never retries: on a torn read
//! it keeps what it read last time.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};
use std::time::Duration;

use rbs_core::time::Timestamp;

use super::Shared;
use super::picture::{MediaNodeError, MediaPhase};

/// Records kept in the handle.
pub const RECORDS: usize = 4;
/// The instant's margin before the picture node has reported a render period.
const DEFAULT_RENDER_PERIOD: Duration = Duration::from_millis(50);

/// The play state the handle publishes (ADR-0108 decision 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlayState {
    /// Pictures and sound advance with the master clock.
    Playing,
    /// Stopped at the cursor: the picture holds, the sound is silence.
    Paused,
    /// The end of the file without `loop`: the last picture holds, no worker.
    Ended,
    /// Asleep with no stored pause or end.
    Standby,
    /// A fault stopped playback.
    Unavailable,
}

/// One command as the nodes and the worker see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) generation: u32,
    pub(crate) instant: Timestamp,
    pub(crate) paused: bool,
    pub(crate) position: Option<Duration>,
}

/// What a node does with an item of one generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Its own stamp decides.
    Play,
    /// A later command ended its generation before its stamp: skip and return it.
    Skip,
    /// Its generation is newer than every record read: leave it for a later read.
    Wait,
}

/// A copy of the records a node read, in generation order. Plain data: copying it
/// does not allocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Records {
    list: [Option<Record>; RECORDS],
}

impl Records {
    const EMPTY: Self = Self {
        list: [None; RECORDS],
    };

    fn get(&self, generation: u32) -> Option<&Record> {
        self.list
            .iter()
            .flatten()
            .find(|r| r.generation == generation)
    }

    pub(crate) fn newest(&self) -> Option<&Record> {
        self.list.iter().flatten().max_by_key(|r| r.generation)
    }

    fn oldest(&self) -> Option<u32> {
        self.list.iter().flatten().map(|r| r.generation).min()
    }

    /// The verdict for an item of `generation` stamped at `stamp`.
    pub(crate) fn verdict(&self, generation: u32, stamp: Timestamp) -> Verdict {
        if self.get(generation).is_some() {
            return match self.get(generation.wrapping_add(1)) {
                Some(next) if stamp >= next.instant => Verdict::Skip,
                _ => Verdict::Play,
            };
        }
        match self.oldest() {
            Some(oldest) if generation < oldest => Verdict::Skip,
            _ => Verdict::Wait,
        }
    }

    /// The record that applies at `t`: the newest one whose instant is at or before
    /// `t`.
    pub(crate) fn at(&self, t: Timestamp) -> Option<&Record> {
        self.list
            .iter()
            .flatten()
            .filter(|r| r.instant <= t)
            .max_by_key(|r| r.generation)
    }
}

struct Slot {
    generation: AtomicU32,
    /// Nanoseconds plus one; 0 is an empty slot.
    instant: AtomicU64,
    paused: AtomicBool,
    /// Nanoseconds plus one; 0 is no position.
    position: AtomicU64,
}

impl Slot {
    fn new() -> Self {
        Self {
            generation: AtomicU32::new(0),
            instant: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            position: AtomicU64::new(0),
        }
    }
}

/// The records of one source, inside [`Shared`]. On cache lines of their own: the
/// worker's and the nodes' frequent writes elsewhere in `Shared` do not evict them
/// from the readers' caches.
#[repr(align(64))]
pub(crate) struct Controls {
    seq: AtomicU64,
    slots: [Slot; RECORDS],
    /// The picture node's last render period, in nanoseconds; 0 before it is known.
    pub(crate) render_period: AtomicU64,
}

impl Controls {
    pub(crate) fn new() -> Self {
        let controls = Self {
            seq: AtomicU64::new(0),
            slots: std::array::from_fn(|_| Slot::new()),
            render_period: AtomicU64::new(0),
        };
        // Generation 0: playing from the start of time.
        controls.write(Record {
            generation: 0,
            instant: Timestamp::ZERO,
            paused: false,
            position: None,
        });
        controls
    }

    /// Writes `record` over the oldest slot. Control path only, one writer at a time
    /// (the player lock).
    pub(crate) fn write(&self, record: Record) {
        let slot = &self.slots[record.generation as usize % RECORDS];
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        slot.generation.store(record.generation, Ordering::Relaxed);
        slot.instant.store(
            record.instant.as_nanos().saturating_add(1),
            Ordering::Relaxed,
        );
        slot.paused.store(record.paused, Ordering::Relaxed);
        slot.position.store(
            record.position.map_or(0, |p| {
                u64::try_from(p.as_nanos()).unwrap_or(u64::MAX - 1) + 1
            }),
            Ordering::Relaxed,
        );
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }

    /// Reads the records once. `None` on a torn read (a write was in progress).
    /// Never blocks, never retries.
    pub(crate) fn try_read(&self) -> Option<Records> {
        let before = self.seq.load(Ordering::Acquire);
        if before % 2 == 1 {
            return None;
        }
        let mut records = Records::EMPTY;
        for (out, slot) in records.list.iter_mut().zip(&self.slots) {
            let instant = slot.instant.load(Ordering::Relaxed);
            if instant == 0 {
                continue;
            }
            let position = slot.position.load(Ordering::Relaxed);
            *out = Some(Record {
                generation: slot.generation.load(Ordering::Relaxed),
                instant: Timestamp::from_nanos(instant - 1),
                paused: slot.paused.load(Ordering::Relaxed),
                position: (position != 0).then(|| Duration::from_nanos(position - 1)),
            });
        }
        fence(Ordering::Acquire);
        (self.seq.load(Ordering::Relaxed) == before).then_some(records)
    }

    /// Reads the records, retrying a torn read. Worker and control path only.
    pub(crate) fn read(&self) -> Records {
        loop {
            if let Some(records) = self.try_read() {
                return records;
            }
            std::hint::spin_loop();
        }
    }

    /// The newest record. Worker and control path only.
    pub(crate) fn newest(&self) -> Record {
        self.read().newest().copied().unwrap_or(Record {
            generation: 0,
            instant: Timestamp::ZERO,
            paused: false,
            position: None,
        })
    }
}

/// A node's view of the records: the last good read.
pub(crate) struct View {
    records: Records,
}

impl View {
    pub(crate) fn new() -> Self {
        Self {
            records: Records::EMPTY,
        }
    }

    /// Reads again; keeps the previous records on a torn read and says so.
    pub(crate) fn refresh(&mut self, controls: &Controls) -> bool {
        match controls.try_read() {
            Some(records) => {
                self.records = records;
                true
            }
            None => false,
        }
    }

    pub(crate) fn records(&self) -> &Records {
        &self.records
    }
}

/// Play, pause, seek, and restart for one media source (ADR-0108 decision 10).
///
/// Every method runs on the control path. A command never writes the document, is
/// not an undo step, and does not replace the source.
#[derive(Clone)]
pub struct MediaControl {
    shared: Arc<Shared>,
}

impl MediaControl {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }

    /// Plays from the cursor. From the end hold it restarts. Does nothing while
    /// playing.
    pub fn play(&self) -> Result<(), MediaNodeError> {
        match self.state() {
            PlayState::Playing => Ok(()),
            PlayState::Ended => self.restart(),
            _ => {
                self.command(false, None);
                Ok(())
            }
        }
    }

    /// Stops at the media time of the command's instant. Asleep, the source wakes
    /// paused. Does nothing when paused or ended.
    pub fn pause(&self) {
        if matches!(self.state(), PlayState::Playing | PlayState::Standby) {
            self.command(true, None);
        }
    }

    /// Moves the cursor to `position` (clamped to the stated duration). While paused,
    /// or from the end hold, the picture at the new position is shown and held.
    pub fn seek(&self, position: Duration) -> Result<(), MediaNodeError> {
        let position = match self.duration() {
            Some(duration) => position.min(duration),
            None => position,
        };
        let state = self.state();
        let paused = matches!(state, PlayState::Paused | PlayState::Ended)
            || self.shared.controls.newest().paused;
        if state == PlayState::Ended {
            return self.start_at(position, true);
        }
        self.command(paused, Some(position));
        Ok(())
    }

    /// Plays from the start of the range (0 until the play range is built).
    pub fn restart(&self) -> Result<(), MediaNodeError> {
        if self.state() == PlayState::Ended {
            return self.start_at(Duration::ZERO, false);
        }
        self.command(false, Some(Duration::ZERO));
        Ok(())
    }

    /// The play state now.
    pub fn state(&self) -> PlayState {
        let player = self.shared.player();
        let paused = self.shared.controls.newest().paused;
        if player.progress.failed.load(Ordering::Acquire) {
            return PlayState::Unavailable;
        }
        match player.phase {
            MediaPhase::Standby if paused => PlayState::Paused,
            MediaPhase::Standby => PlayState::Standby,
            MediaPhase::Hold => PlayState::Ended,
            MediaPhase::Playing if player.ended.load(Ordering::Acquire) => PlayState::Ended,
            MediaPhase::Playing if paused => PlayState::Paused,
            MediaPhase::Playing => PlayState::Playing,
        }
    }

    /// The media cursor: the picture shown last, or where a pause stopped.
    pub fn cursor(&self) -> Duration {
        self.shared.cursor()
    }

    /// The file's stated duration, once a worker has opened it.
    pub fn duration(&self) -> Option<Duration> {
        match self.shared.duration.load(Ordering::Acquire) {
            0 => None,
            n => Some(Duration::from_nanos(n - 1)),
        }
    }

    /// Writes one record whose instant is now plus the render period, on the audio
    /// block grid, and wakes the worker.
    fn command(&self, paused: bool, position: Option<Duration>) {
        let player = self.shared.player();
        let instant = self.instant(player.clock().now());
        let generation = self.shared.controls.newest().generation.wrapping_add(1);
        if let Some(position) = position {
            // Asleep, the next start begins here.
            if player.phase == MediaPhase::Standby {
                self.shared.set_cursor(position);
            }
        }
        self.shared.controls.write(Record {
            generation,
            instant,
            paused,
            position,
        });
        player.unpark();
    }

    /// Starts a worker at `position` after the end hold, paused or playing.
    fn start_at(&self, position: Duration, paused: bool) -> Result<(), MediaNodeError> {
        let shared = Arc::clone(&self.shared);
        let mut player = shared.player();
        let instant = self.instant(player.clock().now());
        let generation = shared.controls.newest().generation.wrapping_add(1);
        shared.controls.write(Record {
            generation,
            instant,
            paused,
            position: None,
        });
        shared.set_cursor(position);
        player.start(&shared)
    }

    fn instant(&self, now: Timestamp) -> Timestamp {
        let period = match self.shared.controls.render_period.load(Ordering::Relaxed) {
            0 => DEFAULT_RENDER_PERIOD,
            n => Duration::from_nanos(n),
        };
        let t = now + period;
        let Some(grid) = self.shared.grid() else {
            return t;
        };
        let block = self.shared.block_period.as_nanos().max(1);
        let (t, g) = (u128::from(t.as_nanos()), u128::from(grid.as_nanos()));
        if t <= g {
            return Timestamp::from_nanos(u64::try_from(t).unwrap_or(u64::MAX));
        }
        let up = (t - g).div_ceil(block) * block + g;
        Timestamp::from_nanos(u64::try_from(up).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(generation: u32, instant_ms: u64, paused: bool) -> Record {
        Record {
            generation,
            instant: Timestamp::from_nanos(instant_ms * 1_000_000),
            paused,
            position: None,
        }
    }

    fn ms(n: u64) -> Timestamp {
        Timestamp::from_nanos(n * 1_000_000)
    }

    #[test]
    fn an_item_plays_until_the_next_records_instant() {
        let controls = Controls::new();
        controls.write(rec(1, 100, true));
        let records = controls.try_read().unwrap_or(Records::EMPTY);
        assert_eq!(records.verdict(0, ms(99)), Verdict::Play);
        assert_eq!(records.verdict(0, ms(100)), Verdict::Skip);
        assert_eq!(records.verdict(1, ms(500)), Verdict::Play);
        assert_eq!(records.verdict(2, ms(0)), Verdict::Wait);
        assert!(records.at(ms(99)).is_some_and(|r| !r.paused));
        assert!(records.at(ms(100)).is_some_and(|r| r.paused));
    }

    #[test]
    fn a_generation_that_left_the_array_is_skipped() {
        let controls = Controls::new();
        for g in 1..=5 {
            controls.write(rec(g, 10 * u64::from(g), false));
        }
        let records = controls.try_read().unwrap_or(Records::EMPTY);
        assert_eq!(records.verdict(0, ms(0)), Verdict::Skip);
        assert_eq!(records.verdict(1, ms(0)), Verdict::Skip);
        assert_eq!(records.verdict(2, ms(0)), Verdict::Play);
        assert_eq!(records.newest().map(|r| r.generation), Some(5));
    }

    #[test]
    fn a_read_during_a_write_is_torn_and_the_view_keeps_the_last_records() {
        let controls = Controls::new();
        let mut view = View::new();
        assert!(view.refresh(&controls));
        let before = *view.records();
        // A write in progress: the counter is odd.
        controls.seq.fetch_add(1, Ordering::Relaxed);
        assert!(!view.refresh(&controls));
        assert_eq!(*view.records(), before);
        controls.seq.fetch_add(1, Ordering::Relaxed);
        assert!(view.refresh(&controls));
    }
}
