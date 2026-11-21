//! Fault injection with the test-double helper (WP-H, ADR-0109 decision 12, ADR-0157
//! decision 11): a helper that dies, hangs or publishes bad values leaves the host
//! running, gives every lease back and keeps its last picture.
//!
//! Waits are for conditions (a picture, an exit) with a hang guard, never for timing.
//! The GPU tests run on every platform: the test double publishes through the slot
//! protocol with no texture, and the test's fence stands in for the shared fence.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rbs_browser_ipc::exit::Cause;
use rbs_browser_ipc::{Configure, D3dShare, GpuShare, HEARTBEAT_LIMIT, MAX_GPU_TEXTURES};
use rbs_browser_runner::{
    FENCE_JUMP, FenceView, HANDOVER_LIMIT, HandoverPoll, HelperChild, PagePicture, Supervisor,
    SurfaceHandover, spawn_helper, spawn_helper_prepared,
};
use rbs_browser_shm::{FrameRing, SlotLayout};

const GUARD: Duration = Duration::from_secs(20);
const SLOTS: u32 = 4;

fn program() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test executable");
    path.pop();
    if path.file_name().is_some_and(|name| name == "deps") {
        path.pop();
    }
    path.push(format!(
        "rbs-browser-helper{}",
        std::env::consts::EXE_SUFFIX
    ));
    path
}

fn config() -> Configure {
    Configure {
        url: "https://example.com/".into(),
        width: 4,
        height: 2,
        fps: 60,
        transparent: false,
        frames: String::new(),
        sound: String::new(),
        sample_rate: 0,
        page: rbs_browser_ipc::PageOptions::default(),
        gpu: None,
    }
}

fn spawn(args: &[&str]) -> HelperChild {
    spawn_helper(
        program().as_path(),
        args,
        &config(),
        SlotLayout::Rgba8Straight,
    )
    .unwrap()
}

/// A helper on the GPU path whose fence reads `fence`. No texture exists; the
/// handles are placeholders the test double never opens.
fn spawn_gpu(args: &[&str], fence: &Arc<AtomicU64>) -> HelperChild {
    let mut child = spawn_helper_prepared(
        program().as_path(),
        args,
        &[],
        &config(),
        SlotLayout::Bgra8Premultiplied,
        |_| {
            Some(GpuShare::D3d(D3dShare {
                adapter_luid: 1,
                fence: 1,
                textures: [1; MAX_GPU_TEXTURES],
                count: SLOTS,
            }))
        },
    )
    .unwrap();
    child.set_gpu_gate(Box::new(Fence(Arc::clone(fence))), 60);
    child
}

struct Fence(Arc<AtomicU64>);

impl FenceView for Fence {
    fn completed(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A fence far enough ahead that every picture's copy has finished, and close
/// enough to stay plausible.
const DONE: u64 = FENCE_JUMP / 2;

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < GUARD, "hang guard: {what}");
        std::thread::yield_now();
    }
}

/// Starts a supervisor that gives each GPU lease straight back, as a frame that was
/// shown and dropped would, and counts pictures.
fn supervise_gpu(child: HelperChild) -> (Supervisor, Arc<AtomicU32>, Arc<Mutex<Vec<u64>>>) {
    let view = FrameRing::open(child.frame_name(), 4, 2).unwrap();
    let pictures = Arc::new(AtomicU32::new(0));
    let fences = Arc::new(Mutex::new(Vec::new()));
    let (count, seen) = (Arc::clone(&pictures), Arc::clone(&fences));
    let sup = Supervisor::start(child, 60, move |_, _, _, _, picture| {
        if let PagePicture::Texture { slot, fence } = picture {
            view.lease_sub(slot);
            seen.lock().unwrap().push(fence);
            count.fetch_add(1, Ordering::Relaxed);
        }
    })
    .unwrap();
    (sup, pictures, fences)
}

fn assert_no_lease(ring: &FrameRing) {
    for slot in 0..SLOTS {
        assert_eq!(ring.lease(slot), 0, "slot {slot} still leased");
    }
}

/// The shared-memory helper aborts in the middle of a run. The host stays up, the
/// exit counts toward back-off, and the last copied frame is still the studio's.
#[test]
fn a_helper_that_crashes_mid_run_is_an_exit_and_its_last_frame_stays() {
    let frames = Arc::new(AtomicU32::new(0));
    let seen = Arc::clone(&frames);
    let mut sup = Supervisor::start(
        spawn(&["--hold", "--crash-after=4"]),
        60,
        move |px, w, h, _, _| {
            assert_eq!(px.len(), (w * h * 4) as usize);
            seen.fetch_add(1, Ordering::Relaxed);
        },
    )
    .unwrap();
    until("exit", || sup.exited());
    assert_eq!(sup.exit_cause(), Cause::Other);
    assert!(
        frames.load(Ordering::Relaxed) >= 1,
        "frames before the crash"
    );
    let child = sup.stop().unwrap();
    assert!(child.unavailable);
    assert_eq!(child.attempts, 1);
    assert_eq!(child.retry_after, Duration::from_millis(500));
    assert_ne!(child.exit_code, Some(0), "not a clean exit");
    assert_eq!(child.frame.len(), 4 * 2 * 4, "the last frame is kept");
    assert_eq!(child.frame[3], 255);
}

/// The helper stops painting and beating in the middle of a run but stays alive (a
/// page stuck in script). On the studio's clock the heartbeat limit stops it; the
/// frame it painted last is kept.
#[test]
fn a_helper_that_freezes_mid_run_is_stopped_and_its_last_frame_stays() {
    let mut child = spawn(&["--hold", "--freeze-after=4"]);
    until("four frames", || {
        child.refresh();
        child.frames_written() >= 4
    });
    child.refresh();
    let kept = child.frame.clone();
    assert_eq!(kept.len(), 4 * 2 * 4);
    // The studio's clock moves a heartbeat limit per check. A beat that still lands
    // after the last paint resets it once; then the helper is silent.
    let mut now = Instant::now();
    let start = Instant::now();
    while child.check_heartbeat(now) {
        assert!(child.process_alive(), "alive until the studio stops it");
        assert!(start.elapsed() < GUARD, "hang guard");
        now += HEARTBEAT_LIMIT;
        std::thread::yield_now();
    }
    assert!(child.unavailable);
    assert_eq!(child.attempts, 1);
    assert!(!child.process_alive(), "the hung helper was stopped");
    assert_eq!(
        child.frames_written(),
        4,
        "nothing painted after the freeze"
    );
    assert_eq!(child.frame, kept, "the last frame is kept");
}

/// The GPU helper dies while it holds `WRITING` on a slot. The host stays up, the
/// slot it was writing is never handed out, every studio lease comes back, and its
/// last finished picture can still be taken (a page shown again).
#[test]
fn a_gpu_helper_that_dies_while_writing_leaves_no_lease_and_its_last_picture() {
    let fence = Arc::new(AtomicU64::new(DONE));
    let child = spawn_gpu(&["--hold", "--crash-after=6"], &fence);
    let view = FrameRing::open(child.frame_name(), 4, 2).unwrap();
    let (mut sup, pictures, _) = supervise_gpu(child);
    until("exit", || sup.exited());
    assert_eq!(sup.exit_cause(), Cause::Other, "a crash, not a fallback");
    assert!(pictures.load(Ordering::Relaxed) >= 1);
    let child = sup.stop().unwrap();
    assert!(child.unavailable);
    assert!(!child.gpu_failed());
    let stuck = writing_slots(&view);
    assert_eq!(stuck.len(), 1, "it died holding one slot");
    assert_no_lease(&view);
    let last = view.lease_latest_gpu().expect("the last picture stays");
    assert_ne!(last.slot, stuck[0], "the slot being written is never shown");
    view.lease_sub(last.slot);
}

/// Slots the helper left marked `WRITING`, probed as a helper would: every slot
/// but the latest that a claim does not get, and that no studio lease holds.
fn writing_slots(view: &FrameRing) -> Vec<u32> {
    let mut free = Vec::new();
    while let Some(s) = view.claim_gpu() {
        free.push(s);
    }
    for s in &free {
        view.abandon_gpu(*s);
    }
    let latest = view.lease_latest_gpu();
    if let Some(p) = latest {
        view.lease_sub(p.slot);
    }
    (0..SLOTS)
        .filter(|s| !free.contains(s) && latest.is_none_or(|p| p.slot != *s))
        .filter(|&s| view.lease(s) == 0)
        .collect()
}

/// The helper keeps publishing and beating, but its fence never arrives (a GPU that
/// hangs while the helper's CPU side goes on). After 30 page frames on the
/// supervisor's clock the tree falls back to shared memory, with every lease back.
#[test]
fn a_fence_that_never_arrives_falls_back_and_gives_every_lease_back() {
    let fence = Arc::new(AtomicU64::new(0));
    let child = spawn_gpu(&["--hold"], &fence);
    let view = FrameRing::open(child.frame_name(), 4, 2).unwrap();
    let (mut sup, pictures, _) = supervise_gpu(child);
    until("fallback", || sup.exited());
    assert_eq!(sup.exit_cause(), Cause::GpuFallback);
    assert_eq!(
        pictures.load(Ordering::Relaxed),
        0,
        "nothing unfinished went out"
    );
    let child = sup.stop().unwrap();
    assert!(child.gpu_failed());
    assert_no_lease(&view);
}

/// The helper publishes every picture with fence `u64::MAX`, which no plausible
/// completed value reaches: no picture goes out, and the tree falls back.
#[test]
fn a_helper_that_publishes_fence_max_falls_back_without_showing_it() {
    let fence = Arc::new(AtomicU64::new(DONE));
    let child = spawn_gpu(&["--hold", "--fence=18446744073709551615"], &fence);
    let view = FrameRing::open(child.frame_name(), 4, 2).unwrap();
    let (mut sup, pictures, _) = supervise_gpu(child);
    until("fallback", || sup.exited());
    assert_eq!(sup.exit_cause(), Cause::GpuFallback);
    assert_eq!(pictures.load(Ordering::Relaxed), 0);
    sup.stop().unwrap();
    assert_no_lease(&view);
}

/// The helper reuses one fence value for every picture (it should increase). The
/// studio cannot tell a finished copy from an unfinished one then: the pictures go
/// out and at worst show that page torn or stale (ADR-0157 decision 2), the tree
/// keeps running, and every lease comes back.
#[test]
fn a_helper_that_reuses_a_fence_value_costs_only_its_own_picture() {
    let fence = Arc::new(AtomicU64::new(DONE));
    let child = spawn_gpu(&["--hold", "--fence=1"], &fence);
    let view = FrameRing::open(child.frame_name(), 4, 2).unwrap();
    let (mut sup, pictures, fences) = supervise_gpu(child);
    until("pictures", || pictures.load(Ordering::Relaxed) >= 5);
    assert!(!sup.exited(), "a reused value is not a failure");
    assert!(fences.lock().unwrap().iter().all(|&f| f == 1));
    let child = sup.stop().unwrap();
    assert!(!child.gpu_failed());
    assert_no_lease(&view);
}

/// The device behind the fence is removed while pictures flow: the completed value
/// reads `u64::MAX` and the tree falls back at once.
#[test]
fn a_removed_device_mid_run_falls_back_and_gives_every_lease_back() {
    let fence = Arc::new(AtomicU64::new(DONE));
    let child = spawn_gpu(&["--hold"], &fence);
    let view = FrameRing::open(child.frame_name(), 4, 2).unwrap();
    let (mut sup, pictures, _) = supervise_gpu(child);
    until("pictures", || pictures.load(Ordering::Relaxed) >= 3);
    fence.store(u64::MAX, Ordering::Relaxed);
    until("fallback", || sup.exited());
    assert_eq!(sup.exit_cause(), Cause::GpuFallback);
    sup.stop().unwrap();
    assert_no_lease(&view);
}

/// A handover the test drives: what `poll` answers, and whether `close` ran.
#[derive(Debug)]
struct TestHandover(Arc<Mutex<(HandoverPoll, bool)>>);

impl SurfaceHandover for TestHandover {
    fn poll(&mut self) -> HandoverPoll {
        self.0.lock().unwrap().0
    }
    fn close(&mut self) {
        self.0.lock().unwrap().1 = true;
    }
}

fn spawn_with_handover(state: &Arc<Mutex<(HandoverPoll, bool)>>) -> HelperChild {
    let fence = Arc::new(AtomicU64::new(DONE));
    let mut child = spawn_gpu(&[], &fence);
    child.set_handover(Box::new(TestHandover(Arc::clone(state))));
    let start = Instant::now();
    // The deadline starts only once the helper has beaten.
    until("first heartbeat", || {
        child.check_heartbeat(Instant::now()) && child.has_beaten()
    });
    assert!(start.elapsed() < GUARD);
    child
}

/// A helper that never takes the surfaces falls back with `open` exactly
/// [`HANDOVER_LIMIT`] after the first pass that saw its heartbeat, and the offer is
/// withdrawn (ADR-0167 decision 3). Times are the test's, not waited for.
#[test]
fn a_handover_that_is_never_taken_falls_back_after_its_limit() {
    let state = Arc::new(Mutex::new((HandoverPoll::Waiting, false)));
    let mut child = spawn_with_handover(&state);
    let t0 = Instant::now();
    let _ = child.take_gpu(t0);
    let _ = child.take_gpu(t0 + HANDOVER_LIMIT - Duration::from_millis(1));
    assert!(!child.gpu_failed(), "failed before the limit");
    assert!(!state.lock().unwrap().1);
    let _ = child.take_gpu(t0 + HANDOVER_LIMIT);
    assert!(child.gpu_failed(), "no fallback at the limit");
    assert!(state.lock().unwrap().1, "the offer was not withdrawn");
    child.close(Duration::ZERO);
}

/// A finished handover stops being polled and never fails the page.
#[test]
fn a_finished_handover_never_fails_the_page() {
    let state = Arc::new(Mutex::new((HandoverPoll::Done, false)));
    let mut child = spawn_with_handover(&state);
    let t0 = Instant::now();
    let _ = child.take_gpu(t0);
    // Even a later "failed" answer is never read again.
    state.lock().unwrap().0 = HandoverPoll::Failed;
    let _ = child.take_gpu(t0 + HANDOVER_LIMIT * 2);
    assert!(!child.gpu_failed());
    child.close(Duration::ZERO);
}

/// A handover that fails falls back at once.
#[test]
fn a_failed_handover_falls_back_at_once() {
    let state = Arc::new(Mutex::new((HandoverPoll::Failed, false)));
    let mut child = spawn_with_handover(&state);
    let _ = child.take_gpu(Instant::now());
    assert!(child.gpu_failed());
    child.close(Duration::ZERO);
}
