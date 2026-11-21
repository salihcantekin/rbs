//! A latest-only hand-off from a frame thread to the UI thread (stability plan WP-J).
//!
//! The preview, Interact and projector frame threads used to post one event-loop call
//! per received frame. When the UI thread was slow (a long layout, a resize, a busy
//! main window), those calls queued up: each one held a frame, and therefore one of the
//! publisher's few slots, until the UI ran it, and then the UI showed every stale frame
//! in turn. With [`Latest`] a frame thread replaces the waiting value and posts a call
//! only when none is pending; the call takes whatever is newest when it runs. A
//! replaced frame is dropped on the frame thread at once, so its slot returns to the
//! publisher without waiting for the UI.
//!
//! No toolkit here: the caller posts the call (`entry::invoke_latest` in the app).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

/// One waiting value and whether a call to take it is already posted.
#[derive(Debug)]
pub struct Latest<T> {
    value: Mutex<Option<T>>,
    posted: AtomicBool,
}

impl<T> Default for Latest<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Latest<T> {
    /// An empty slot with no call posted.
    pub const fn new() -> Self {
        Self {
            value: Mutex::new(None),
            posted: AtomicBool::new(false),
        }
    }

    /// Stores `value` in place of the waiting one, which is returned so the caller drops
    /// it outside the lock. `true` in the pair when the caller must post a call that
    /// runs [`take`](Self::take): none was pending.
    pub fn put(&self, value: T) -> (bool, Option<T>) {
        let replaced = self
            .value
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(value);
        // After the store: a call that clears the flag after this point sees the value.
        let post = !self.posted.swap(true, Ordering::AcqRel);
        (post, replaced)
    }

    /// The newest value, run by the posted call. The flag is cleared before the value
    /// is taken, so a value stored meanwhile either is taken here or posts a new call:
    /// none is left behind without a call (an extra call finds nothing and does
    /// nothing).
    pub fn take(&self) -> Option<T> {
        self.posted.store(false, Ordering::Release);
        self.value
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// A posted call could not be delivered (the event loop is gone): the next
    /// [`put`](Self::put) posts again.
    pub fn unpost(&self) {
        self.posted.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn the_first_put_posts_and_later_ones_replace() {
        let slot = Latest::new();
        assert_eq!(slot.put(1), (true, None));
        assert_eq!(slot.put(2), (false, Some(1)));
        assert_eq!(slot.put(3), (false, Some(2)));
        assert_eq!(slot.take(), Some(3));
        assert_eq!(slot.take(), None);
    }

    #[test]
    fn a_put_after_a_take_posts_again() {
        let slot = Latest::new();
        assert!(slot.put(1).0);
        assert_eq!(slot.take(), Some(1));
        assert!(slot.put(2).0);
        assert_eq!(slot.take(), Some(2));
    }

    #[test]
    fn a_failed_post_is_posted_again() {
        let slot = Latest::new();
        assert!(slot.put(1).0);
        slot.unpost();
        assert!(slot.put(2).0);
    }

    /// The race the flag order closes: a value stored between the flag's clear and the
    /// take is taken by that call, and the extra call it posted finds nothing.
    #[test]
    fn a_put_between_clear_and_take_is_never_lost() {
        let slot = Latest::new();
        assert!(slot.put(1).0);
        // `take` split at its two steps.
        slot.posted.store(false, Ordering::Release);
        assert!(slot.put(2).0, "the flag was clear: a new call is posted");
        assert_eq!(
            slot.value
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
            Some(2)
        );
        assert_eq!(slot.take(), None, "the extra call does nothing");
    }

    /// Many frames from another thread while calls run: every posted call is matched
    /// and the last value is always seen (bounded by a count, not by time).
    #[test]
    fn the_last_value_of_a_burst_is_taken() {
        const N: u32 = 10_000;
        let slot = Arc::new(Latest::new());
        let posts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let producer = {
            let slot = Arc::clone(&slot);
            let posts = Arc::clone(&posts);
            std::thread::spawn(move || {
                for i in 1..=N {
                    if slot.put(i).0 {
                        posts.fetch_add(1, Ordering::AcqRel);
                    }
                }
            })
        };
        let mut taken = 0;
        let mut last = 0;
        let mut run = |taken: &mut u32| {
            *taken += 1;
            if let Some(v) = slot.take() {
                assert!(v > last, "values arrive in order");
                last = v;
            }
        };
        // Run calls while the producer runs, then the ones still posted.
        while !producer.is_finished() {
            if taken < posts.load(Ordering::Acquire) {
                run(&mut taken);
            } else {
                std::thread::yield_now();
            }
        }
        assert!(producer.join().is_ok());
        while taken < posts.load(Ordering::Acquire) {
            run(&mut taken);
        }
        assert_eq!(last, N, "the newest value reached a call");
    }
}
