//! Latest-value parameters from the control thread to a running node (ADR-0023).
//!
//! The control side keeps a [`ParamSender`]; the node owns the matching
//! [`ParamReceiver`] and calls [`ParamReceiver::update`] in `process`. Only the newest
//! value counts: a value sent while an older one is still unread replaces it.
//!
//! Real-time rules on the receiving side: `update` never allocates, never frees and
//! never waits. With nothing new it is one atomic load. Otherwise it `try_lock`s the
//! exchange (never `lock`) and moves values with `std::mem::swap`; the value a new one
//! replaces goes back to the sender, which drops it or hands it out for reuse through
//! [`ParamSender::recycle`]. When the sender holds the exchange at that moment, the
//! receiver keeps its current value and takes the new one on the first call after the
//! sender let go, normally the next tick.
//!
//! Keep `T` small or heap-backed (a `Vec`, a `Box`): it is moved under the lock and
//! swapped inline.
//!
//! Where values are dropped: the sender drops displaced values on its own thread. The
//! receiver's current value, and anything left in the exchange when the sender is gone,
//! drop where the node drops, which is the control thread under ADR-0020 (or the
//! executor thread only if an executor is dropped without draining, at shutdown).

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

/// The state both ends share.
struct Shared<T> {
    /// Set when `fresh` is filled and cleared when it is taken, both under the lock,
    /// so the receiver's common case, nothing new, is one atomic load. The mutex
    /// orders the values; the flag is only a hint, and a stale `false` delays a value
    /// by one call without losing it.
    fresh_ready: AtomicBool,
    exchange: Mutex<Exchange<T>>,
}

/// What the two sides exchange. Both ends hold the lock only to move `Option`s, never
/// while running user code or dropping a value.
///
/// Invariant: `fresh` and `used` are never both `Some`, because `send` empties `used`
/// whenever it fills `fresh`, and `update` fills `used` only when it empties `fresh`.
/// So `update` always has room to hand back the value it replaces.
struct Exchange<T> {
    /// The newest value the receiver has not taken yet.
    fresh: Option<T>,
    /// The value the receiver last replaced, waiting for the sender.
    used: Option<T>,
}

/// Creates a connected sender (control thread) and receiver (owned by the node).
///
/// The receiver starts with `initial`. Neither end is `Clone`, so there is exactly one
/// writer and one reader.
#[must_use]
pub fn channel<T: Send>(initial: T) -> (ParamSender<T>, ParamReceiver<T>) {
    let shared = Arc::new(Shared {
        fresh_ready: AtomicBool::new(false),
        exchange: Mutex::new(Exchange {
            fresh: None,
            used: None,
        }),
    });
    // Lock once here, on the creating thread, so a platform mutex that initialises
    // itself lazily never does so on the real-time thread.
    drop(lock(&shared.exchange));
    (
        ParamSender {
            shared: Arc::clone(&shared),
        },
        ParamReceiver {
            shared,
            current: initial,
        },
    )
}

/// The control-thread end of a parameter channel.
///
/// `send` and `recycle` take `&mut self`, so values come from one thread at a time
/// and arrive in the order sent. Control code that runs on several threads wraps the
/// sender itself (for example in a `Mutex` on the control side).
pub struct ParamSender<T> {
    shared: Arc<Shared<T>>,
}

impl<T> ParamSender<T> {
    /// Makes `value` the newest value; the receiver takes it on its next
    /// [`ParamReceiver::update`].
    ///
    /// Returns the older value it displaced if the receiver had not taken that one
    /// yet, so its allocations can be reused. A replaced value that was not taken
    /// with [`ParamSender::recycle`] before this call is dropped here.
    pub fn send(&mut self, value: T) -> Option<T> {
        let (unread, _used) = {
            let mut x = lock(&self.shared.exchange);
            let moved = (x.fresh.replace(value), x.used.take());
            self.shared.fresh_ready.store(true, Ordering::Release);
            moved
        };
        // `_used` drops here, after the lock is released.
        unread
    }

    /// Takes the value the receiver last replaced, so its allocations can be reused
    /// for the next [`ParamSender::send`] (for example clear and refill a `Vec`). Call
    /// it before `send`, which drops a value that was not taken.
    pub fn recycle(&mut self) -> Option<T> {
        lock(&self.shared.exchange).used.take()
    }

    /// True while a sent value has not been taken by the receiver. Stays true while
    /// the node is idle or after it was dropped (see [`ParamSender::is_closed`]).
    pub fn pending(&self) -> bool {
        self.shared.fresh_ready.load(Ordering::Acquire)
    }

    /// True once the receiver is gone (the node was dropped).
    pub fn is_closed(&self) -> bool {
        Arc::strong_count(&self.shared) == 1
    }
}

impl<T> fmt::Debug for ParamSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParamSender")
            .field("pending", &self.pending())
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// The node's end of a parameter channel. Lives in the node and is used in `process`.
pub struct ParamReceiver<T> {
    shared: Arc<Shared<T>>,
    current: T,
}

impl<T> ParamReceiver<T> {
    /// Takes the newest value if one was sent since the last call, and returns true if
    /// the current value changed.
    ///
    /// Real time: no allocation, no deallocation, no waiting. If the sender holds the
    /// exchange at this moment, this returns false and the value is taken on a later
    /// call. On futex-based platforms, releasing the lock may wake a sender that was
    /// parked waiting for it; that is one non-blocking system call, and only when the
    /// two sides met on the lock.
    pub fn update(&mut self) -> bool {
        if !self.shared.fresh_ready.load(Ordering::Acquire) {
            return false;
        }
        let mut x = match self.shared.exchange.try_lock() {
            Ok(guard) => guard,
            // Poisoning cannot leave the exchange inconsistent: both ends only move
            // `Option`s while holding it.
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        let Some(mut next) = x.fresh.take() else {
            return false;
        };
        self.shared.fresh_ready.store(false, Ordering::Relaxed);
        debug_assert!(
            x.used.is_none(),
            "send empties `used` when it fills `fresh`"
        );
        std::mem::swap(&mut self.current, &mut next);
        // `next` now holds the replaced value. `used` is empty (see `Exchange`), so
        // this hands it back without dropping anything here.
        x.used = Some(next);
        true
    }

    /// The current value.
    pub fn get(&self) -> &T {
        &self.current
    }

    /// The current value, mutable (for example to keep per-node caches next to it).
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.current
    }
}

impl<T: fmt::Debug> fmt::Debug for ParamReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParamReceiver")
            .field("current", &self.current)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // See `update`: the exchange holds only `Option`s, so a poisoned lock is usable.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn update_skips_a_held_exchange_and_takes_the_value_on_the_next_call() {
        let (mut tx, mut rx) = channel(vec![0_u8; 8]);
        let _ = tx.send(vec![1; 8]);
        let held = lock(&tx.shared.exchange);
        let info = allocation_counter::measure(|| {
            assert!(!rx.update(), "the exchange is held: no change, no wait");
        });
        assert_eq!(info.count_total, 0);
        assert_eq!(info.count_current, 0);
        assert_eq!(rx.get()[0], 0);
        drop(held);
        assert!(rx.update());
        assert_eq!(rx.get()[0], 1);
    }

    #[test]
    fn the_exchange_invariant_holds_after_any_sequence() {
        let (mut tx, mut rx) = channel(0_u32);
        for step in 0..1_000_u32 {
            match step % 5 {
                0 | 3 => {
                    let _ = tx.send(step);
                }
                1 => {
                    let _ = rx.update();
                }
                2 => {
                    let _ = tx.recycle();
                }
                _ => {
                    let _ = rx.update();
                    let _ = rx.update();
                }
            }
            let x = lock(&tx.shared.exchange);
            assert!(!(x.fresh.is_some() && x.used.is_some()));
            assert_eq!(x.fresh.is_some(), tx.pending());
        }
    }
}
