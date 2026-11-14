//! The estimate's disk probe (ADR-0087 decision 1): never waits, caches per folder,
//! times out to unknown, one read at a time.

use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_record::estimate::{CACHE_FOR, DiskProbe, FreeSpace, PROBE_TIMEOUT, ReadFreeSpace};

/// Hang guard only.
const LIMIT: Duration = Duration::from_secs(10);

fn at(secs: u64) -> Timestamp {
    Timestamp::from_nanos(secs * 1_000_000_000)
}

/// Answers `free` at once and counts the reads.
fn counting(free: u64) -> (ReadFreeSpace, Arc<Mutex<u32>>) {
    let calls = Arc::new(Mutex::new(0));
    let c = Arc::clone(&calls);
    let read: ReadFreeSpace = Arc::new(move |_: &Path| {
        if let Ok(mut n) = c.lock() {
            *n += 1;
        }
        Ok(free)
    });
    (read, calls)
}

fn reads(calls: &Arc<Mutex<u32>>) -> u32 {
    calls.lock().map(|n| *n).unwrap_or(u32::MAX)
}

#[test]
fn the_first_ask_is_pending_and_the_answer_is_cached_for_a_minute() {
    let (read, calls) = counting(500);
    let mut probe = DiskProbe::new(read);
    let folder = Path::new("/rec");
    assert_eq!(probe.free_space(folder, at(0)), FreeSpace::Pending);
    assert!(probe.wait_for_answer(LIMIT));
    assert_eq!(probe.free_space(folder, at(1)), FreeSpace::Known(500));
    let stale = at(0) + CACHE_FOR;
    assert_eq!(
        probe.free_space(folder, stale - Duration::from_nanos(1)),
        FreeSpace::Known(500)
    );
    assert_eq!(reads(&calls), 1);
    // Stale: the old value is not used and a new read starts.
    assert_eq!(probe.free_space(folder, stale), FreeSpace::Pending);
    assert!(probe.wait_for_answer(LIMIT));
    assert_eq!(reads(&calls), 2);
}

#[test]
fn a_failed_read_is_unknown_with_its_reason() {
    let read: ReadFreeSpace = Arc::new(|_: &Path| Err("no such folder".to_owned()));
    let mut probe = DiskProbe::new(read);
    let folder = Path::new("/gone");
    assert_eq!(probe.free_space(folder, at(0)), FreeSpace::Pending);
    assert!(probe.wait_for_answer(LIMIT));
    assert_eq!(
        probe.free_space(folder, at(1)),
        FreeSpace::Unknown("no such folder".to_owned())
    );
    assert_eq!(probe.free_space(folder, at(1)).bytes(), None);
}

#[test]
fn a_slow_read_times_out_to_unknown_and_holds_one_thread_at_most() {
    let (release, hold) = mpsc::channel::<()>();
    let hold = Arc::new(Mutex::new(hold));
    let calls = Arc::new(Mutex::new(0_u32));
    let c = Arc::clone(&calls);
    let read: ReadFreeSpace = Arc::new(move |_: &Path| {
        if let Ok(mut n) = c.lock() {
            *n += 1;
        }
        if let Ok(h) = hold.lock() {
            let _ = h.recv_timeout(LIMIT);
        }
        Ok(9)
    });
    let mut probe = DiskProbe::new(read);
    let (a, b) = (Path::new("/net"), Path::new("/local"));
    assert_eq!(probe.free_space(a, at(0)), FreeSpace::Pending);
    assert!(probe.busy());
    // Before the timeout: still pending, and a second folder starts no second read.
    let before = at(0) + PROBE_TIMEOUT - Duration::from_nanos(1);
    assert_eq!(probe.free_space(a, before), FreeSpace::Pending);
    assert_eq!(probe.free_space(b, before), FreeSpace::Pending);
    let after = at(0) + PROBE_TIMEOUT;
    assert!(matches!(probe.free_space(a, after), FreeSpace::Unknown(_)));
    assert!(probe.busy());
    send(&release);
    assert!(probe.wait_for_answer(LIMIT));
    assert_eq!(probe.free_space(a, after), FreeSpace::Known(9));
    // The answer took the reading slot; the other folder is read now.
    assert_eq!(probe.free_space(b, after), FreeSpace::Pending);
    send(&release);
    assert!(probe.wait_for_answer(LIMIT));
    assert_eq!(reads(&calls), 2);
}

fn send(tx: &Sender<()>) {
    tx.send(()).unwrap_or_else(|e| panic!("release: {e}"));
}

#[test]
fn nothing_runs_until_asked() {
    let (read, calls) = counting(1);
    let mut probe = DiskProbe::new(read);
    probe.poll(at(100));
    assert!(!probe.busy());
    assert_eq!(reads(&calls), 0);
}
