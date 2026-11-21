//! The playback of one media source, shared by its picture node and sound node
//! (ADR-0106 decision 1, ADR-0108 decision 5).
//!
//! Either node starts playback when it is prepared and the other is not, so a source
//! that is only heard (its sound node reaches a sink, its picture node is not
//! scheduled) still plays. The worker stops when neither node is prepared. The player
//! sits behind a mutex in [`super::Shared`] that only the control path takes: `process`
//! never touches it.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rbs_audio::AudioGraphConfig;
use rbs_core::time::Clock;
use rbs_graph::runtime::RestartPolicy;
use rbs_media_decode::MediaDecoderFactory;

use super::Shared;
use super::control::Record;
use super::picture::{MediaNodeError, MediaPhase};
use super::return_edge::Resumed;
use super::worker::{self, Job, Pending, Progress};

/// The picture rings of one worker run.
pub(crate) struct Rings {
    pub(crate) pictures: rtrb::Consumer<Pending>,
    pub(crate) returns: rtrb::Producer<Pending>,
}

struct Worker {
    join: JoinHandle<()>,
    shutdown: Arc<AtomicBool>,
}

/// Which node uses the player.
#[derive(Clone, Copy)]
pub(crate) enum User {
    Picture,
    Sound,
}

pub(crate) struct Player {
    factory: Arc<dyn MediaDecoderFactory>,
    path: std::path::PathBuf,
    clock: Arc<dyn Clock>,
    config: AudioGraphConfig,
    pub(crate) loop_flag: Arc<AtomicBool>,
    pub(crate) restart_when_shown: bool,
    /// `on_return: continue` (ADR-0152 decision 6); `restart_when_shown` is false.
    pub(crate) continues: bool,
    /// Where a `continue` film was when it left air playing.
    pub(crate) anchor: Option<super::return_edge::Anchor>,
    /// The source has been on air since it was created.
    pub(crate) aired: bool,
    pub(crate) phase: MediaPhase,
    worker: Option<Worker>,
    /// Joins a worker that was signalled to stop. The control path does not wait
    /// on it (ADR-0108 decision 7). The next start does, so two decoders of this
    /// source are never open together.
    reaper: Option<JoinHandle<()>>,
    pub(crate) ended: Arc<AtomicBool>,
    pub(crate) progress: Arc<Progress>,
    picture_in: bool,
    sound_in: bool,
    pub(crate) queue_allocations: u32,
    /// The source went to sleep from the end hold: it reports `Ended` until a
    /// command or a shown start leaves it.
    pub(crate) ended_stored: bool,
    pub(crate) attempts: u32,
    pub(crate) retry_after: Option<Duration>,
    /// A program or hold scene draws the source (the binder's on-air set, ADR-0107
    /// decision 6). True until the host says otherwise.
    pub(crate) on_air: bool,
    /// Play or Restart was pressed. Until then a file that goes on air stays
    /// stopped on its first picture (owner, 2026-11-20).
    pub(crate) started: bool,
    /// Stopped because only a preview scene draws the source: it plays when a program
    /// or hold scene draws it (ADR-0108 decision 10). Kept across Standby.
    pub(crate) cued: bool,
    /// The pause release let the worker and the decoder go while the source stays
    /// paused or cued; the next command that needs a picture starts a worker again.
    pub(crate) released: bool,
    /// How long a source stays paused or cued before the pause release
    /// (`pause_release_s`).
    pub(crate) pause_release: Duration,
    /// When the current worker started: the release timer counts from the later of
    /// this and the newest record's instant, so a source that wakes paused or cued
    /// keeps its worker for the full time.
    pub(crate) started_at: rbs_core::time::Timestamp,
    /// Armed as the stinger (ADR-0140 decision 4): the saved `loop` and
    /// return edge, given back when the transition ends.
    pub(crate) armed: Option<(bool, super::return_edge::ReturnEdge)>,
}

/// The default of the `pause_release_s` app setting (ADR-0108 decision 10).
pub const PAUSE_RELEASE: Duration = Duration::from_secs(30);

impl Player {
    pub(crate) fn new(
        factory: Arc<dyn MediaDecoderFactory>,
        path: std::path::PathBuf,
        loop_playback: bool,
        restart_when_shown: bool,
        clock: Arc<dyn Clock>,
        config: AudioGraphConfig,
    ) -> Self {
        Self {
            factory,
            path,
            clock,
            config,
            loop_flag: Arc::new(AtomicBool::new(loop_playback)),
            restart_when_shown,
            continues: false,
            anchor: None,
            aired: false,
            phase: MediaPhase::Standby,
            worker: None,
            reaper: None,
            ended: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(Progress::default()),
            picture_in: false,
            sound_in: false,
            queue_allocations: 0,
            ended_stored: false,
            attempts: 0,
            retry_after: None,
            on_air: true,
            started: true,
            cued: false,
            released: false,
            pause_release: PAUSE_RELEASE,
            started_at: rbs_core::time::Timestamp::ZERO,
            armed: None,
        }
    }

    pub(crate) fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Marks `user` as prepared. Starts a worker from Standby; a held player stays
    /// held (the end of the file is silent until the picture node wakes it again).
    pub(crate) fn acquire(
        &mut self,
        user: User,
        shared: &Arc<Shared>,
    ) -> Result<(), MediaNodeError> {
        match user {
            User::Picture => self.picture_in = true,
            User::Sound => self.sound_in = true,
        }
        if self.phase == MediaPhase::Standby {
            self.start_shown(shared)
        } else if self.released && matches!(user, User::Picture) {
            // A picture node that wakes after the pause release needs the picture at
            // the cursor: one is decoded, then the release timer runs again.
            self.start(shared)
        } else {
            Ok(())
        }
    }

    /// Starts a worker because the source is shown, following the transition table
    /// of ADR-0108 decision 10. Shown only in a preview scene, a source with no
    /// stored pause or end is cued at the range start (or at the stored cursor with
    /// `restart_when_shown` false) and a stored pause or end stays. On air,
    /// `restart_when_shown` moves the cursor to the start and makes a stored pause or
    /// end play; without it a stored pause or end stays and a cued source plays.
    pub(crate) fn start_shown(&mut self, shared: &Arc<Shared>) -> Result<(), MediaNodeError> {
        let newest = shared.controls.newest();
        // A take of a cued file still starts. A file that was never started and
        // was not waiting in preview stays on its first picture.
        if self.on_air && !self.started && !self.cued {
            // Added to a scene: show the first picture and wait for Play.
            if self.restart_when_shown {
                shared.move_cursor(shared.range_start());
            }
            if !newest.paused {
                self.write_now(shared, true);
            }
            return self.start(shared);
        }
        if !self.on_air {
            let stored = self.ended_stored || (newest.paused && !self.cued);
            if !stored {
                self.cued = true;
                if self.restart_when_shown {
                    shared.move_cursor(shared.range_start());
                }
            }
            if !newest.paused {
                self.write_now(shared, true);
            }
            return self.start(shared);
        }
        let was_cued = std::mem::replace(&mut self.cued, false);
        if let Some(resumed) = self.continue_at(shared, self.clock.now()) {
            // `continue`: where it would be had it kept playing (ADR-0152 decision 6).
            let (position, ended) = match resumed {
                Resumed::At(position) => (position, false),
                Resumed::Ended(position) => (position, true),
            };
            self.ended_stored = ended;
            shared.move_cursor(position);
            if newest.paused != ended {
                self.write_now(shared, ended);
            }
        } else if self.restart_when_shown {
            self.ended_stored = false;
            shared.move_cursor(shared.range_start());
            if newest.paused {
                self.write_now(shared, false);
            }
        } else if self.ended_stored {
            // A stored end stays ended (the table): the last picture is shown, paused.
            if !newest.paused {
                self.write_now(shared, true);
            }
        } else if was_cued && newest.paused {
            self.write_now(shared, false);
        }
        self.start(shared)
    }

    /// Writes a record that applies from now: the worker that starts next follows it.
    fn write_now(&self, shared: &Shared, paused: bool) {
        let newest = shared.controls.newest();
        shared.controls.write(Record {
            generation: newest.generation.wrapping_add(1),
            instant: self.clock.now(),
            paused,
            position: None,
        });
    }

    /// True while a node is prepared.
    pub(crate) fn in_use(&self) -> bool {
        self.picture_in || self.sound_in
    }

    /// Wakes a parked worker so it reads a new control record at once.
    pub(crate) fn unpark(&self) {
        if let Some(worker) = &self.worker {
            worker.join.thread().unpark();
        }
    }

    /// Marks `user` as released. With neither node prepared, stops the worker and
    /// frees the decoder and the rings.
    pub(crate) fn release(&mut self, user: User, shared: &Shared) {
        match user {
            User::Picture => self.picture_in = false,
            User::Sound => self.sound_in = false,
        }
        if !self.picture_in && !self.sound_in {
            self.stop(shared);
        }
    }

    /// Opens the file, seeks to the cursor, and starts a worker that follows the
    /// newest control record (paused or playing). Control thread only. Blocks until
    /// the worker holds its first picture.
    pub(crate) fn start(&mut self, shared: &Arc<Shared>) -> Result<(), MediaNodeError> {
        // The previous decoder must be gone before this one opens: it still writes
        // the cursor and the worker-thread slot until it returns.
        self.detach_worker();
        self.wait_reaped();
        let was_released = std::mem::replace(&mut self.released, false);
        shared.offer_rings(None);
        self.phase = MediaPhase::Standby;
        let cursor = shared.cursor();
        self.ended = Arc::new(AtomicBool::new(false));
        self.progress = Arc::new(Progress::default());
        // A new epoch: sound blocks a previous worker left in the ring are skipped.
        let epoch = shared.epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1);

        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let job = Job {
            factory: Arc::clone(&self.factory),
            path: self.path.clone(),
            cursor,
            clock: Arc::clone(&self.clock),
            config: self.config,
            loop_flag: Arc::clone(&self.loop_flag),
            shutdown: Arc::clone(&shutdown),
            shared: Arc::clone(shared),
            epoch,
            progress: Arc::clone(&self.progress),
            ready: tx,
        };
        let ended = Arc::clone(&self.ended);
        let progress = Arc::clone(&self.progress);
        let join = thread::Builder::new()
            .name("rbs-media".into())
            .spawn(move || {
                if std::panic::catch_unwind(AssertUnwindSafe(|| worker::run(job))).is_err() {
                    progress.failed.store(true, Ordering::Release);
                }
                ended.store(true, Ordering::Release);
            })
            .map_err(|error| MediaNodeError::Failed(error.to_string()))?;

        let ready = rx.recv().unwrap_or(Err(MediaNodeError::Failed(
            "media worker stopped before it queued a picture".into(),
        )));
        match ready {
            Ok(ready) => {
                shared.offer_rings(Some((
                    Rings {
                        pictures: ready.pictures,
                        returns: ready.returns,
                    },
                    Arc::clone(&self.progress),
                )));
                self.queue_allocations = self.queue_allocations.saturating_add(1);
                self.worker = Some(Worker { join, shutdown });
                self.phase = MediaPhase::Playing;
                self.started_at = self.clock.now();
                Ok(())
            }
            Err(error) => {
                shutdown.store(true, Ordering::Release);
                join.thread().unpark();
                let _ = join.join();
                if was_released {
                    // The file could not be opened again after the pause release (it
                    // moved, its drive left): the nodes stay prepared and keep the last
                    // picture, the source reports `Unavailable`, and the next command
                    // tries again. The picture node's `service` holds the picture and
                    // counts the attempt.
                    self.progress.failed.store(true, Ordering::Release);
                    self.released = true;
                    self.phase = MediaPhase::Playing;
                } else {
                    self.phase = MediaPhase::Standby;
                }
                Err(error)
            }
        }
    }

    /// Joins the worker after it ended and moves to Hold. A fault counts towards the
    /// back-off.
    pub(crate) fn hold(&mut self, shared: &Shared) {
        let failed = self.progress.failed.load(Ordering::Acquire);
        self.join_worker();
        shared.offer_rings(None);
        if failed {
            self.attempts = self.attempts.saturating_add(1);
            self.retry_after = Some(RestartPolicy::default().delay(self.attempts));
        }
        if self.phase == MediaPhase::Playing {
            self.phase = MediaPhase::Hold;
        }
    }

    /// Stops the worker and frees the decoder and the rings.
    pub(crate) fn stop(&mut self, shared: &Shared) {
        let failed = self.progress.failed.load(Ordering::Acquire);
        let ended = self.phase == MediaPhase::Hold
            || (self.phase == MediaPhase::Playing && self.ended.load(Ordering::Acquire));
        // A source woken from a stored end that never left it keeps it.
        self.ended_stored = (ended || self.ended_stored) && !failed;
        self.released = false;
        self.join_worker();
        shared.offer_rings(None);
        self.phase = MediaPhase::Standby;
    }

    /// The pause release (ADR-0108 decision 10): joins the worker, which frees the
    /// decoder, the file handle and the worker's buffers, while the state, the cursor
    /// and the picture node's last picture stay. Control path.
    pub(crate) fn release_worker(&mut self, shared: &Shared) {
        // A worker that reached the end of the file goes to the end hold instead.
        if self.phase != MediaPhase::Playing
            || self.worker.is_none()
            || self.ended.load(Ordering::Acquire)
        {
            return;
        }
        self.join_worker();
        // The thread marks itself ended when it returns; this run did not end.
        self.ended = Arc::new(AtomicBool::new(false));
        shared.offer_rings(None);
        self.released = true;
    }

    pub(crate) fn worker_joined(&self) -> bool {
        match &self.reaper {
            Some(reaper) => reaper.is_finished(),
            None => self.worker.is_none(),
        }
    }

    /// Signals the worker to stop and hands the join to a reaper thread. Returns
    /// without waiting (ADR-0108 decision 7).
    fn join_worker(&mut self) {
        self.detach_worker();
    }

    fn detach_worker(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        worker.shutdown.store(true, Ordering::Release);
        worker.join.thread().unpark();
        let previous = self.reaper.take();
        // A failed spawn drops the closure, which detaches the worker. It still
        // sees the shutdown flag and exits on its own.
        if let Ok(reaper) = thread::Builder::new()
            .name("rbs-media-reaper".into())
            .spawn(move || {
                if let Some(previous) = previous {
                    let _ = previous.join();
                }
                let _ = worker.join.join();
            })
        {
            self.reaper = Some(reaper);
        }
    }

    /// Waits for a reaper handed off earlier. The next start calls this so it
    /// does not open a second decoder while the previous one is still closing.
    fn wait_reaped(&mut self) {
        if let Some(reaper) = self.reaper.take() {
            let _ = reaper.join();
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.join_worker();
    }
}
