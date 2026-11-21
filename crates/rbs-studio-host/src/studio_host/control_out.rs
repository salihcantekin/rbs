//! The control thread's command channel (ADR-0154).
//!
//! The control thread parks with no deadline while nothing runs, and a park does not
//! notice a channel send or a disconnect. So the channel's only `Sender` lives here,
//! private: every send unparks, and dropping the last `ControlOut` disconnects first
//! and then unparks. Nothing outside this file can send or disconnect without a wake.

use std::sync::mpsc::{SendError, Sender};
use std::sync::{Arc, OnceLock};

use rbs_graph::runtime::ControlWake;

use super::Msg;

/// The control thread's command channel. Sending also unparks that thread once it
/// has bound itself (ADR-0154). A bare channel send does not wake a `park`.
pub(crate) struct ControlOut<M = Msg> {
    /// `Some` until [`Drop`]: the one `Sender` of the channel, never cloned or lent.
    tx: Option<Sender<M>>,
    wake: OnceLock<Arc<ControlWake>>,
}

impl<M> ControlOut<M> {
    /// The channel's sending side. The receiver is the control thread's.
    pub(super) fn new(tx: Sender<M>) -> Arc<Self> {
        Arc::new(Self {
            tx: Some(tx),
            wake: OnceLock::new(),
        })
    }

    /// The control thread bound `wake` to itself: sends and the drop unpark it from
    /// now on. Once only; a later call keeps the first cell.
    pub(super) fn bind(&self, wake: Arc<ControlWake>) {
        let _ = self.wake.set(wake);
    }

    // The same shape as `Sender::send`: the error hands the message back, so it is as
    // large as `Msg`. Callers only test it; nothing moves it around.
    #[allow(clippy::result_large_err)]
    pub(super) fn send(&self, msg: M) -> Result<(), SendError<M>> {
        let sent = match &self.tx {
            Some(tx) => tx.send(msg),
            None => Err(SendError(msg)),
        };
        self.unpark();
        sent
    }

    fn unpark(&self) {
        if let Some(wake) = self.wake.get() {
            wake.unpark();
        }
    }
}

impl<M> Drop for ControlOut<M> {
    /// Disconnects, then unparks: a thread woken before the disconnect would find the
    /// channel empty and park again for good. Runs on whichever thread drops the last
    /// reference, the UI's `StudioHost` or a worker that upgraded its `Weak`.
    fn drop(&mut self) {
        drop(self.tx.take());
        self.unpark();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use super::*;

    /// A hang guard only: every wait below ends on an event, not on this time.
    const HANG_GUARD: Duration = Duration::from_secs(10);

    enum Seen {
        /// The thread found the channel empty and is about to park with no deadline.
        Idle,
        Got(u32),
        Disconnected,
    }

    /// A thread that waits like `studio_host::run`: `try_recv`, then an untimed park.
    /// It reports `Idle` after its empty `try_recv` and before the park, so a message
    /// sent after `Idle` reaches it only through an unpark.
    fn parked_receiver(
        rx: Receiver<u32>,
        wake: Arc<ControlWake>,
    ) -> (JoinHandle<()>, Receiver<Seen>) {
        let (seen_tx, seen) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            wake.bind();
            loop {
                match rx.try_recv() {
                    Ok(m) => {
                        let _ = seen_tx.send(Seen::Got(m));
                    }
                    Err(TryRecvError::Empty) => {
                        let _ = seen_tx.send(Seen::Idle);
                        std::thread::park();
                    }
                    Err(TryRecvError::Disconnected) => {
                        let _ = seen_tx.send(Seen::Disconnected);
                        return;
                    }
                }
            }
        });
        (thread, seen)
    }

    fn start() -> (Arc<ControlOut<u32>>, JoinHandle<()>, Receiver<Seen>) {
        let (tx, rx) = mpsc::channel();
        let out = ControlOut::new(tx);
        let wake = ControlWake::new();
        out.bind(Arc::clone(&wake));
        let (thread, seen) = parked_receiver(rx, wake);
        (out, thread, seen)
    }

    /// The next report that is not `Idle`.
    fn next_event(seen: &Receiver<Seen>) -> Result<Seen, RecvTimeoutError> {
        loop {
            match seen.recv_timeout(HANG_GUARD)? {
                Seen::Idle => continue,
                other => return Ok(other),
            }
        }
    }

    fn wait_idle(seen: &Receiver<Seen>) {
        assert!(matches!(seen.recv_timeout(HANG_GUARD), Ok(Seen::Idle)));
    }

    #[test]
    fn a_send_wakes_a_thread_parked_without_a_deadline() {
        let (out, thread, seen) = start();
        for m in 0..3 {
            wait_idle(&seen);
            assert!(out.send(m).is_ok());
            assert!(matches!(next_event(&seen), Ok(Seen::Got(got)) if got == m));
        }
        drop(out);
        assert!(matches!(next_event(&seen), Ok(Seen::Disconnected)));
        assert!(thread.join().is_ok());
    }

    #[test]
    fn dropping_the_last_reference_wakes_the_parked_thread_to_see_the_disconnect() {
        let (out, thread, seen) = start();
        wait_idle(&seen);
        drop(out);
        assert!(matches!(next_event(&seen), Ok(Seen::Disconnected)));
        assert!(thread.join().is_ok());
    }

    #[test]
    fn a_worker_dropping_the_last_reference_wakes_the_parked_thread() {
        let (out, thread, seen) = start();
        let weak = Arc::downgrade(&out);
        wait_idle(&seen);
        // A picker or writer thread holds an upgraded `Weak` while the owner lets go.
        let Some(held) = weak.upgrade() else {
            panic!("the owner still holds the channel");
        };
        drop(out);
        let worker = std::thread::spawn(move || {
            assert!(held.send(7).is_ok());
            drop(held);
        });
        assert!(worker.join().is_ok());
        assert!(matches!(next_event(&seen), Ok(Seen::Got(7))));
        assert!(matches!(next_event(&seen), Ok(Seen::Disconnected)));
        assert!(thread.join().is_ok());
        assert!(weak.upgrade().is_none());
    }
}
