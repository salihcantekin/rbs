//! Wake-up lateness of a periodic thread, with and without `OsThreadPolicy`, idle and
//! with every CPU kept busy by normal-priority spinners (measure-performance skill,
//! ADR-0030). A thread parks until each deadline of a 1 kHz cadence, like an engine
//! worker; lateness is how long after the deadline it runs.
//!
//! `cargo bench -p rbs-thread-os --bench wakeup`

#![allow(clippy::unwrap_used, clippy::expect_used)] // Benches fail by panicking.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rbs_core::thread::{ThreadClass, ThreadPolicy, ThreadRequest};
use rbs_thread_os::OsThreadPolicy;

const PERIOD: Duration = Duration::from_millis(1);
const TICKS: usize = 2_000;

fn run(policy: bool) -> (String, Vec<Duration>) {
    std::thread::spawn(move || {
        let os = OsThreadPolicy::new();
        let guard =
            policy.then(|| os.enter(&ThreadRequest::new("bench", ThreadClass::periodic(PERIOD))));
        let grant = match &guard {
            None => "none".to_owned(),
            Some(Ok(g)) => format!("{:?}", g.grant()),
            Some(Err(e)) => format!("error: {e}"),
        };
        let mut late = Vec::with_capacity(TICKS);
        let start = Instant::now();
        for i in 1..=TICKS {
            let due = start + PERIOD * i as u32;
            loop {
                let now = Instant::now();
                if now >= due {
                    late.push(now - due);
                    break;
                }
                std::thread::park_timeout(due - now);
            }
        }
        drop(guard);
        (grant, late)
    })
    .join()
    .unwrap()
}

fn report(name: &str, (grant, mut late): (String, Vec<Duration>)) {
    late.sort();
    let at = |q: f64| late[((late.len() - 1) as f64 * q) as usize];
    println!(
        "wakeup/{name}: grant {grant}; lateness p50 {:.1} us, p99 {:.1} us, max {:.1} us",
        at(0.5).as_secs_f64() * 1e6,
        at(0.99).as_secs_f64() * 1e6,
        late[late.len() - 1].as_secs_f64() * 1e6,
    );
}

fn main() {
    report("idle_default", run(false));
    report("idle_policy", run(true));
    let stop = Arc::new(AtomicBool::new(false));
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    let spinners: Vec<_> = (0..cpus * 2)
        .map(|_| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            })
        })
        .collect();
    report("loaded_default", run(false));
    report("loaded_policy", run(true));
    stop.store(true, Ordering::Relaxed);
    for s in spinners {
        s.join().unwrap();
    }
}
