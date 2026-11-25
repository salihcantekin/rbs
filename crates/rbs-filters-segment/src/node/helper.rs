//! The readback helper thread (child of `node`).
//!
//! The small picture is copied to a staging buffer on the GPU queue. Waiting for that copy
//! on the render thread meant the picture reached the session one tick late (the render
//! thread looks at the buffer once per tick, about 33 ms at 30 fps). This thread waits for
//! each copy instead, reads the bytes and lends them to the session at once, so the
//! session starts about a tick earlier and its matte is ready for the next tick.
//! The thread exists from `prepare` to `release` (a disabled filter has none, P13) and
//! sleeps in `recv` and in the device wait; the render thread never waits for it.

use std::sync::mpsc;
use std::time::Duration;

use super::*;

/// One staging buffer. `state` is written by the map callback too.
#[derive(Clone)]
pub(super) struct SlotView {
    pub(super) buffer: wgpu::Buffer,
    pub(super) state: Arc<AtomicU8>,
}

/// The session, shared between the render thread (`latest`, `reset`) and the helper
/// (`submit`); `None` while the node is cold.
pub(super) type SharedSession = Arc<Mutex<Option<Box<dyn PersonMaskSession>>>>;

/// A readback to finish: the staging slot, the picture's time and the GPU submission.
pub(super) struct Job {
    pub(super) slot: usize,
    pub(super) time: Timestamp,
    pub(super) submission: wgpu::SubmissionIndex,
}

/// What the helper needs besides the jobs.
pub(super) struct Setup {
    pub(super) device: wgpu::Device,
    pub(super) slots: Vec<SlotView>,
    pub(super) stride: u32,
    pub(super) small: [u32; 2],
    pub(super) session: SharedSession,
    pub(super) stats: Arc<RemovalStats>,
}

/// The helper thread's handle. Dropping it ends the thread.
pub(super) struct Readback {
    sender: Option<mpsc::SyncSender<Job>>,
    stop: Arc<AtomicBool>,
}

impl Readback {
    /// Starts the helper.
    ///
    /// # Errors
    /// The thread could not be started.
    pub(super) fn start(setup: Setup) -> Result<Self, NodeError> {
        // Bounded by the staging ring (a job exists only for a busy slot), so `try_send`
        // never allocates on the render thread.
        let (sender, receiver) = mpsc::sync_channel::<Job>(RING);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("rbs-segment-readback".into())
            .spawn(move || run(&setup, &receiver, &flag))
            .map_err(|_| NodeError::Failed("readback thread".into()))?;
        Ok(Self {
            sender: Some(sender),
            stop,
        })
    }

    /// Hands a queued readback over. Never blocks.
    pub(super) fn send(&self, job: Job) {
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(job);
        }
    }
}

impl Drop for Readback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.sender = None;
    }
}

fn run(setup: &Setup, jobs: &mpsc::Receiver<Job>, stop: &AtomicBool) {
    let mut pixels = BytesMut::with_capacity(setup.small[0] as usize * setup.small[1] as usize * 4);
    while let Ok(job) = jobs.recv() {
        let slot = &setup.slots[job.slot];
        // Wait until this copy is done and its map callback ran.
        while slot.state.load(Ordering::Acquire) == MAPPING {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let _ = setup.device.poll(wgpu::PollType::Wait {
                submission_index: Some(job.submission.clone()),
                timeout: Some(Duration::from_millis(100)),
            });
        }
        if slot.state.load(Ordering::Acquire) != MAPPED {
            setup.stats.failed.fetch_add(1, Ordering::Relaxed);
            slot.state.store(IDLE, Ordering::Release);
            continue;
        }
        let row = setup.small[0] as usize * 4;
        pixels.clear();
        if let Ok(view) = slot.buffer.slice(..).get_mapped_range() {
            for y in 0..setup.small[1] as usize {
                let start = y * setup.stride as usize;
                pixels.extend_from_slice(&view[start..start + row]);
            }
        }
        slot.buffer.unmap();
        slot.state.store(IDLE, Ordering::Release);
        let bytes = std::mem::take(&mut pixels).freeze();
        lend(setup, job.time, &bytes, row);
        // Back to the helper's buffer when the session did not keep a reference.
        pixels = bytes.try_into_mut().unwrap_or_default();
    }
}

/// Lends the session the small picture as a system frame.
fn lend(setup: &Setup, time: Timestamp, bytes: &Bytes, row: usize) {
    let format = VideoFormat::new(
        Size::new(setup.small[0], setup.small[1]),
        PixelFormat::Bgra8,
        ColorSpace::SRGB,
    );
    let planes = [PlaneLayout {
        offset: 0,
        stride: row,
    }];
    let Ok(system) = SystemFrame::new(format, bytes.clone(), &planes) else {
        setup.stats.failed.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let frame = VideoFrame::system(time, system, None);
    let Ok(mut guard) = setup.session.lock() else {
        return;
    };
    let Some(session) = guard.as_mut() else {
        return;
    };
    match session.submit(&frame) {
        SubmitOutcome::Accepted => setup.stats.submitted.fetch_add(1, Ordering::Relaxed),
        _ => setup.stats.dropped_busy.fetch_add(1, Ordering::Relaxed),
    };
}
