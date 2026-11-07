//! An in-process virtual camera adapter for tests (ADR-0066): "installing" sets a flag,
//! and the consumer side is a sender thread that records the sequence numbers of the
//! frames it takes. [`TestConsumer`] can stall it, to test what happens when an app stops
//! reading.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{MemoryKind, PixelFormat, VideoFrame};
use rbs_vcam::caps;
use rbs_vcam::conformance::Probe;
use rbs_vcam::{
    CameraFormat, Handoff, HandoffReceiver, InstallState, SlotToken, VcamError, VcamStats,
    VirtualCameraConfig, VirtualCameraFactory, VirtualCameraSession,
};

/// Adapter id.
pub const ID: &str = "org.rbs.vcam.test";

/// The consumer side shared by the factory and its sessions.
#[derive(Debug, Default)]
pub struct TestConsumer {
    stalled: AtomicBool,
    /// Whether an app "reads" the camera (a format change then stays pending).
    reading: AtomicBool,
    delivered: Mutex<Vec<u64>>,
}

impl TestConsumer {
    /// Simulates an app opening or closing the camera.
    pub fn set_reading(&self, reading: bool) {
        self.reading.store(reading, Ordering::Release);
    }
}

impl Probe for TestConsumer {
    fn stall(&self) {
        self.stalled.store(true, Ordering::Release);
    }

    fn resume(&self) {
        self.stalled.store(false, Ordering::Release);
    }

    fn delivered(&self) -> Vec<u64> {
        self.delivered.lock().map(|d| d.clone()).unwrap_or_default()
    }
}

/// The test factory.
#[derive(Debug)]
pub struct TestVcamFactory {
    caps: CapabilitySet,
    installed: AtomicBool,
    format: Mutex<Option<CameraFormat>>,
    consumer: Arc<TestConsumer>,
}

impl Default for TestVcamFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl TestVcamFactory {
    /// A factory whose camera is not installed.
    pub fn new() -> Self {
        let set = |v: &[&str]| CapabilityValue::Set(v.iter().map(|s| (*s).to_owned()).collect());
        let caps = CapabilitySet::new()
            .with(caps::INPUT_MEMORY, set(&["system"]))
            .with(caps::INPUT_FORMAT, set(&["nv12"]))
            .with(caps::MAX_WIDTH, CapabilityValue::Int(1920))
            .with(caps::MAX_HEIGHT, CapabilityValue::Int(1080))
            .with(caps::RATES, set(&["15", "24", "30", "60"]))
            .with(caps::PERSISTENT, CapabilityValue::Flag(true));
        Self {
            caps,
            installed: AtomicBool::new(false),
            format: Mutex::new(None),
            consumer: Arc::new(TestConsumer::default()),
        }
    }

    /// The consumer side.
    pub fn consumer(&self) -> &Arc<TestConsumer> {
        &self.consumer
    }

    /// The format the camera announces now.
    pub fn format(&self) -> Option<CameraFormat> {
        self.format.lock().ok().and_then(|f| *f)
    }

    fn state(&self) -> InstallState {
        if self.installed.load(Ordering::Acquire) {
            InstallState::Installed
        } else {
            InstallState::NotInstalled
        }
    }
}

fn check_format(format: &CameraFormat) -> Result<(), VcamError> {
    let ok = format.width > 0
        && format.height > 0
        && format.width.is_multiple_of(2)
        && format.height.is_multiple_of(2)
        && format.width <= 1920
        && format.height <= 1080
        && format.pixel == PixelFormat::Nv12;
    if ok {
        Ok(())
    } else {
        Err(VcamError::InvalidConfig(format!("{format:?}")))
    }
}

impl VirtualCameraFactory for TestVcamFactory {
    fn id(&self) -> &'static str {
        ID
    }

    fn capabilities(&self) -> &CapabilitySet {
        &self.caps
    }

    fn status(&self) -> Result<InstallState, VcamError> {
        Ok(self.state())
    }

    fn install(&self) -> Result<InstallState, VcamError> {
        self.installed.store(true, Ordering::Release);
        Ok(self.state())
    }

    fn uninstall(&self) -> Result<InstallState, VcamError> {
        self.installed.store(false, Ordering::Release);
        Ok(self.state())
    }

    fn set_format(&self, format: &CameraFormat) -> Result<bool, VcamError> {
        check_format(format)?;
        if self.consumer.reading.load(Ordering::Acquire) {
            return Ok(false);
        }
        if let Ok(mut f) = self.format.lock() {
            *f = Some(*format);
        }
        Ok(true)
    }

    fn open(
        &self,
        config: &VirtualCameraConfig,
    ) -> Result<Box<dyn VirtualCameraSession>, VcamError> {
        if !self.installed.load(Ordering::Acquire) {
            return Err(VcamError::NotInstalled);
        }
        check_format(&config.format)?;
        if config.memory != MemoryKind::System {
            return Err(VcamError::InvalidConfig("system memory only".into()));
        }
        Ok(Box::new(TestSession::start(
            *config,
            Arc::clone(&self.consumer),
        )?))
    }
}

/// A session: a sender thread that records what it takes.
#[derive(Debug)]
pub struct TestSession {
    handoff: Handoff,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TestSession {
    fn start(config: VirtualCameraConfig, consumer: Arc<TestConsumer>) -> Result<Self, VcamError> {
        let (mut handoff, rx) = Handoff::new(config);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("rbs-vcam-test".into())
                .spawn(move || sender(rx, consumer, stop))
                .map_err(|e| VcamError::Os {
                    code: 0,
                    message: format!("spawn: {e}"),
                })?
        };
        handoff.set_sender(thread.thread().clone());
        Ok(Self {
            handoff,
            stop,
            thread: Some(thread),
        })
    }
}

fn sender(rx: HandoffReceiver, consumer: Arc<TestConsumer>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        if !consumer.stalled.load(Ordering::Acquire)
            && let Some(frame) = rx.take_newest()
        {
            deliver(&consumer, &frame);
            rx.counters().count_sent();
            continue;
        }
        // Woken by `send`, by `drop`, or after a while to see a resume.
        rx.wait(Duration::from_millis(5));
    }
}

fn deliver(consumer: &TestConsumer, frame: &VideoFrame) {
    if let Ok(mut d) = consumer.delivered.lock() {
        d.push(frame.sequence());
    }
}

impl VirtualCameraSession for TestSession {
    fn reserve(&self) -> Option<SlotToken> {
        self.handoff.reserve()
    }

    fn send(&self, slot: SlotToken, frame: VideoFrame) -> Result<(), VcamError> {
        self.handoff.send(slot, frame)
    }

    fn stats(&self) -> VcamStats {
        self.handoff.stats()
    }
}

impl Drop for TestSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}
