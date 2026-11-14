//! The system probe port and its worker thread (ADR-0090 decisions 1 and 11).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use rbs_core::time::Timestamp;

/// How often free space and memory are read while a guard runs.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// How long a probe call may take before disk and memory count as unknown. A network
/// drive can hang for much longer; the worker keeps waiting, the guard does not.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// One volume (file system) as the probe names it: outputs whose folders have the same
/// key share free space. Opaque; only compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VolumeKey(pub u64);

/// A folder's volume and the bytes the current user can still write there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeSpace {
    /// The folder's volume.
    pub volume: VolumeKey,
    /// Bytes available to the current user.
    pub free_bytes: u64,
}

/// A reading that may be missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probed<T> {
    /// The value read.
    Value(T),
    /// The reading failed or took too long this time.
    Unknown,
    /// This platform has no reading for it (never a guess, ADR-0090 decision 11).
    Unsupported,
}

/// Reads free space and memory from the operating system. Implemented by the
/// `rbs-system-probe` adapter crate and wired in by a composition root; tests use a
/// fake. Calls may block (a network drive): they run on [`ProbeWorker`]'s thread only.
pub trait SystemProbe: Send + 'static {
    /// The volume of `folder` and its free space. `Unknown` when the folder cannot be
    /// read, `Unsupported` on a platform without a reading.
    fn volume_space(&mut self, folder: &Path) -> Probed<VolumeSpace>;

    /// Physical memory available to programs without swapping, in bytes.
    fn available_memory(&mut self) -> Probed<u64>;
}

/// One answer of the probe worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReading {
    /// Per asked folder, in the order asked.
    pub folders: Vec<(PathBuf, Probed<VolumeSpace>)>,
    /// Available memory.
    pub memory: Probed<u64>,
}

/// What [`ProbeWorker::poll`] has for the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// A fresh reading.
    Reading(ProbeReading),
    /// The request has waited longer than the timeout: disk and memory are unknown
    /// until the worker answers. Reported once per request.
    TimedOut,
}

/// The probe on its own thread, asked every [`PROBE_INTERVAL`] on the caller's clock.
/// One request is in flight at a time: while the worker is busy (a hung drive), later
/// requests are skipped and counted, and no thread is added.
#[derive(Debug)]
pub struct ProbeWorker {
    ask: Option<Sender<Vec<PathBuf>>>,
    answers: Receiver<ProbeReading>,
    thread: Option<JoinHandle<()>>,
    /// When the request in flight was sent.
    asked_at: Option<Timestamp>,
    /// The request in flight was reported as timed out.
    timed_out: bool,
    /// When the next request is due.
    due: Timestamp,
    interval: Duration,
    timeout: Duration,
    skipped: u64,
}

impl ProbeWorker {
    /// Starts the worker thread; the first request is due at `now`.
    pub fn spawn(probe: Box<dyn SystemProbe>, now: Timestamp) -> std::io::Result<Self> {
        Self::with_timing(probe, now, PROBE_INTERVAL, PROBE_TIMEOUT)
    }

    /// [`ProbeWorker::spawn`] with another interval and timeout.
    pub fn with_timing(
        mut probe: Box<dyn SystemProbe>,
        now: Timestamp,
        interval: Duration,
        timeout: Duration,
    ) -> std::io::Result<Self> {
        let (ask, asks) = mpsc::channel::<Vec<PathBuf>>();
        let (answer, answers) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("rbs-protection-probe".into())
            .spawn(move || {
                while let Ok(folders) = asks.recv() {
                    let folders = folders
                        .into_iter()
                        .map(|f| {
                            let space = probe.volume_space(&f);
                            (f, space)
                        })
                        .collect();
                    let memory = probe.available_memory();
                    if answer.send(ProbeReading { folders, memory }).is_err() {
                        return;
                    }
                }
            })?;
        Ok(Self {
            ask: Some(ask),
            answers,
            thread: Some(thread),
            asked_at: None,
            timed_out: false,
            due: now,
            interval,
            timeout,
            skipped: 0,
        })
    }

    /// Takes an answer that arrived, reports a timeout, and sends the next request for
    /// `folders` when it is due. Never waits.
    pub fn poll(&mut self, now: Timestamp, folders: &[PathBuf]) -> Option<ProbeOutcome> {
        let mut outcome = None;
        match self.answers.try_recv() {
            Ok(reading) => {
                self.asked_at = None;
                self.timed_out = false;
                outcome = Some(ProbeOutcome::Reading(reading));
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                if let Some(at) = self.asked_at
                    && !self.timed_out
                    && now.saturating_since(at) >= self.timeout
                {
                    self.timed_out = true;
                    outcome = Some(ProbeOutcome::TimedOut);
                }
            }
        }
        if now >= self.due {
            self.due = now + self.interval;
            if self.asked_at.is_some() {
                self.skipped += 1;
            } else if let Some(ask) = &self.ask
                && ask.send(folders.to_vec()).is_ok()
            {
                self.asked_at = Some(now);
            }
        }
        outcome
    }

    /// How long the caller may wait before [`ProbeWorker::poll`] has something to do:
    /// the next request or the timeout of the one in flight. An answer that arrives
    /// earlier is taken on the next poll.
    pub fn wait(&self, now: Timestamp) -> Duration {
        let mut at = self.due;
        if let Some(asked) = self.asked_at
            && !self.timed_out
        {
            at = at.min(asked + self.timeout);
        }
        at.saturating_since(now)
    }

    /// A request is in flight.
    pub fn busy(&self) -> bool {
        self.asked_at.is_some()
    }

    /// Requests skipped because the worker was still busy.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Blocks up to `limit` of real time until an answer is ready to be polled. For
    /// tests (as a hang guard) and nothing else: the control thread never waits.
    pub fn wait_for_answer(&mut self, limit: Duration) -> Option<ProbeReading> {
        match self.answers.recv_timeout(limit) {
            Ok(r) => {
                self.asked_at = None;
                self.timed_out = false;
                Some(r)
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
        }
    }
}

impl Drop for ProbeWorker {
    fn drop(&mut self) {
        // Closing the request channel ends the worker after its current call. A call
        // that hangs is not waited for: the thread is left to end on its own, so a
        // stuck drive never holds the control thread.
        self.ask = None;
        if self.asked_at.is_none()
            && let Some(t) = self.thread.take()
        {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

    use super::*;
    use std::sync::{Arc, Mutex};

    /// Hang guard only.
    const LIMIT: Duration = Duration::from_secs(10);

    /// Answers at once, or blocks until `release` gets a message when `hold` is set.
    struct FakeProbe {
        hold: Option<Receiver<()>>,
        calls: Arc<Mutex<u32>>,
    }

    impl SystemProbe for FakeProbe {
        fn volume_space(&mut self, _folder: &Path) -> Probed<VolumeSpace> {
            *self.calls.lock().unwrap() += 1;
            if let Some(h) = &self.hold {
                let _ = h.recv_timeout(LIMIT);
            }
            Probed::Value(VolumeSpace {
                volume: VolumeKey(7),
                free_bytes: 42,
            })
        }
        fn available_memory(&mut self) -> Probed<u64> {
            Probed::Unsupported
        }
    }

    fn at(secs: u64) -> Timestamp {
        Timestamp::from_nanos(secs * 1_000_000_000)
    }

    #[test]
    fn asks_every_interval_and_hands_over_the_answer() {
        let calls = Arc::new(Mutex::new(0));
        let probe = FakeProbe {
            hold: None,
            calls: Arc::clone(&calls),
        };
        let mut w = ProbeWorker::spawn(Box::new(probe), at(0)).unwrap();
        let folders = [PathBuf::from("/rec")];
        assert_eq!(w.poll(at(0), &folders), None);
        assert!(w.busy());
        let r = w.wait_for_answer(LIMIT).expect("answer");
        assert_eq!(r.folders.len(), 1);
        assert_eq!(r.memory, Probed::Unsupported);
        assert!(!w.busy());
        // Not due yet: nothing is sent.
        assert_eq!(w.poll(at(4), &folders), None);
        assert!(!w.busy());
        assert_eq!(w.wait(at(4)), Duration::from_secs(1));
        assert_eq!(w.poll(at(5), &folders), None);
        assert!(w.busy());
        assert!(w.wait_for_answer(LIMIT).is_some());
        assert_eq!(*calls.lock().unwrap(), 2);
        assert_eq!(w.skipped(), 0);
    }

    #[test]
    fn a_hung_probe_times_out_once_skips_requests_and_answers_late() {
        let (release, hold) = mpsc::channel();
        let probe = FakeProbe {
            hold: Some(hold),
            calls: Arc::new(Mutex::new(0)),
        };
        let mut w = ProbeWorker::spawn(Box::new(probe), at(0)).unwrap();
        let folders = [PathBuf::from("/net")];
        assert_eq!(w.poll(at(0), &folders), None);
        assert_eq!(w.poll(at(5), &folders), None);
        assert_eq!(w.skipped(), 1);
        assert_eq!(w.wait(at(5)), Duration::from_secs(5));
        assert_eq!(w.poll(at(10), &folders), Some(ProbeOutcome::TimedOut));
        assert_eq!(w.skipped(), 2);
        // Reported once.
        assert_eq!(w.poll(at(12), &folders), None);
        release.send(()).unwrap();
        // The late answer still arrives and is taken; the next request goes out.
        let r = w.wait_for_answer(LIMIT).expect("late answer");
        assert_eq!(r.folders[0].0, PathBuf::from("/net"));
        assert!(!w.busy());
        assert_eq!(w.poll(at(15), &folders), None);
        assert!(w.busy());
        release.send(()).unwrap();
        assert!(w.wait_for_answer(LIMIT).is_some());
    }

    #[test]
    fn dropping_a_worker_with_a_hung_call_does_not_wait() {
        let (_release, hold) = mpsc::channel::<()>();
        let probe = FakeProbe {
            hold: Some(hold),
            calls: Arc::new(Mutex::new(0)),
        };
        let mut w = ProbeWorker::spawn(Box::new(probe), at(0)).unwrap();
        w.poll(at(0), &[PathBuf::from("/x")]);
        drop(w);
        // Reaching this line is the test: drop returned while the call hangs. The
        // fake's own bound ends the thread later.
    }
}
