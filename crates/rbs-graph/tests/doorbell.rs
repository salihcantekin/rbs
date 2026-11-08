//! Doorbell hand-off between a ringer and a waiting worker (ADR-0032: no sleeps; the
//! time bound is a hang guard only).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rbs_graph::runtime::queue::Doorbell;

/// Hang guard, far above what the loop needs.
const GUARD: Duration = Duration::from_secs(20);
const ROUNDS: u64 = 20_000;

/// Every ring must wake a worker that waits with a very long timer. The ringer rings
/// once, then waits until the worker took that ring before it rings again, so a lost
/// ring stalls the pair (the worker would sleep for the timer) instead of hiding behind
/// the next ring. This is the shape of the last packet before a pause.
#[test]
fn no_ring_is_lost_with_a_long_timer() {
    let bell = Arc::new(Doorbell::new());
    let taken = Arc::new(AtomicU64::new(0));
    let worker = {
        let (bell, taken) = (Arc::clone(&bell), Arc::clone(&taken));
        thread::spawn(move || {
            while taken.load(Ordering::Acquire) < ROUNDS {
                if bell.wait(Duration::from_secs(3600)) {
                    taken.fetch_add(1, Ordering::Release);
                }
            }
        })
    };
    let start = Instant::now();
    for round in 1..=ROUNDS {
        bell.ring();
        while taken.load(Ordering::Acquire) < round {
            if start.elapsed() > GUARD {
                // Releases the worker, then reports the lost ring.
                taken.store(ROUNDS, Ordering::Release);
                bell.ring();
                worker.join().unwrap();
                panic!("ring {round} of {ROUNDS} did not wake the worker");
            }
            thread::yield_now();
        }
    }
    worker.join().unwrap();
}
