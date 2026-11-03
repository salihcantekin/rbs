//! Windows: MMCSS for periodic threads (the "Games" task at high priority, which the
//! Multimedia Class Scheduler keeps above normal threads within its CPU budget), an
//! above-normal priority for responsive ones, and 1 ms timer resolution for both so
//! `park_timeout` does not round up to the 15.6 ms default tick.
//!
//! Side effects (ADR-0030): while any thread is registered with MMCSS, Windows applies
//! `NetworkThrottlingIndex` to non-multimedia network traffic system-wide (10 packets
//! per ms by default); a thread that uses up the task's CPU share drops to low priority
//! for the rest of the period; Windows 11 may ignore `timeBeginPeriod` for a process
//! without a visible window, so a headless engine can keep the coarse timer.

use rbs_core::thread::{ThreadClass, ThreadGrant, ThreadGuard, ThreadPolicyError, ThreadRequest};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Threading::{
    AVRT_PRIORITY_HIGH, AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
    AvSetMmThreadPriority, GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY,
    THREAD_PRIORITY_ABOVE_NORMAL,
};
use windows::core::w;

use crate::OsThreadConfig;

/// `TIMERR_NOERROR`.
const TIMERR_NOERROR: u32 = 0;
/// `THREAD_PRIORITY_ERROR_RETURN`.
const THREAD_PRIORITY_ERROR_RETURN: i32 = 0x7FFF_FFFF;

type Undo = Box<dyn FnOnce()>;

pub(crate) fn enter(
    config: &OsThreadConfig,
    request: &ThreadRequest<'_>,
) -> Result<ThreadGuard, ThreadPolicyError> {
    let mut undo: Vec<Undo> = Vec::new();
    let mut grant = ThreadGrant::Unchanged;
    // SAFETY: plain value argument. The period is reference-counted by the system and
    // undone below with the same value.
    if unsafe { timeBeginPeriod(1) } == TIMERR_NOERROR {
        undo.push(Box::new(|| {
            // SAFETY: pairs the successful `timeBeginPeriod(1)` above.
            let _ = unsafe { timeEndPeriod(1) };
        }));
        grant = ThreadGrant::Raised;
    }
    let periodic = matches!(request.class, ThreadClass::Periodic { .. });
    let mmcss = if periodic && config.realtime {
        mmcss()
    } else {
        None
    };
    match mmcss {
        Some(restore) => {
            undo.push(restore);
            grant = ThreadGrant::RealTime;
        }
        None => {
            if let Some(restore) = above_normal() {
                undo.push(restore);
                grant = ThreadGrant::Raised;
            }
        }
    }
    if grant == ThreadGrant::Unchanged {
        return Err(ThreadPolicyError::Denied(
            "no MMCSS task, priority or timer resolution",
        ));
    }
    Ok(ThreadGuard::restoring(grant, move || {
        for undo in undo.into_iter().rev() {
            undo();
        }
    }))
}

fn mmcss() -> Option<Undo> {
    let mut index = 0u32;
    // SAFETY: the task name is a static, NUL-terminated wide string; `index` is an
    // initialised local that outlives the call. Registers the calling thread.
    let task = unsafe { AvSetMmThreadCharacteristicsW(w!("Games"), &mut index) }.ok()?;
    // SAFETY: `task` is the handle just returned for this thread.
    let _ = unsafe { AvSetMmThreadPriority(task, AVRT_PRIORITY_HIGH) };
    Some(Box::new(move || {
        // SAFETY: runs on the same thread (the guard is not `Send`) with the handle
        // `AvSetMmThreadCharacteristicsW` returned for it, once.
        let _ = unsafe { AvRevertMmThreadCharacteristics(task) };
    }))
}

fn above_normal() -> Option<Undo> {
    // SAFETY: `GetCurrentThread` returns a pseudo-handle for the calling thread; it
    // needs no closing and is valid on that thread.
    let me = unsafe { GetCurrentThread() };
    // SAFETY: `me` is the calling thread's pseudo-handle.
    let old = unsafe { GetThreadPriority(me) };
    if old == THREAD_PRIORITY_ERROR_RETURN {
        return None;
    }
    // SAFETY: as above.
    unsafe { SetThreadPriority(me, THREAD_PRIORITY_ABOVE_NORMAL) }.ok()?;
    Some(Box::new(move || {
        // SAFETY: runs on the same thread; `GetCurrentThread` is its pseudo-handle.
        let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY(old)) };
    }))
}
