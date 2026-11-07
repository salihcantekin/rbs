//! The session side every adapter shares: reserve, check, hand over, wake the sender
//! (ADR-0066 decision 3).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::Thread;
use std::time::Duration;

use rbs_core::media::{FramePayload, MemoryKind, VideoFrame};

use crate::error::VcamError;
use crate::mailbox::{Mailbox, Reservation};
use crate::session::{VcamCounters, VcamStats, VirtualCameraConfig};

/// A slot reserved for one frame. Dropping it unused frees the slot.
#[derive(Debug)]
pub struct SlotToken {
    reservation: Reservation<VideoFrame>,
}

/// The render-thread half: reserves slots, checks frames against the configuration,
/// hands them over and wakes the sender thread.
#[derive(Debug)]
pub struct Handoff {
    mailbox: Arc<Mailbox<VideoFrame>>,
    counters: Arc<VcamCounters>,
    config: VirtualCameraConfig,
    sender: Option<Thread>,
}

/// The sender-thread half: takes the newest frame and drops replaced ones.
#[derive(Debug, Clone)]
pub struct HandoffReceiver {
    mailbox: Arc<Mailbox<VideoFrame>>,
    counters: Arc<VcamCounters>,
}

impl Handoff {
    /// A hand-off for `config`. The adapter starts its sender thread with the receiver
    /// and then calls [`Handoff::set_sender`] so `send` can wake it.
    pub fn new(config: VirtualCameraConfig) -> (Self, HandoffReceiver) {
        let mailbox = Mailbox::new();
        let counters = Arc::new(VcamCounters::default());
        (
            Self {
                mailbox: Arc::clone(&mailbox),
                counters: Arc::clone(&counters),
                config,
                sender: None,
            },
            HandoffReceiver { mailbox, counters },
        )
    }

    /// The thread `send` wakes (with `Thread::unpark`, a non-blocking wake).
    pub fn set_sender(&mut self, thread: Thread) {
        self.sender = Some(thread);
    }

    /// The configuration frames are checked against.
    pub fn config(&self) -> &VirtualCameraConfig {
        &self.config
    }

    /// See [`crate::VirtualCameraSession::reserve`].
    pub fn reserve(&self) -> Option<SlotToken> {
        match self.mailbox.reserve() {
            Some(reservation) => Some(SlotToken { reservation }),
            None => {
                self.counters.count_consumer_busy();
                None
            }
        }
    }

    fn check(&self, frame: &VideoFrame) -> Result<(), &'static str> {
        let f = frame.format();
        let c = &self.config.format;
        if f.size.width != c.width || f.size.height != c.height {
            return Err("frame size differs from the camera size");
        }
        if f.pixel_format != c.pixel {
            return Err("pixel format differs from the camera's");
        }
        let memory = match frame.payload() {
            FramePayload::System(_) => MemoryKind::System,
            FramePayload::Texture(_) => MemoryKind::Gpu,
            _ => return Err("native frames are not taken by this session"),
        };
        if memory != self.config.memory {
            return Err("memory kind differs from the session's");
        }
        Ok(())
    }

    /// See [`crate::VirtualCameraSession::send`].
    pub fn send(&self, slot: SlotToken, frame: VideoFrame) -> Result<(), VcamError> {
        if !slot.reservation.is_for(&self.mailbox) {
            self.counters.refused.fetch_add(1, Ordering::Relaxed);
            return Err(VcamError::InvalidFrame("slot of another session"));
        }
        if let Err(reason) = self.check(&frame) {
            self.counters.refused.fetch_add(1, Ordering::Relaxed);
            return Err(VcamError::InvalidFrame(reason));
        }
        slot.reservation.fill(frame);
        if let Some(sender) = &self.sender {
            sender.unpark();
        }
        Ok(())
    }

    /// The counters.
    pub fn stats(&self) -> VcamStats {
        self.counters.snapshot()
    }

    /// The counters, shared with the sender thread.
    pub fn counters(&self) -> &Arc<VcamCounters> {
        &self.counters
    }
}

impl HandoffReceiver {
    /// The newest frame handed over since the last call; older ones are dropped here
    /// and counted as `replaced`.
    pub fn take_newest(&self) -> Option<VideoFrame> {
        let counters = &self.counters;
        self.mailbox.take_newest(|old| {
            counters.replaced.fetch_add(1, Ordering::Relaxed);
            drop(old);
        })
    }

    /// Waits until `send` wakes this thread or `timeout` passes (may wake early).
    /// Call only from the sender thread.
    pub fn wait(&self, timeout: Duration) {
        std::thread::park_timeout(timeout);
    }

    /// The counters, shared with the session.
    pub fn counters(&self) -> &Arc<VcamCounters> {
        &self.counters
    }
}
