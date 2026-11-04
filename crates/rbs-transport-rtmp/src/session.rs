//! The RTMP transport session: the producer side (the output worker's calls).

use std::collections::VecDeque;
use std::sync::atomic::{Ordering, fence};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rbs_core::media::StreamDescriptor;
use rbs_core::time::Timestamp;
use rbs_media_format::flv::{self, FlvTracks, TrackKind};
use rbs_transport::{
    DropReason, QueueLimits, ReconnectPolicy, SendOutcome, Transport, TransportError,
    TransportEvent, TransportItem, TransportStats,
};

use crate::io::{Io, IoConfig, Item, Shared};
use crate::staging::{Admitted, Class, Committed, Entry, Limits, Staging};
use crate::url::RtmpUrl;

/// Slots of the hand-off ring. Normal operation fills at most [`HANDOFF_ITEMS`];
/// `finish` uses the rest to hand over what is staged.
const RING_SLOTS: usize = 1024;
/// Items committed to the I/O thread at a time: about 100 ms of 30 fps video with
/// 48 kHz AAC. Items past this wait in staging, where they can still be dropped.
const HANDOFF_ITEMS: usize = 8;
/// Initial staging capacity (items); it grows only if the limits allow more.
const STAGING_ITEMS: usize = 512;
/// How far before the first packet's decode time the RTMP clock starts, so packets
/// of other streams that arrive later with earlier timestamps (audio before video,
/// B-frame decode times) still have non-negative timestamps.
pub(crate) const ORIGIN_LEAD: Duration = Duration::from_secs(1);
/// Share of the `finish` timeout spent waiting for a clean end; the rest is the grace
/// period after the abort, so `finish` never blocks longer than its timeout.
const CLEAN_SHARE: u32 = 4; // of 5
/// How long `Drop` waits for the aborted thread before detaching it.
const DROP_GRACE: Duration = Duration::from_millis(200);

/// Session settings from the configuration.
pub(crate) struct Settings {
    pub url: RtmpUrl,
    pub key: String,
    pub limits: QueueLimits,
    pub reconnect: ReconnectPolicy,
}

struct Running {
    producer: rtrb::Producer<Item>,
    shared: Arc<Shared>,
    thread: JoinHandle<()>,
    done: mpsc::Receiver<()>,
    tracks: FlvTracks,
    origin: Option<Timestamp>,
    staging: Staging<Item>,
    /// Timestamp and size of each item pushed to the hand-off and not yet known to be
    /// done, oldest first.
    committed: VecDeque<(u64, u64)>,
    committed_bytes: u64,
    /// Items pushed to the hand-off since start.
    pushed: u64,
    /// Drops not yet reported as an event.
    unreported_drops: u32,
    keyframe_wanted: bool,
    items_dropped: u64,
    seen_publishes: u32,
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

pub(crate) struct RtmpTransport {
    settings: Settings,
    state: State,
}

impl RtmpTransport {
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
            // While no connection takes them, committed items are discarded or skipped
            // to a keyframe when one comes (I/O thread): they must not make the
            // duration limit refuse newer media.
            oldest_ts: self
                .shared
                .linked
                .load(Ordering::Relaxed)
                .then(|| self.committed.front().map(|(ts, _)| *ts))
                .flatten(),
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
            let (ts, bytes) = (entry.ts, entry.bytes);
            if self.producer.push(entry.item).is_err() {
                // Unreachable: a slot was free and only this side pushes.
                break;
            }
            self.committed.push_back((ts, bytes));
            self.committed_bytes += bytes;
            self.pushed += 1;
            moved = true;
        }
        if moved {
            self.wake();
        }
        moved
    }

    fn wake(&self) {
        // Pairs with the fence in the I/O loop: orders the push before the load of
        // `sleeping`.
        fence(Ordering::SeqCst);
        if self.shared.progress.sleeping.load(Ordering::SeqCst) {
            self.thread.thread().unpark();
        }
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
        stats.reconnects = progress.publishes.load(Ordering::Relaxed).saturating_sub(1);
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
                // The thread is detached (inside DNS or `connect`): what it has not
                // finished is lost, and the final counters must not show it as still
                // queued.
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

fn class(kind: TrackKind, keyframe: bool) -> Class {
    match (kind, keyframe) {
        (TrackKind::Video, true) => Class::Key,
        (TrackKind::Video, false) => Class::Inter,
        (TrackKind::Audio, _) => Class::Audio,
    }
}

impl Transport for RtmpTransport {
    fn start(&mut self, streams: &[StreamDescriptor]) -> Result<(), TransportError> {
        match self.state {
            State::Created => {}
            State::Running(_) => return Err(TransportError::Rejected("already started".into())),
            State::Finished(_) => return Err(TransportError::Closed),
        }
        let tracks = FlvTracks::new(streams)
            .map_err(|e| TransportError::InvalidInput(format!("streams: {e}")))?;
        let (producer, consumer) = rtrb::RingBuffer::new(RING_SLOTS);
        let shared = Arc::new(Shared::default());
        let (done_tx, done) = mpsc::channel();
        let io = Io::new(
            IoConfig {
                url: self.settings.url.clone(),
                key: self.settings.key.clone(),
                reconnect: self.settings.reconnect,
                tracks: tracks.clone(),
                stale_ms: self
                    .settings
                    .limits
                    .max_duration
                    .as_millis()
                    .min(i32::MAX as u128) as i32,
            },
            Arc::clone(&shared),
            consumer,
        );
        let thread = thread::Builder::new()
            .name("rbs-rtmp-transport".into())
            .spawn(move || io.run(done_tx))
            .map_err(|e| TransportError::Connection(format!("cannot start the I/O thread: {e}")))?;
        self.state = State::Running(Box::new(Running {
            producer,
            shared,
            thread,
            done,
            tracks,
            origin: None,
            staging: Staging::with_capacity(STAGING_ITEMS),
            committed: VecDeque::with_capacity(RING_SLOTS),
            committed_bytes: 0,
            pushed: 0,
            unreported_drops: 0,
            keyframe_wanted: false,
            items_dropped: 0,
            seen_publishes: 0,
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
        let TransportItem::Packet { stream, packet } = item else {
            return Err(TransportError::InvalidInput(
                "the RTMP transport takes encoded packets".into(),
            ));
        };
        if let Some(e) = running.shared.error.get() {
            if running.returned_error {
                return Err(TransportError::Closed);
            }
            running.returned_error = true;
            return Err(e.clone());
        }
        let plan = running
            .tracks
            .plan(usize::from(stream.0), packet)
            .map_err(|e| TransportError::InvalidInput(e.to_string()))?;
        let dts = packet.dts();
        let origin = *running.origin.get_or_insert_with(|| {
            Timestamp::from_nanos(dts.as_nanos().saturating_sub(ORIGIN_LEAD.as_nanos() as u64))
        });
        if dts < origin {
            return Err(TransportError::InvalidInput(
                "packet decode time before the stream's start".into(),
            ));
        }
        let dts_ms = flv::millis_since(dts, origin);
        let cts = flv::millis_since(packet.pts(), origin) - dts_ms;
        let entry = Entry {
            class: class(plan.kind, packet.keyframe()),
            ts: dts.as_nanos(),
            bytes: plan.data_size as u64,
            item: Item {
                packet: packet.clone(),
                plan,
                // RTMP timestamps are 32-bit milliseconds and wrap.
                ms: dts_ms as u32,
                cts,
            },
        };
        // Before the item can reach the hand-off, so the I/O thread never sees an item
        // newer than this (up to the interleaving of the streams).
        running
            .shared
            .newest
            .0
            .store(dts_ms as u32, Ordering::Relaxed);
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
        let publishes = progress.publishes.load(Ordering::Acquire);
        if publishes != running.seen_publishes {
            running.seen_publishes = publishes;
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

impl Drop for RtmpTransport {
    fn drop(&mut self) {
        let state = std::mem::replace(&mut self.state, State::Finished(TransportStats::default()));
        if let State::Running(running) = state {
            let _ = running.stop(DROP_GRACE, true);
        }
    }
}
