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
use super::picture::{MediaNodeError, MediaPhase};
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
    pub(crate) phase: MediaPhase,
    worker: Option<Worker>,
    pub(crate) ended: Arc<AtomicBool>,
    pub(crate) progress: Arc<Progress>,
    /// The picture rings of the current run while the picture node does not hold them.
    pub(crate) rings: Option<Rings>,
    picture_in: bool,
    sound_in: bool,
    pub(crate) queue_allocations: u32,
    pub(crate) attempts: u32,
    pub(crate) retry_after: Option<Duration>,
}

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
            phase: MediaPhase::Standby,
            worker: None,
            ended: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(Progress::default()),
            rings: None,
            picture_in: false,
            sound_in: false,
            queue_allocations: 0,
            attempts: 0,
            retry_after: None,
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
            self.start(shared)
        } else {
            Ok(())
        }
    }

    /// Marks `user` as released. With neither node prepared, stops the worker and
    /// frees the decoder and the rings.
    pub(crate) fn release(&mut self, user: User) {
        match user {
            User::Picture => self.picture_in = false,
            User::Sound => self.sound_in = false,
        }
        if !self.picture_in && !self.sound_in {
            self.stop();
        }
    }

    /// Opens the file, seeks, and starts a worker. Control thread only. Blocks until
    /// the worker holds its first picture.
    pub(crate) fn start(&mut self, shared: &Arc<Shared>) -> Result<(), MediaNodeError> {
        self.stop();
        if self.restart_when_shown {
            shared.set_cursor(Duration::ZERO);
        }
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
                self.rings = Some(Rings {
                    pictures: ready.pictures,
                    returns: ready.returns,
                });
                self.queue_allocations = self.queue_allocations.saturating_add(1);
                self.worker = Some(Worker { join, shutdown });
                self.phase = MediaPhase::Playing;
                Ok(())
            }
            Err(error) => {
                shutdown.store(true, Ordering::Release);
                join.thread().unpark();
                let _ = join.join();
                self.phase = MediaPhase::Standby;
                Err(error)
            }
        }
    }

    /// Joins the worker after it ended and moves to Hold. A fault counts towards the
    /// back-off.
    pub(crate) fn hold(&mut self) {
        let failed = self.progress.failed.load(Ordering::Acquire);
        self.join_worker();
        self.rings = None;
        if failed {
            self.attempts = self.attempts.saturating_add(1);
            self.retry_after = Some(RestartPolicy::default().delay(self.attempts));
        }
        if self.phase == MediaPhase::Playing {
            self.phase = MediaPhase::Hold;
        }
    }

    /// Stops the worker and frees the decoder and the rings.
    pub(crate) fn stop(&mut self) {
        self.join_worker();
        self.rings = None;
        self.phase = MediaPhase::Standby;
    }

    pub(crate) fn worker_joined(&self) -> bool {
        self.worker.is_none()
    }

    fn join_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.shutdown.store(true, Ordering::Release);
            worker.join.thread().unpark();
            let _ = worker.join.join();
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}
