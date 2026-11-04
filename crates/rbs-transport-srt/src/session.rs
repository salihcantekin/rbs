//! The SRT transport session: the producer side (the output worker's calls).

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rbs_core::media::{ChunkKind, StreamDescriptor};
use rbs_transport::{
    DropReason, QueueLimits, ReconnectPolicy, SendOutcome, Transport, TransportError,
    TransportEvent, TransportItem, TransportStats,
};

use crate::io::{Io, IoConfig, Item, Kind, Shared};
use crate::staging::{Admitted, Class, Committed, Entry, Limits, Staging};
use crate::url::SrtUrl;

/// Slots of the hand-off ring. Normal operation fills at most [`HANDOFF_ITEMS`];
/// `finish` uses the rest to hand over what is staged.
const RING_SLOTS: usize = 1024;
/// Chunks committed to the I/O thread at a time: about 100 ms of a TS stream whose
/// muxer emits a chunk per video frame and per audio frame. Chunks past this wait in
/// staging, where they can still be dropped.
const HANDOFF_ITEMS: usize = 8;
/// Initial staging capacity (items); it grows only if the limits allow more.
const STAGING_ITEMS: usize = 512;
/// Share of the `finish` timeout spent waiting for a clean end; the rest is the grace
/// period after the abort, so `finish` never blocks longer than its timeout.
const CLEAN_SHARE: u32 = 4; // of 5
/// How long `Drop` waits for the aborted thread before detaching it.
const DROP_GRACE: Duration = Duration::from_millis(200);

/// Session settings from the configuration.
pub(crate) struct Settings {
    pub url: SrtUrl,
    pub stream_id: Option<Vec<u8>>,
    pub latency_ms: u16,
    pub limits: QueueLimits,
    pub reconnect: ReconnectPolicy,
}

struct Running {
    producer: rtrb::Producer<Item>,
    shared: Arc<Shared>,
    thread: JoinHandle<()>,
    done: mpsc::Receiver<()>,
    staging: Staging<Item>,
    /// Media timestamp (none for headers and trailers) and size of each item pushed
    /// to the hand-off and not yet known to be done, oldest first.
    committed: VecDeque<(Option<u64>, u64)>,
    committed_bytes: u64,
    /// Items pushed to the hand-off since start.
    pushed: u64,
    /// Drops not yet reported as an event.
    unreported_drops: u32,
    keyframe_wanted: bool,
    items_dropped: u64,
    seen_connects: u32,
    seen_attempts: u32,
    reported_error: bool,
    returned_error: bool,
}

enum State {
    Created,
    Running(Box<Running>),
    /// The counters as they were when the thread ended.
    Finished(TransportStats),
}

pub(crate) struct SrtTransport {
    settings: Settings,
    state: State,
}

impl SrtTransport {
    pub(crate) fn new(settings: Settings) -> Self {
        Self {
            settings,
            state: State::Created,
        }
    }
}

impl Running {
    fn limits(&self, limits: QueueLimits) -> Limits {
        Limits {
            max_bytes: limits.max_bytes as u64,
            max_duration_ns: limits.max_duration.as_nanos().min(u128::from(u64::MAX)) as u64,
        }
    }

    /// Forgets committed items the I/O thread is done with.
    fn retire(&mut self) {
        let done = self.shared.progress.done_items.load(Ordering::Acquire);
        while self.pushed - (self.committed.len() as u64) < done {
            match self.committed.pop_front() {
                Some((_, bytes)) => self.committed_bytes -= bytes,
                None => break,
            }
        }
    }

    fn in_flight(&self) -> Committed {
        Committed {
            bytes: self.committed_bytes,
            // Headers and trailers have no media time; they are few.
            // While no connection takes them, committed chunks are discarded or skipped
            // to a keyframe when one comes (I/O thread): they must not make the
            // duration limit refuse newer media.
            oldest_ts: if self.shared.linked.load(Ordering::Relaxed) {
                self.committed.iter().find_map(|(ts, _)| *ts)
            } else {
                None
            },
        }
    }

    /// Moves staged items to the hand-off while a connection takes them. While none
    /// does (connecting, reconnecting), they stay staged, where the drop policy keeps
    /// the newest GOPs within the queue limits.
    fn pump_linked(&mut self) {
        if self.shared.linked.load(Ordering::Acquire) {
            self.pump(HANDOFF_ITEMS);
        }
    }

    /// Moves staged items to the hand-off while it has room (`limit` items in flight).
    fn pump(&mut self, limit: usize) -> bool {
        self.retire();
        if self.shared.exited.load(Ordering::Acquire) {
            // Nothing takes items any more; they stay staged and are counted as
            // dropped when the session ends.
            return false;
        }
        let mut moved = false;
        while self.committed.len() < limit && self.producer.slots() > 0 {
            let Some(entry) = self.staging.pop() else {
                break;
            };
            let ts = (entry.class != Class::Keep).then_some(entry.ts);
            let bytes = entry.bytes;
            if self.producer.push(entry.item).is_err() {
                // Unreachable: a slot was free and only this side pushes.
                break;
            }
            self.committed.push_back((ts, bytes));
            self.committed_bytes += bytes;
            self.pushed += 1;
            moved = true;
        }
        // No wake-up: the I/O thread never sleeps longer than one receive slice.
        moved
    }

    fn stats(&self) -> TransportStats {
        let progress = &self.shared.progress;
        let done = progress.done_items.load(Ordering::Acquire);
        let retired = (done + self.committed.len() as u64).saturating_sub(self.pushed) as usize;
        let in_flight: u64 = self.committed.iter().skip(retired).map(|(_, b)| b).sum();
        let mut stats = TransportStats::default();
        stats.bytes_sent = progress.bytes_sent.load(Ordering::Relaxed);
        stats.queued_bytes = self.staging.bytes() + in_flight;
        stats.items_dropped = self.items_dropped + progress.discarded.load(Ordering::Relaxed);
        stats.reconnects = progress.connects.load(Ordering::Relaxed).saturating_sub(1);
        stats.rtt = match progress.rtt_us.load(Ordering::Relaxed) {
            0 => None,
            us => Some(Duration::from_micros(u64::from(us))),
        };
        stats
    }

    /// Stops the thread and returns the final counters with the result. Without
    /// `abort`, four fifths of `budget` go to handing over what is staged and waiting
    /// for a clean end; then the thread is aborted and gets the rest. A thread still
    /// blocked in the OS after that (a connect or DNS lookup) is detached; it exits
    /// when that call returns.
    fn stop(
        mut self,
        budget: Duration,
        abort: bool,
    ) -> (Result<(), TransportError>, TransportStats) {
        let start = Instant::now();
        let deadline = later(start, budget);
        let mut ended = false;
        let mut timed_out = false;
        if abort {
            self.shared.abort();
        } else {
            let clean = later(start, budget / 5 * CLEAN_SHARE);
            // A connection that failed is not tried again: the thread ends now if it
            // waits for its next attempt, instead of holding `finish` to its budget.
            self.shared.ending.store(true, Ordering::SeqCst);
            self.thread.thread().unpark();
            // Hand over what is staged, then ask for a clean end.
            while !self.staging.is_empty() {
                self.pump(RING_SLOTS);
                if self.staging.is_empty() {
                    break;
                }
                let left = clean.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    timed_out = true;
                    break;
                }
                if self.done.recv_timeout(left.min(SLICE_WAIT)).is_ok() {
                    ended = true;
                    break;
                }
            }
            self.shared.stop.store(true, Ordering::SeqCst);
            self.thread.thread().unpark();
            if !ended && !timed_out {
                let left = clean.saturating_duration_since(Instant::now());
                match self.done.recv_timeout(left) {
                    Ok(()) => ended = true,
                    Err(_) => timed_out = true,
                }
            }
        }
        if !ended {
            self.shared.abort();
            self.thread.thread().unpark();
            let grace = deadline.saturating_duration_since(Instant::now());
            if self.done.recv_timeout(grace).is_err() {
                // The thread is detached: what it has not finished is lost, and the
                // final counters must not show it as still queued.
                self.staging_dropped();
                self.retire();
                let done = self.shared.progress.done_items.load(Ordering::Acquire);
                self.items_dropped += self.pushed.saturating_sub(done);
                self.committed.clear();
                self.committed_bytes = 0;
                return (Err(TransportError::Timeout), self.stats());
            }
        }
        self.staging_dropped();
        self.retire();
        // Items pushed while the thread was ending were never taken.
        let done = self.shared.progress.done_items.load(Ordering::Acquire);
        self.items_dropped += self.pushed.saturating_sub(done);
        self.committed.clear();
        self.committed_bytes = 0;
        let stats = self.stats();
        let error = self.shared.error.get().cloned();
        if self.thread.join().is_err() {
            return (
                Err(TransportError::Connection("I/O thread panicked".into())),
                stats,
            );
        }
        let result = match error {
            _ if timed_out => Err(TransportError::Timeout),
            Some(e) if !abort => Err(e),
            _ => Ok(()),
        };
        (result, stats)
    }

    /// Counts what is still staged as dropped (the session ends without it).
    fn staging_dropped(&mut self) {
        while self.staging.pop().is_some() {
            self.items_dropped += 1;
        }
    }
}

/// `start + d`, or a day later when that overflows (`finish(Duration::MAX)`).
fn later(start: Instant, d: Duration) -> Instant {
    start
        .checked_add(d)
        .unwrap_or(start + Duration::from_secs(86_400))
}

/// Longest single wait of `finish` while it hands staged items over.
const SLICE_WAIT: Duration = Duration::from_millis(5);

impl Transport for SrtTransport {
    fn start(&mut self, _streams: &[StreamDescriptor]) -> Result<(), TransportError> {
        match self.state {
            State::Created => {}
            State::Running(_) => return Err(TransportError::Rejected("already started".into())),
            State::Finished(_) => return Err(TransportError::Closed),
        }
        let (producer, consumer) = rtrb::RingBuffer::new(RING_SLOTS);
        let shared = Arc::new(Shared::default());
        let (done_tx, done) = mpsc::channel();
        let io = Io::new(
            IoConfig {
                url: self.settings.url.clone(),
                stream_id: self.settings.stream_id.clone(),
                latency_ms: self.settings.latency_ms,
                reconnect: self.settings.reconnect,
                stale_ns: self
                    .settings
                    .limits
                    .max_duration
                    .as_nanos()
                    .min(u128::from(u64::MAX)) as u64,
            },
            Arc::clone(&shared),
            consumer,
        );
        let thread = thread::Builder::new()
            .name("rbs-srt-transport".into())
            .spawn(move || io.run(done_tx))
            .map_err(|e| TransportError::Connection(format!("cannot start the I/O thread: {e}")))?;
        self.state = State::Running(Box::new(Running {
            producer,
            shared,
            thread,
            done,
            staging: Staging::with_capacity(STAGING_ITEMS),
            committed: VecDeque::with_capacity(RING_SLOTS),
            committed_bytes: 0,
            pushed: 0,
            unreported_drops: 0,
            keyframe_wanted: false,
            items_dropped: 0,
            seen_connects: 0,
            seen_attempts: 0,
            reported_error: false,
            returned_error: false,
        }));
        Ok(())
    }

    fn send(&mut self, item: TransportItem<'_>) -> Result<SendOutcome, TransportError> {
        let running = match &mut self.state {
            State::Created => return Err(TransportError::NotStarted),
            State::Finished(_) => return Err(TransportError::Closed),
            State::Running(running) => running,
        };
        let TransportItem::Chunk(chunk) = item else {
            return Err(TransportError::InvalidInput(
                "the SRT transport takes MPEG-TS chunks".into(),
            ));
        };
        if let Some(e) = running.shared.error.get() {
            if running.returned_error {
                return Err(TransportError::Closed);
            }
            running.returned_error = true;
            return Err(e.clone());
        }
        let (kind, class, ts) = match chunk.kind {
            ChunkKind::Header => (Kind::Header, Class::Keep, 0),
            ChunkKind::Trailer => (Kind::Trailer, Class::Keep, 0),
            ChunkKind::Media {
                keyframe: true,
                timestamp,
            } => (Kind::Key, Class::Key, timestamp.as_nanos()),
            ChunkKind::Media { timestamp, .. } => (Kind::Inter, Class::Inter, timestamp.as_nanos()),
            // A live stream cannot be rewritten; the output stays playable without
            // patches (media types spec).
            ChunkKind::Patch { .. } => return Ok(SendOutcome::Queued),
            _ => {
                return Err(TransportError::InvalidInput(
                    "a chunk kind the SRT transport does not know".into(),
                ));
            }
        };
        let entry = Entry {
            class,
            ts,
            bytes: chunk.data.len() as u64,
            item: Item {
                data: chunk.data.clone(),
                kind,
                at: Instant::now(),
                ts,
            },
        };
        if matches!(kind, Kind::Key | Kind::Inter) {
            // Before the chunk can reach the hand-off (I/O thread: stale check at the
            // start of a connection).
            running.shared.newest.0.store(ts, Ordering::Relaxed);
        }
        running.retire();
        let limits = running.limits(self.settings.limits);
        let Admitted {
            queued,
            dropped,
            cut,
        } = running.staging.admit(entry, limits, running.in_flight());
        let lost = dropped + u32::from(!queued);
        running.items_dropped += u64::from(lost);
        running.unreported_drops = running.unreported_drops.saturating_add(lost);
        running.keyframe_wanted |= cut;
        running.pump_linked();
        Ok(match (queued, dropped) {
            (false, _) => SendOutcome::Dropped {
                reason: DropReason::QueueFull,
            },
            (true, 0) => SendOutcome::Queued,
            (true, dropped) => SendOutcome::QueuedAfterDrops {
                dropped,
                reason: DropReason::QueueFull,
            },
        })
    }

    fn poll_events(&mut self, out: &mut Vec<TransportEvent>) {
        let State::Running(running) = &mut self.state else {
            return;
        };
        running.pump_linked();
        let progress = &running.shared.progress;
        let attempts = progress.attempts.load(Ordering::Acquire);
        if attempts != running.seen_attempts {
            running.seen_attempts = attempts;
            out.push(TransportEvent::Reconnecting {
                attempt: progress.attempt.load(Ordering::Relaxed),
            });
        }
        let connects = progress.connects.load(Ordering::Acquire);
        if connects != running.seen_connects {
            running.seen_connects = connects;
            out.push(TransportEvent::Connected);
        }
        if running.shared.keyframe_wanted.swap(false, Ordering::AcqRel) {
            running.keyframe_wanted = true;
        }
        if std::mem::take(&mut running.keyframe_wanted) {
            out.push(TransportEvent::KeyframeRequested);
        }
        let drops = std::mem::take(&mut running.unreported_drops);
        if drops > 0 {
            out.push(TransportEvent::Dropped {
                items: drops,
                reason: DropReason::QueueFull,
            });
        }
        if !running.reported_error {
            if let Some(e) = running.shared.error.get() {
                running.reported_error = true;
                out.push(TransportEvent::Disconnected { error: e.clone() });
            }
        }
    }

    fn stats(&self) -> TransportStats {
        match &self.state {
            State::Created => TransportStats::default(),
            State::Running(running) => running.stats(),
            State::Finished(stats) => *stats,
        }
    }

    fn finish(&mut self, timeout: Duration) -> Result<(), TransportError> {
        let state = std::mem::replace(&mut self.state, State::Finished(TransportStats::default()));
        match state {
            State::Running(running) => {
                let (result, stats) = running.stop(timeout, false);
                self.state = State::Finished(stats);
                result
            }
            State::Created => Ok(()),
            State::Finished(stats) => {
                self.state = State::Finished(stats);
                Err(TransportError::Closed)
            }
        }
    }
}

impl Drop for SrtTransport {
    fn drop(&mut self) {
        let state = std::mem::replace(&mut self.state, State::Finished(TransportStats::default()));
        if let State::Running(running) = state {
            let _ = running.stop(DROP_GRACE, true);
        }
    }
}
