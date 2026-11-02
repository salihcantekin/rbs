//! Latest-value parameters from the control thread to a running node (ADR-0023).
//!
//! The control side keeps a [`ParamSender`]; the node owns the matching
//! [`ParamReceiver`] and calls [`ParamReceiver::update`] in `process`. Only the newest
//! value counts: a value sent while an older one is still unread replaces it.
//!
//! Real-time rules on the receiving side: `update` never allocates, never frees and
//! never waits. Values move by `std::mem::swap`; the value a new one replaces goes back
//! to the sender, which drops it (or hands it out for reuse through
//! [`ParamSender::recycle`]) on the control thread. The exchange is a mutex that the
//! receiver only ever `try_lock`s: when the sender holds it (for a few moves), the
//! receiver keeps its current value and picks the new one up on its next call.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

/// The state both ends share.
struct Shared<T> {
    /// Set (under the lock) when `fresh` is filled and cleared when it is taken, so
    /// the receiver's common case, nothing new, is one atomic load.
    fresh_ready: AtomicBool,
    exchange: Mutex<Exchange<T>>,
}

/// What the two sides exchange. The sender holds the lock only to move values in and
/// out, never while running user code or dropping a value.
struct Exchange<T> {
    /// The newest value the receiver has not taken yet.
    fresh: Option<T>,
    /// A value the receiver replaced, waiting for the sender to drop or reuse it.
    used: Option<T>,
}

/// Creates a connected sender (control thread) and receiver (owned by the node).
///
/// The receiver starts with `initial`. Both ends are `Send` when `T` is; neither is
/// `Clone`, so there is exactly one writer and one reader.
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
            spare: None,
        },
    )
}

/// The control-thread end of a parameter channel.
pub struct ParamSender<T> {
    shared: Arc<Shared<T>>,
}

impl<T> ParamSender<T> {
    /// Makes `value` the newest value. The receiver takes it on its next
    /// [`ParamReceiver::update`]; an older value it has not taken yet is dropped here.
    pub fn send(&mut self, value: T) {
        let (_unread, _used) = {
            let mut x = lock(&self.shared.exchange);
            let replaced = (x.fresh.replace(value), x.used.take());
            self.shared.fresh_ready.store(true, Ordering::Release);
            replaced
        };
        // `_unread` and `_used` drop here, after the lock is released.
    }

    /// Takes a value the receiver no longer uses, so its allocations can be reused for
    /// the next [`ParamSender::send`] (for example clear and refill a `Vec`).
    pub fn recycle(&mut self) -> Option<T> {
        lock(&self.shared.exchange).used.take()
    }

    /// True while a sent value has not been taken by the receiver.
    pub fn pending(&self) -> bool {
        self.shared.fresh_ready.load(Ordering::Acquire)
    }

    /// True once the receiver is gone (the node was dropped).
    pub fn is_closed(&self) -> bool {
        Arc::strong_count(&self.shared) == 1
    }
}

/// The node's end of a parameter channel. Lives in the node and is used in `process`.
pub struct ParamReceiver<T> {
    shared: Arc<Shared<T>>,
    current: T,
    /// A replaced value the sender has not made room for yet; while it is held, no new
    /// value is taken, so nothing is ever dropped here.
    spare: Option<T>,
}

impl<T> ParamReceiver<T> {
    /// Takes the newest value if one was sent since the last call, and returns true if
    /// the current value changed.
    ///
    /// Real time: no allocation, no deallocation, no waiting. If the sender holds the
    /// exchange at this moment, or has not taken back the previously replaced value,
    /// this returns false and the new value is taken on a later call.
    pub fn update(&mut self) -> bool {
        if self.spare.is_none() && !self.shared.fresh_ready.load(Ordering::Acquire) {
            return false;
        }
        let mut x = match self.shared.exchange.try_lock() {
            Ok(guard) => guard,
            // Poisoning cannot leave the exchange inconsistent: the sender only moves
            // `Option`s while holding it.
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        if x.used.is_none() {
            x.used = self.spare.take();
        }
        if self.spare.is_some() {
            return false;
        }
        let Some(mut next) = x.fresh.take() else {
            return false;
        };
        self.shared.fresh_ready.store(false, Ordering::Release);
        std::mem::swap(&mut self.current, &mut next);
        // `next` now holds the replaced value; hand it back without dropping it here.
        if x.used.is_none() {
            x.used = Some(next);
        } else {
            self.spare = Some(next);
        }
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

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // See `update`: the exchange holds only `Option`s, so a poisoned lock is usable.
    m.lock().unwrap_or_else(|p| p.into_inner())
}
