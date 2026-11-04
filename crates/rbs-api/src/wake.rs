//! The API thread's one wake primitive (ADR-0044 decision 5): permit-based, so a signal
//! sent before the thread sleeps is not lost.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

/// A permit the transport inbox, the response channel and the event slots signal, and
/// the API thread waits on.
#[derive(Debug, Default)]
pub struct Wake {
    permit: Mutex<bool>,
    cv: Condvar,
}

impl Wake {
    /// A wake primitive without a permit.
    pub fn new() -> Self {
        Self::default()
    }

    /// Leaves a permit and wakes the waiter. Never blocks for long: the lock is only
    /// held to set or clear the flag.
    pub fn signal(&self) {
        let mut permit = self.permit.lock().unwrap_or_else(PoisonError::into_inner);
        *permit = true;
        self.cv.notify_one();
    }

    /// Waits until a permit is there (and takes it) or `timeout` passed (`None`: no
    /// limit). Returns true when a permit was taken.
    pub fn wait(&self, timeout: Option<Duration>) -> bool {
        let mut permit = self.permit.lock().unwrap_or_else(PoisonError::into_inner);
        match timeout {
            None => {
                while !*permit {
                    permit = self.cv.wait(permit).unwrap_or_else(PoisonError::into_inner);
                }
            }
            Some(timeout) => {
                let (guard, _) = self
                    .cv
                    .wait_timeout_while(permit, timeout, |p| !*p)
                    .unwrap_or_else(PoisonError::into_inner);
                permit = guard;
            }
        }
        std::mem::replace(&mut *permit, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signal_before_the_wait_is_kept() {
        let wake = Wake::new();
        wake.signal();
        assert!(wake.wait(Some(Duration::ZERO)));
        assert!(!wake.wait(Some(Duration::ZERO)));
    }
}
