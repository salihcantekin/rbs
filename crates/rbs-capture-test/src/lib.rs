//! A synthetic capture adapter: test-pattern displays, windows and cameras.
//!
//! It runs on every platform without hardware and is fully deterministic: frames are
//! produced on the cadence of the request's clock when [`CaptureSession::latest_frame`]
//! is called, with no threads. It is the reference implementation of the capture port
//! (`docs/specs/capture-port.md`) and lets the engine, CI and other crates' tests use
//! capture without an OS capture API.
//!
//! Frames are delivered in system memory from a fixed ring of
//! [`FRAMES_IN_FLIGHT`] buffers allocated in `start`. Delivering a frame does not
//! allocate.
//!
//! Tests can make targets disappear with [`TestCaptureFactory::remove_target`] to
//! exercise target-loss handling.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use rbs_capture::{
    CameraMode, CaptureError, CaptureFactory, CaptureRequest, CaptureSession, CaptureStats,
    CaptureTarget, TargetId, TargetKind, TargetSelector, caps,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    Bytes, ColorSpace, FramePayload, Lease, LeaseOwner, MemoryKind, PixelFormat, PlaneLayout, Size,
    SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, HARDWARE, ProbeError};
use rbs_core::time::Clock;
use rbs_core::timing::{Cadence, FrameRate};

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.capture.test";

/// Frames a consumer may hold at once.
pub const FRAMES_IN_FLIGHT: usize = 3;

/// Highest frame rate delivered, for displays and windows.
const MAX_RATE: FrameRate = FrameRate::FPS_60;

const SCREEN_FORMATS: [PixelFormat; 2] = [PixelFormat::Bgra8, PixelFormat::Nv12];

struct Target {
    info: CaptureTarget,
    present: Arc<AtomicBool>,
}

/// Factory of the synthetic adapter.
pub struct TestCaptureFactory {
    targets: Vec<Target>,
}

impl Default for TestCaptureFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl TestCaptureFactory {
    /// A factory with one display (`test-display-0`, 1280x720), one window
    /// (`test-window-0`, 640x360) and one camera (`test-camera-0`).
    pub fn new() -> Self {
        let camera_modes = vec![
            CameraMode {
                width: 640,
                height: 480,
                pixel_format: PixelFormat::Yuy2,
                frame_rate: FrameRate::FPS_30,
            },
            CameraMode {
                width: 640,
                height: 480,
                pixel_format: PixelFormat::Nv12,
                frame_rate: FrameRate::FPS_30,
            },
            CameraMode {
                width: 1280,
                height: 720,
                pixel_format: PixelFormat::Nv12,
                frame_rate: FrameRate::FPS_30,
            },
        ];
        let target = |id: &str, kind, name: &str, size, modes| Target {
            info: CaptureTarget {
                id: TargetId::new(id),
                kind,
                name: name.to_owned(),
                size,
                modes,
            },
            present: Arc::new(AtomicBool::new(true)),
        };
        Self {
            targets: vec![
                target(
                    "test-display-0",
                    TargetKind::Display,
                    "Test display",
                    Some((1280, 720)),
                    Vec::new(),
                ),
                target(
                    "test-window-0",
                    TargetKind::Window,
                    "Test window",
                    Some((640, 360)),
                    Vec::new(),
                ),
                target(
                    "test-camera-0",
                    TargetKind::Camera,
                    "Test camera",
                    None,
                    camera_modes,
                ),
            ],
        }
    }

    /// Makes a target disappear, as if the window closed or the device was unplugged.
    /// Active sessions on it return [`CaptureError::TargetLost`]. Returns `false` if
    /// the id is unknown.
    pub fn remove_target(&self, id: &TargetId) -> bool {
        match self.targets.iter().find(|t| &t.info.id == id) {
            Some(t) => {
                t.present.store(false, Ordering::Release);
                true
            }
            None => false,
        }
    }

    fn find(&self, id: &TargetId) -> Option<&Target> {
        self.targets
            .iter()
            .find(|t| &t.info.id == id && t.present.load(Ordering::Acquire))
    }
}

impl AdapterFactory for TestCaptureFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "Test pattern capture"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        let set =
            |items: &[&str]| CapabilityValue::Set(items.iter().map(|s| (*s).to_owned()).collect());
        let kinds = ["display", "window", "camera"];
        Ok(CapabilitySet::new()
            .with(caps::TARGET_KINDS, set(&kinds))
            .with(caps::TARGET_ENUMERATE, set(&kinds))
            .with(caps::TARGET_PICKER, set(&[]))
            .with(
                caps::PIXEL_FORMATS,
                set(&[
                    PixelFormat::Bgra8.as_str(),
                    PixelFormat::Nv12.as_str(),
                    PixelFormat::Yuy2.as_str(),
                ]),
            )
            .with(caps::MEMORY, set(&[MemoryKind::System.as_str()]))
            .with(caps::CURSOR, CapabilityValue::Flag(false))
            .with(caps::FRAME_RATE_MAX, CapabilityValue::Int(60))
            .with(
                caps::FRAMES_IN_FLIGHT,
                CapabilityValue::Int(FRAMES_IN_FLIGHT as i64),
            )
            .with(caps::HDR, CapabilityValue::Flag(false))
            .with(HARDWARE, CapabilityValue::Flag(false)))
    }
}

impl CaptureFactory for TestCaptureFactory {
    fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
        Ok(self
            .targets
            .iter()
            .filter(|t| t.info.kind == kind && t.present.load(Ordering::Acquire))
            .map(|t| t.info.clone())
            .collect())
    }

    fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
        let id = match &request.target {
            TargetSelector::Id(id) => id,
            TargetSelector::Picker(_) => {
                return Err(CaptureError::Unsupported("the test adapter has no picker"));
            }
        };
        let target = self.find(id).ok_or(CaptureError::TargetNotFound)?;
        if !request.memory.contains(&MemoryKind::System) {
            return Err(CaptureError::FormatUnsupported);
        }
        let (size, pixel_format, rate) = negotiate(&target.info, request)?;
        let color = if pixel_format.is_yuv() {
            ColorSpace::BT709_LIMITED
        } else {
            ColorSpace::SRGB
        };
        Ok(Box::new(TestSession {
            format: VideoFormat::new(size, pixel_format, color),
            rate,
            clock: Arc::clone(&request.clock),
            present: Arc::clone(&target.present),
            active: None,
            next_sequence: 0,
            last_timestamp: None,
            stats: CaptureStats::default(),
        }))
    }
}

/// Picks size, pixel format and rate for a request, following the request order.
fn negotiate(
    target: &CaptureTarget,
    request: &CaptureRequest,
) -> Result<(Size, PixelFormat, FrameRate), CaptureError> {
    match target.kind {
        TargetKind::Display | TargetKind::Window => {
            let pf = request
                .pixel_formats
                .iter()
                .copied()
                .find(|pf| SCREEN_FORMATS.contains(pf))
                .ok_or(CaptureError::FormatUnsupported)?;
            let (w, h) = target.size.unwrap_or((1280, 720));
            let rate = match request.frame_rate {
                Some(r) if r.per_second().as_f64() < MAX_RATE.per_second().as_f64() => r,
                _ => MAX_RATE,
            };
            Ok((Size::new(w, h), pf, rate))
        }
        TargetKind::Camera => {
            let pf = request
                .pixel_formats
                .iter()
                .copied()
                .find(|pf| target.modes.iter().any(|m| m.pixel_format == *pf))
                .ok_or(CaptureError::FormatUnsupported)?;
            let want = request.size.unwrap_or(Size::new(0, 0));
            let distance = |m: &CameraMode| {
                u64::from(m.width.abs_diff(want.width)) + u64::from(m.height.abs_diff(want.height))
            };
            let mode = target
                .modes
                .iter()
                .filter(|m| m.pixel_format == pf)
                .min_by_key(|m| distance(m))
                .ok_or(CaptureError::FormatUnsupported)?;
            Ok((Size::new(mode.width, mode.height), pf, mode.frame_rate))
        }
    }
}

/// The ring of frame buffers of an Active session. Also the lease owner: a slot's count
/// is the number of frames referencing it.
struct Ring {
    buffers: [Bytes; FRAMES_IN_FLIGHT],
    planes: [PlaneLayout; 3],
    plane_count: usize,
    refs: [AtomicU32; FRAMES_IN_FLIGHT],
}

impl Ring {
    fn new(format: &VideoFormat) -> Self {
        let pf = format.pixel_format;
        let mut planes = [PlaneLayout::default(); 3];
        let mut offset = 0usize;
        let plane_count = pf.plane_count().min(planes.len());
        for (index, plane) in planes.iter_mut().enumerate().take(plane_count) {
            let row = pf.min_row_bytes(index, format.size.width).unwrap_or(0) as usize;
            let rows = pf.plane_rows(index, format.size.height).unwrap_or(0) as usize;
            *plane = PlaneLayout {
                offset,
                stride: row,
            };
            offset += row * rows;
        }
        let buffers = std::array::from_fn(|slot| {
            let bytes = Bytes::from(pattern(pf, offset, slot));
            // Promote the buffer to shared storage now, so that later clones on the
            // render thread only bump a reference count and never allocate.
            drop(bytes.clone());
            bytes
        });
        Self {
            buffers,
            planes,
            plane_count,
            refs: std::array::from_fn(|_| AtomicU32::new(0)),
        }
    }

    /// Claims a free slot, or `None` when the consumer holds every slot.
    fn claim(&self) -> Option<usize> {
        self.refs.iter().position(|r| {
            r.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        })
    }
}

impl LeaseOwner for Ring {
    fn retain(&self, token: u64) {
        if let Some(r) = self.refs.get(token as usize) {
            r.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn release(&self, token: u64) {
        if let Some(r) = self.refs.get(token as usize) {
            // Release pairs with the Acquire in `claim`: the consumer's reads of the
            // buffer happen before the slot is handed out again.
            r.fetch_sub(1, Ordering::Release);
        }
    }
}

/// Fills a buffer with a flat pattern that differs per slot.
fn pattern(pf: PixelFormat, len: usize, slot: usize) -> Vec<u8> {
    let shade = 32 + 64 * (slot as u8 % 3);
    match pf {
        PixelFormat::Yuy2 => (0..len)
            .map(|i| if i % 2 == 0 { shade } else { 128 })
            .collect(),
        PixelFormat::Nv12 => vec![shade; len],
        _ => (0..len)
            .map(|i| if i % 4 == 3 { 255 } else { shade })
            .collect(),
    }
}

struct Active {
    cadence: Cadence,
    /// Index of the last frame delivered or skipped, `None` before the first.
    last_index: Option<u64>,
    /// Sequence number of cadence index 0.
    sequence_base: u64,
    ring: Arc<Ring>,
    owner: Arc<dyn LeaseOwner>,
}

struct TestSession {
    format: VideoFormat,
    rate: FrameRate,
    clock: Arc<dyn Clock>,
    present: Arc<AtomicBool>,
    active: Option<Active>,
    next_sequence: u64,
    last_timestamp: Option<rbs_core::time::Timestamp>,
    stats: CaptureStats,
}

impl CaptureSession for TestSession {
    fn format(&self) -> VideoFormat {
        self.format
    }

    fn start(&mut self) -> Result<(), CaptureError> {
        if self.active.is_some() {
            return Ok(());
        }
        if !self.present.load(Ordering::Acquire) {
            return Err(CaptureError::TargetLost);
        }
        // Start strictly after the last delivered frame so timestamps keep increasing
        // across stop and start.
        let now = self.clock.now();
        let origin = match self.last_timestamp {
            Some(t) if t >= now => t + Duration::from_nanos(1),
            _ => now,
        };
        let ring = Arc::new(Ring::new(&self.format));
        self.active = Some(Active {
            cadence: Cadence::frames(origin, self.rate),
            last_index: None,
            sequence_base: self.next_sequence,
            owner: Arc::clone(&ring) as Arc<dyn LeaseOwner>,
            ring,
        });
        Ok(())
    }

    fn stop(&mut self) {
        // Frames still held keep the ring alive through their leases.
        self.active = None;
    }

    fn latest_frame(&mut self) -> Result<Option<VideoFrame>, CaptureError> {
        let Some(active) = self.active.as_mut() else {
            return Ok(None);
        };
        if !self.present.load(Ordering::Acquire) {
            return Err(CaptureError::TargetLost);
        }
        let Some(index) = active.cadence.index_at_or_before(self.clock.now()) else {
            return Ok(None);
        };
        let first_new = active.last_index.map_or(0, |last| last + 1);
        if index < first_new {
            return Ok(None);
        }
        active.last_index = Some(index);
        self.next_sequence = active.sequence_base + index + 1;
        // Frames between the previous delivery and this one were "produced" and
        // replaced before anyone asked for them.
        self.stats.dropped += index - first_new;

        let Some(slot) = active.ring.claim() else {
            // The consumer holds every buffer: drop rather than block (invariant 9).
            self.stats.dropped += 1;
            return Ok(None);
        };
        let ring = &active.ring;
        let system = match SystemFrame::new(
            &self.format,
            ring.buffers[slot].clone(),
            &ring.planes[..ring.plane_count],
        ) {
            Ok(frame) => frame,
            Err(_) => {
                ring.release(slot as u64);
                return Err(CaptureError::Backend {
                    code: 0,
                    recoverable: false,
                });
            }
        };
        let timestamp = active.cadence.time_of(index);
        self.last_timestamp = Some(timestamp);
        self.stats.delivered += 1;
        let lease = Lease::adopt(Arc::clone(&active.owner), slot as u64);
        Ok(Some(
            VideoFrame::new(self.format, timestamp, FramePayload::System(system))
                .with_sequence(active.sequence_base + index)
                .with_source_time(timestamp.as_nanos())
                .with_lease(lease),
        ))
    }

    fn stats(&self) -> CaptureStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_core::time::TestClock;

    fn open(factory: &TestCaptureFactory, id: &str, clock: &TestClock) -> Box<dyn CaptureSession> {
        let request = CaptureRequest::new(
            TargetSelector::Id(TargetId::new(id)),
            Arc::new(clock.clone()),
        )
        .with_pixel_formats(&[PixelFormat::Bgra8, PixelFormat::Nv12])
        .with_memory(&[MemoryKind::System]);
        match factory.open(&request) {
            Ok(session) => session,
            Err(e) => panic!("open {id}: {e}"),
        }
    }

    fn frame(session: &mut dyn CaptureSession) -> Option<VideoFrame> {
        match session.latest_frame() {
            Ok(f) => f,
            Err(e) => panic!("latest_frame: {e}"),
        }
    }

    #[test]
    fn held_frames_cause_drops_not_blocking() {
        let factory = TestCaptureFactory::new();
        let clock = TestClock::new();
        let mut session = open(&factory, "test-display-0", &clock);
        assert!(session.start().is_ok());
        let step = MAX_RATE.frame_duration() + Duration::from_nanos(1);
        let mut frames = Vec::new();
        for _ in 0..FRAMES_IN_FLIGHT {
            clock.advance(step);
            frames.extend(frame(session.as_mut()));
        }
        assert_eq!(frames.len(), FRAMES_IN_FLIGHT);
        let dropped = session.stats().dropped;
        clock.advance(step);
        assert!(frame(session.as_mut()).is_none(), "all buffers are held");
        assert_eq!(session.stats().dropped, dropped + 1);
        frames.clear();
        clock.advance(step);
        assert!(
            frame(session.as_mut()).is_some(),
            "buffers returned on drop"
        );
    }

    #[test]
    fn removed_target_is_lost_and_not_listed() {
        let factory = TestCaptureFactory::new();
        let clock = TestClock::new();
        let mut session = open(&factory, "test-window-0", &clock);
        assert!(session.start().is_ok());
        assert!(frame(session.as_mut()).is_some());
        assert!(factory.remove_target(&TargetId::new("test-window-0")));
        assert_eq!(session.latest_frame().err(), Some(CaptureError::TargetLost));
        assert_eq!(
            factory.enumerate(TargetKind::Window).map(|t| t.len()),
            Ok(0)
        );
        let request = CaptureRequest::new(
            TargetSelector::Id(TargetId::new("test-window-0")),
            Arc::new(clock.clone()),
        )
        .with_pixel_formats(&[PixelFormat::Bgra8])
        .with_memory(&[MemoryKind::System]);
        assert_eq!(
            factory.open(&request).err(),
            Some(CaptureError::TargetNotFound)
        );
    }

    #[test]
    fn camera_picks_mode_by_format_order_and_size() {
        let factory = TestCaptureFactory::new();
        let clock = Arc::new(TestClock::new());
        let request =
            CaptureRequest::new(TargetSelector::Id(TargetId::new("test-camera-0")), clock)
                .with_pixel_formats(&[PixelFormat::Bgra8, PixelFormat::Nv12, PixelFormat::Yuy2])
                .with_memory(&[MemoryKind::System])
                .with_size(Size::new(1200, 700));
        let format = factory.open(&request).map(|s| s.format());
        assert_eq!(
            format.map(|f| (f.pixel_format, f.size)),
            Ok((PixelFormat::Nv12, Size::new(1280, 720)))
        );
        let gpu_only = request.with_memory(&[MemoryKind::D3d11]);
        assert_eq!(
            factory.open(&gpu_only).err(),
            Some(CaptureError::FormatUnsupported)
        );
    }
}
