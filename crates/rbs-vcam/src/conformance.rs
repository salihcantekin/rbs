//! The conformance suite every virtual camera adapter passes (spec, "Conformance tests").
//!
//! An adapter's test calls [`run`] with its factory and a [`Probe`] into its consumer side
//! (a test reader of its ring, for example). The checks never sleep for timing and never
//! assert durations (ADR-0032): they wait for conditions with a bounded number of yields,
//! which is only a hang guard.

use std::collections::BTreeSet;

use rbs_core::media::VideoFrame;

use crate::error::VcamError;
use crate::session::{
    CameraFormat, InstallState, VirtualCameraConfig, VirtualCameraFactory, VirtualCameraSession,
};

/// Yields before a wait gives up (a hang guard, not a timing assumption).
const PATIENCE: u64 = 50_000_000;

/// The adapter's view into its consumer side.
pub trait Probe {
    /// Makes the consumer side stop taking frames (the sender thread holds what it has).
    fn stall(&self);
    /// Lets it take frames again.
    fn resume(&self);
    /// Sequence numbers (`VideoFrame::sequence`) of the frames that reached the
    /// consumer side, in order.
    fn delivered(&self) -> Vec<u64>;
}

/// Counts allocations plus frees made by the closure on this thread.
pub type MeasureAllocations = fn(&mut dyn FnMut()) -> u64;
/// Lists this process's thread ids.
pub type ThreadIds = fn() -> Option<Vec<u64>>;
type Check = fn(&dyn VirtualCameraFactory, &dyn Probe, &Options<'_>) -> Result<(), Failure>;

/// What [`run`] needs.
pub struct Options<'a> {
    /// The configuration to open sessions with.
    pub config: VirtualCameraConfig,
    /// Another format the camera can announce (for `set_format`).
    pub other_format: CameraFormat,
    /// Makes a frame for `config` with this sequence number.
    pub frame: &'a dyn Fn(u64) -> VideoFrame,
    /// Allocations plus frees made by the closure on this thread, if the test binary
    /// can count them.
    pub measure_allocations: Option<MeasureAllocations>,
    /// This process's thread ids, if the platform can list them and the binary runs
    /// this suite alone.
    pub process_threads: Option<ThreadIds>,
}

/// A failed check: its name and what went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The check.
    pub check: &'static str,
    /// What went wrong.
    pub detail: String,
}

fn fail(check: &'static str, detail: impl Into<String>) -> Failure {
    Failure {
        check,
        detail: detail.into(),
    }
}

fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..PATIENCE {
        if cond() {
            return true;
        }
        std::thread::yield_now();
    }
    false
}

fn open(
    factory: &dyn VirtualCameraFactory,
    config: &VirtualCameraConfig,
    check: &'static str,
) -> Result<Box<dyn VirtualCameraSession>, Failure> {
    factory
        .open(config)
        .map_err(|e| fail(check, format!("open: {e}")))
}

/// Runs every check; returns the failures (empty when the adapter conforms).
pub fn run(factory: &dyn VirtualCameraFactory, probe: &dyn Probe, o: &Options<'_>) -> Vec<Failure> {
    let checks: [Check; 8] = [
        install_and_uninstall_are_idempotent,
        open_needs_the_camera,
        send_never_blocks_with_a_stalled_consumer,
        full_session_reserves_nothing,
        dropped_token_frees_its_slot,
        newest_frame_wins,
        refuses_frames_that_break_the_config,
        no_threads_before_open_or_after_drop,
    ];
    let mut failures: Vec<Failure> = checks
        .iter()
        .filter_map(|c| {
            probe.resume();
            c(factory, probe, o).err()
        })
        .collect();
    if let Err(f) = set_format_reports_pending_or_applied(factory, o) {
        failures.push(f);
    }
    failures
}

fn install_and_uninstall_are_idempotent(
    f: &dyn VirtualCameraFactory,
    _: &dyn Probe,
    _: &Options<'_>,
) -> Result<(), Failure> {
    const C: &str = "install_and_uninstall_are_idempotent";
    for _ in 0..2 {
        let s = f.install().map_err(|e| fail(C, format!("install: {e}")))?;
        if s != InstallState::Installed {
            return Err(fail(C, format!("install returned {s:?}")));
        }
        let now = f.status().map_err(|e| fail(C, format!("status: {e}")))?;
        if now != s {
            return Err(fail(C, format!("status {now:?} after install {s:?}")));
        }
    }
    for _ in 0..2 {
        let s = f
            .uninstall()
            .map_err(|e| fail(C, format!("uninstall: {e}")))?;
        let now = f.status().map_err(|e| fail(C, format!("status: {e}")))?;
        if now != s || s != InstallState::NotInstalled {
            return Err(fail(C, format!("uninstall {s:?}, status {now:?}")));
        }
    }
    f.install().map_err(|e| fail(C, format!("install: {e}")))?;
    Ok(())
}

fn open_needs_the_camera(
    f: &dyn VirtualCameraFactory,
    _: &dyn Probe,
    o: &Options<'_>,
) -> Result<(), Failure> {
    const C: &str = "open_needs_the_camera";
    f.uninstall().map_err(|e| fail(C, e.to_string()))?;
    let refused = matches!(f.open(&o.config), Err(VcamError::NotInstalled));
    f.install().map_err(|e| fail(C, e.to_string()))?;
    if refused {
        Ok(())
    } else {
        Err(fail(C, "open did not return NotInstalled"))
    }
}

fn send_never_blocks_with_a_stalled_consumer(
    f: &dyn VirtualCameraFactory,
    p: &dyn Probe,
    o: &Options<'_>,
) -> Result<(), Failure> {
    const C: &str = "send_never_blocks_with_a_stalled_consumer";
    let s = open(f, &o.config, C)?;
    p.stall();
    // Twice the slots: every call returns (or this check would hang, which the test
    // harness reports), and the extra ones find no slot.
    let mut sent = 0;
    let mut frames: Vec<VideoFrame> = (1..=6).map(|n| (o.frame)(n)).collect();
    frames.reverse();
    let mut call = || {
        for _ in 0..6 {
            if let Some(slot) = s.reserve()
                && let Some(frame) = frames.pop()
                && s.send(slot, frame).is_ok()
            {
                sent += 1;
            }
        }
    };
    if let Some(measure) = o.measure_allocations {
        // Warm-up happened in `open`; the frames were made before measuring.
        let n = measure(&mut call);
        if n != 0 {
            return Err(fail(C, format!("{n} allocations in reserve/send")));
        }
    } else {
        call();
    }
    if sent == 0 {
        return Err(fail(C, "no frame was taken"));
    }
    p.resume();
    Ok(())
}

fn full_session_reserves_nothing(
    f: &dyn VirtualCameraFactory,
    p: &dyn Probe,
    o: &Options<'_>,
) -> Result<(), Failure> {
    const C: &str = "full_session_reserves_nothing";
    let s = open(f, &o.config, C)?;
    p.stall();
    let held: Vec<_> = std::iter::from_fn(|| s.reserve()).take(16).collect();
    if held.is_empty() || held.len() > 8 {
        return Err(fail(C, format!("{} slots reserved", held.len())));
    }
    let before = s.stats().consumer_busy();
    if s.reserve().is_some() || s.stats().consumer_busy() != before + 1 {
        return Err(fail(
            C,
            "a full session reserved, or did not count consumer_busy",
        ));
    }
    Ok(())
}

fn dropped_token_frees_its_slot(
    f: &dyn VirtualCameraFactory,
    p: &dyn Probe,
    o: &Options<'_>,
) -> Result<(), Failure> {
    const C: &str = "dropped_token_frees_its_slot";
    let s = open(f, &o.config, C)?;
    p.stall();
    let held: Vec<_> = std::iter::from_fn(|| s.reserve()).take(16).collect();
    let n = held.len();
    drop(held);
    let again: Vec<_> = std::iter::from_fn(|| s.reserve()).take(16).collect();
    if again.len() != n {
        return Err(fail(C, format!("{n} slots before, {} after", again.len())));
    }
    Ok(())
}

fn newest_frame_wins(
    f: &dyn VirtualCameraFactory,
    p: &dyn Probe,
    o: &Options<'_>,
) -> Result<(), Failure> {
    const C: &str = "newest_frame_wins";
    let base = p.delivered().len();
    let s = open(f, &o.config, C)?;
    p.stall();
    let mut last = 0;
    for n in 1..=3 {
        let Some(slot) = s.reserve() else { break };
        s.send(slot, (o.frame)(1000 + n))
            .map_err(|e| fail(C, format!("send: {e}")))?;
        last = 1000 + n;
    }
    p.resume();
    if !wait_for(|| p.delivered().contains(&last)) {
        return Err(fail(C, format!("frame {last} never delivered")));
    }
    // The sender may have taken an earlier frame before the stall took hold, never one
    // after it took a later one.
    let got: Vec<u64> = p.delivered()[base..].to_vec();
    if got.windows(2).any(|w| w[0] >= w[1]) {
        return Err(fail(C, format!("delivered out of order: {got:?}")));
    }
    let stats = s.stats();
    if stats.replaced() + got.len() as u64 != 3 {
        return Err(fail(
            C,
            format!(
                "{} delivered + {} replaced != 3",
                got.len(),
                stats.replaced()
            ),
        ));
    }
    Ok(())
}

fn refuses_frames_that_break_the_config(
    f: &dyn VirtualCameraFactory,
    _: &dyn Probe,
    o: &Options<'_>,
) -> Result<(), Failure> {
    const C: &str = "refuses_frames_that_break_the_config";
    let s = open(f, &o.config, C)?;
    let other = open(f, &o.config, C)?;
    let Some(foreign) = other.reserve() else {
        return Err(fail(C, "no slot"));
    };
    if !matches!(
        s.send(foreign, (o.frame)(1)),
        Err(VcamError::InvalidFrame(_))
    ) {
        return Err(fail(C, "a slot of another session was accepted"));
    }
    if s.stats().refused() != 1 {
        return Err(fail(C, "refused frame not counted"));
    }
    Ok(())
}

fn no_threads_before_open_or_after_drop(
    f: &dyn VirtualCameraFactory,
    _: &dyn Probe,
    o: &Options<'_>,
) -> Result<(), Failure> {
    const C: &str = "no_threads_before_open_or_after_drop";
    let Some(threads) = o.process_threads else {
        return Ok(());
    };
    let ids = || threads().map(|v| v.into_iter().collect::<BTreeSet<u64>>());
    let Some(before) = ids() else { return Ok(()) };
    let s = open(f, &o.config, C)?;
    drop(s);
    // A joined thread may linger in the OS list for a moment after `join` returns.
    if !wait_for(|| ids().is_some_and(|now| now.is_subset(&before))) {
        return Err(fail(C, "a thread outlived the session"));
    }
    Ok(())
}

fn set_format_reports_pending_or_applied(
    f: &dyn VirtualCameraFactory,
    o: &Options<'_>,
) -> Result<(), Failure> {
    const C: &str = "set_format_applies_when_no_app_reads";
    let applied = f
        .set_format(&o.other_format)
        .map_err(|e| fail(C, e.to_string()))?;
    f.set_format(&o.config.format)
        .map_err(|e| fail(C, e.to_string()))?;
    if !applied {
        return Err(fail(C, "pending with no app reading the camera"));
    }
    Ok(())
}
