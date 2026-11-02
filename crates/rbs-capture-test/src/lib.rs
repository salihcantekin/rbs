//! A synthetic capture adapter: test-pattern displays, windows and cameras.
//!
//! It runs on every platform without hardware and is fully deterministic: frames are
//! produced on the cadence of the request's clock when [`CaptureSession::latest_frame`]
//! is called, with no threads. It is the reference implementation of the capture port
//! (`docs/specs/capture-port.md`) and lets the engine, CI and other crates' tests use
//! capture without an OS capture API.
//!
//! Like a real adapter, it has its own source clock (offset and drifting from the
//! master clock, see [`to_source`]) and converts to master time at the edge with a
//! [`DriftEstimator`] (ADR-0004).
//!
//! Frames are delivered in system memory from a fixed ring of [`FRAMES_IN_FLIGHT`]
//! buffers allocated in `start`. Delivering a frame neither allocates nor frees.
//!
//! The factory also implements [`TestHooks`], so tests can resize windows, remove
//! targets and count the resources the adapter holds.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use rbs_capture::conformance::TestHooks;
use rbs_capture::{
    CameraMode, CaptureError, CaptureFactory, CaptureRequest, CaptureSession, CaptureStats,
    CaptureTarget, ErrorDomain, TargetId, TargetKind, caps,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    Bytes, ColorSpace, Lease, LeaseOwner, MemoryKind, PixelFormat, PlaneLayout, Size, SystemFrame,
    VideoFormat, VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, HARDWARE, ProbeError};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::{Cadence, DriftEstimator, FrameRate};

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.capture.test";

/// Frames a consumer may hold at once.
pub const FRAMES_IN_FLIGHT: usize = 3;

/// Offset of the adapter's source clock from the master clock.
pub const SOURCE_OFFSET: Duration = Duration::from_secs(7);

/// Drift of the source clock in parts per million (it runs fast by this much).
pub const SOURCE_DRIFT_PPM: u64 = 100;

/// Highest frame rate delivered, for displays and windows.
const MAX_RATE: FrameRate = FrameRate::FPS_60;

const SCREEN_FORMATS: [PixelFormat; 2] = [PixelFormat::Bgra8, PixelFormat::Nv12];

/// Largest size a window can be resized to; ring buffers are allocated for it so a
/// resize never allocates on the render thread.
const WINDOW_MAX: (u32, u32) = (1280, 720);

/// The adapter's own source clock, as a function of master time: offset by
/// [`SOURCE_OFFSET`] and running fast by [`SOURCE_DRIFT_PPM`].
pub fn to_source(master: Timestamp) -> u64 {
    let m = u128::from(master.as_nanos());
    let scaled = m * (1_000_000 + u128::from(SOURCE_DRIFT_PPM)) / 1_000_000;
    let source = scaled + SOURCE_OFFSET.as_nanos();
    u64::try_from(source).unwrap_or(u64::MAX)
}

struct Target {
    info: CaptureTarget,
    present: Arc<AtomicBool>,
    /// Current size as `width << 32 | height`.
    size: Arc<AtomicU64>,
    max: (u32, u32),
}

const fn pack(w: u32, h: u32) -> u64 {
    ((w as u64) << 32) | h as u64
}

const fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

/// Factory of the synthetic adapter.
pub struct TestCaptureFactory {
    targets: Vec<Target>,
    /// Ring buffers currently alive, across all sessions of this factory.
    live: Arc<AtomicUsize>,
}

impl Default for TestCaptureFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl TestCaptureFactory {
    /// A factory with one display (`test-display-0`, 1280x720), one window
    /// (`test-window-0`, 640x360, resizable up to 1280x720) and one camera
    /// (`test-camera-0`).
    pub fn new() -> Self {
        let camera_modes = vec![
            CameraMode::fixed(640, 480, PixelFormat::Yuy2, FrameRate::FPS_30),
            CameraMode::ranged(
                640,
                480,
                PixelFormat::Nv12,
                FrameRate::FPS_24,
                FrameRate::FPS_30,
            ),
            CameraMode::fixed(1280, 720, PixelFormat::Nv12, FrameRate::FPS_30),
        ];
        let target = |info: CaptureTarget, max: (u32, u32)| Target {
            size: Arc::new(AtomicU64::new(info.size.map_or(0, |(w, h)| pack(w, h)))),
            info,
            present: Arc::new(AtomicBool::new(true)),
            max,
        };
        Self {
            targets: vec![
                target(
                    CaptureTarget::new(
                        TargetId::new("test-display-0"),
                        TargetKind::Display,
                        "Test display",
                    )
                    .with_size(1280, 720),
                    (1280, 720),
                ),
                target(
                    CaptureTarget::new(
                        TargetId::new("test-window-0"),
                        TargetKind::Window,
                        "Test window",
                    )
                    .with_size(640, 360),
                    WINDOW_MAX,
                ),
                target(
                    CaptureTarget::new(
                        TargetId::new("test-camera-0"),
                        TargetKind::Camera,
                        "Test camera",
                    )
                    .with_modes(camera_modes),
                    (0, 0),
                ),
            ],
            live: Arc::new(AtomicUsize::new(0)),
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

    /// Resizes a display or window target. Returns `false` for cameras, unknown ids,
    /// zero sizes and sizes above the target's maximum.
    pub fn resize_target(&self, id: &TargetId, width: u32, height: u32) -> bool {
        match self.targets.iter().find(|t| &t.info.id == id) {
            Some(t)
                if t.info.kind != TargetKind::Camera
                    && width > 0
                    && height > 0
                    && width <= t.max.0
                    && height <= t.max.1 =>
            {
                t.size.store(pack(width, height), Ordering::Release);
                true
            }
            _ => false,
        }
    }

    /// Ring buffers currently allocated by this factory's sessions.
    pub fn live_buffers(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    fn find(&self, id: &TargetId) -> Option<&Target> {
        self.targets
            .iter()
            .find(|t| &t.info.id == id && t.present.load(Ordering::Acquire))
    }
}

impl TestHooks for TestCaptureFactory {
    fn resize(&self, target: &TargetId, width: u32, height: u32) -> bool {
        self.resize_target(target, width, height)
    }

    fn remove(&self, target: &TargetId) -> bool {
        self.remove_target(target)
    }

    fn resources(&self) -> Option<u64> {
        Some(self.live_buffers() as u64)
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
            .map(|t| {
                let info = t.info.clone();
                if info.kind == TargetKind::Camera {
                    info
                } else {
                    let (w, h) = unpack(t.size.load(Ordering::Acquire));
                    info.with_size(w, h)
                }
            })
            .collect())
    }

    fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
        let target = self
            .find(&request.target)
            .ok_or(CaptureError::TargetNotFound)?;
        if !request.memory.contains(&MemoryKind::System) {
            return Err(CaptureError::FormatUnsupported);
        }
        let current = unpack(target.size.load(Ordering::Acquire));
        let (size, pixel_format, rate) = negotiate(&target.info, current, request)?;
        let color = if pixel_format.is_yuv() {
            ColorSpace::BT709_LIMITED
        } else {
            ColorSpace::SRGB
        };
        let capacity = match target.info.kind {
            TargetKind::Camera => size,
            _ => Size::new(target.max.0, target.max.1),
        };
        Ok(Box::new(TestSession {
            format: VideoFormat::new(size, pixel_format, color),
            capacity,
            rate,
            clock: Arc::clone(&request.clock),
            present: Arc::clone(&target.present),
            size: Arc::clone(&target.size),
            resizable: target.info.kind != TargetKind::Camera,
            live: Arc::clone(&self.live),
            active: None,
            retired: None,
            next_sequence: 0,
            last_timestamp: None,
            drift: DriftEstimator::new(Duration::from_millis(100)),
            stats: CaptureStats::default(),
        }))
    }
}

/// Picks size, pixel format and rate for a request, following the request order.
fn negotiate(
    target: &CaptureTarget,
    current: (u32, u32),
    request: &CaptureRequest,
) -> Result<(Size, PixelFormat, FrameRate), CaptureError> {
    match target.kind {
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
            let rate = match request.frame_rate {
                Some(r) if rate_lt(r, mode.min_frame_rate) => mode.min_frame_rate,
                Some(r) if rate_lt(mode.max_frame_rate, r) => mode.max_frame_rate,
                Some(r) => r,
                None => mode.max_frame_rate,
            };
            Ok((Size::new(mode.width, mode.height), pf, rate))
        }
        _ => {
            let pf = request
                .pixel_formats
                .iter()
                .copied()
                .find(|pf| SCREEN_FORMATS.contains(pf))
                .ok_or(CaptureError::FormatUnsupported)?;
            let rate = match request.frame_rate {
                Some(r) if rate_lt(r, MAX_RATE) => r,
                _ => MAX_RATE,
            };
            Ok((Size::new(current.0, current.1), pf, rate))
        }
    }
}

/// Exact `a < b` for frame rates.
fn rate_lt(a: FrameRate, b: FrameRate) -> bool {
    let (a, b) = (a.per_second(), b.per_second());
    u64::from(a.num()) * u64::from(b.den()) < u64::from(b.num()) * u64::from(a.den())
}

/// Plane layouts of a tightly packed frame of `format`, the plane count and the bytes
/// it needs.
fn layout(format: &VideoFormat) -> ([PlaneLayout; 3], usize, usize) {
    let pf = format.pixel_format;
    let mut planes = [PlaneLayout::default(); 3];
    let mut offset = 0usize;
    let count = pf.plane_count().min(planes.len());
    for (index, plane) in planes.iter_mut().enumerate().take(count) {
        let row = pf.min_row_bytes(index, format.size.width).unwrap_or(0) as usize;
        let rows = pf.plane_rows(index, format.size.height).unwrap_or(0) as usize;
        *plane = PlaneLayout {
            offset,
            stride: row,
        };
        offset += row * rows;
    }
    (planes, count, offset)
}

/// The ring of frame buffers of an Active session. Also the lease owner: a slot's count
/// is the number of frames referencing it.
struct Ring {
    buffers: [Bytes; FRAMES_IN_FLIGHT],
    refs: [AtomicU32; FRAMES_IN_FLIGHT],
    live: Arc<AtomicUsize>,
}

impl Ring {
    /// Allocates buffers big enough for `capacity` in `pixel_format`.
    fn new(pixel_format: PixelFormat, capacity: Size, live: Arc<AtomicUsize>) -> Self {
        let format = VideoFormat::new(capacity, pixel_format, ColorSpace::SRGB);
        let (_, _, len) = layout(&format);
        let buffers = std::array::from_fn(|slot| {
            let bytes = Bytes::from(pattern(pixel_format, len, slot));
            // Promote the buffer to shared storage now, so that later clones on the
            // render thread only bump a reference count and never allocate.
            drop(bytes.clone());
            bytes
        });
        live.fetch_add(1, Ordering::AcqRel);
        Self {
            buffers,
            refs: std::array::from_fn(|_| AtomicU32::new(0)),
            live,
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

impl Drop for Ring {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
    }
}

// SAFETY: a slot's buffer is an immutable `Bytes` owned by the ring, which lives as
// long as any lease (each lease holds an `Arc` to it), so the memory stays valid while
// the count is above zero. Counts are exact: `claim` takes the first reference with a
// 0 -> 1 compare-exchange (the one `Lease::adopt` adopts), `retain` adds one per clone
// and `release` removes one per drop. Neither method allocates, blocks or panics;
// unknown tokens are ignored.
unsafe impl LeaseOwner for Ring {
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
    /// Capture instants, in master time. The adapter only reports them converted to
    /// its own source clock, like a real OS API.
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
    capacity: Size,
    rate: FrameRate,
    clock: Arc<dyn Clock>,
    present: Arc<AtomicBool>,
    size: Arc<AtomicU64>,
    resizable: bool,
    live: Arc<AtomicUsize>,
    active: Option<Active>,
    /// A ring stopped while frames were still held; freed on drop or the next start.
    retired: Option<Arc<Ring>>,
    next_sequence: u64,
    last_timestamp: Option<Timestamp>,
    drift: DriftEstimator,
    stats: CaptureStats,
}

impl TestSession {
    /// Picks up a resize of the target. No allocation: the ring was sized for the
    /// target's maximum.
    fn follow_size(&mut self) {
        if !self.resizable {
            return;
        }
        let (w, h) = unpack(self.size.load(Ordering::Acquire));
        let size = self.format.size;
        if (size.width, size.height) != (w, h)
            && w <= self.capacity.width
            && h <= self.capacity.height
        {
            self.format =
                VideoFormat::new(Size::new(w, h), self.format.pixel_format, self.format.color);
        }
    }
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
        self.retired = None;
        let ring = Arc::new(Ring::new(
            self.format.pixel_format,
            self.capacity,
            Arc::clone(&self.live),
        ));
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
        if let Some(active) = self.active.take() {
            // The session's two references are `ring` and `owner`; any more are leases
            // of frames still held. Keep such a ring until the session is dropped or
            // started again, so the last lease, dropped on the render thread, never
            // frees the buffers there.
            if Arc::strong_count(&active.ring) > 2 {
                self.retired = Some(active.ring);
            }
        }
    }

    fn latest_frame(&mut self) -> Result<Option<VideoFrame>, CaptureError> {
        if self.active.is_none() {
            return Ok(None);
        }
        if !self.present.load(Ordering::Acquire) {
            return Err(CaptureError::TargetLost);
        }
        self.follow_size();
        let now = self.clock.now();
        // Clock edge (ADR-0004): pair the source clock with the master clock now, as an
        // OS adapter does when a frame arrives.
        // A restart (source clock went backwards) needs no action here: the floor below
        // keeps emitted timestamps monotonic.
        let _ = self.drift.observe(to_source(now), now);
        let Some(active) = self.active.as_mut() else {
            return Ok(None);
        };

        let Some(index) = active.cadence.index_at_or_before(now) else {
            return Ok(None);
        };
        let first_new = active.last_index.map_or(0, |last| last + 1);
        if index < first_new {
            return Ok(None);
        }
        active.last_index = Some(index);
        self.next_sequence = active.sequence_base + index + 1;
        // Frames between the previous delivery and this one were received and replaced
        // before anyone asked for them.
        self.stats.dropped += index - first_new;

        let Some(slot) = active.ring.claim() else {
            // The consumer holds every buffer: drop rather than block (invariant 9).
            self.stats.dropped += 1;
            return Ok(None);
        };
        // SAFETY: `claim` counted exactly one reference for `slot`; this lease adopts
        // it, so the count drops back when the frame and its clones are gone.
        let lease = unsafe { Lease::adopt(Arc::clone(&active.owner), slot as u64) };

        let (planes, count, _) = layout(&self.format);
        let Ok(system) = SystemFrame::new(
            self.format,
            active.ring.buffers[slot].clone(),
            &planes[..count],
        ) else {
            return Err(CaptureError::Backend {
                domain: ErrorDomain::Library,
                code: 1,
                recoverable: false,
            });
        };

        // The capture instant as the OS would report it, in source time, converted to
        // master time, kept monotonic and never ahead of the clock.
        let source_time = to_source(active.cadence.time_of(index));
        let mapped = self
            .drift
            .mapping()
            .map_or(now, |m| m.to_master(source_time));
        let floor = self
            .last_timestamp
            .map_or(Timestamp::ZERO, |t| t + Duration::from_nanos(1));
        let timestamp = mapped.min(now).max(floor);
        self.last_timestamp = Some(timestamp);
        self.stats.delivered += 1;
        Ok(Some(
            VideoFrame::system(timestamp, system, Some(lease))
                .with_sequence(active.sequence_base + index)
                .with_source_time(source_time),
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
        let request = CaptureRequest::new(TargetId::new(id), Arc::new(clock.clone()))
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

    /// Inverse of [`to_source`].
    fn to_master(source: u64) -> u64 {
        let s = u128::from(source) - SOURCE_OFFSET.as_nanos();
        (s * 1_000_000 / (1_000_000 + u128::from(SOURCE_DRIFT_PPM))) as u64
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
    fn timestamps_are_converted_from_the_source_clock() {
        let factory = TestCaptureFactory::new();
        let clock = TestClock::new();
        clock.advance(Duration::from_secs(3));
        let mut session = open(&factory, "test-display-0", &clock);
        assert!(session.start().is_ok());
        // Poll between frames, as a render tick would, for two seconds.
        let step = MAX_RATE.frame_duration() / 3;
        let mut worst_after_warmup = 0;
        for i in 0..360 {
            clock.advance(step);
            if let Some(f) = frame(session.as_mut()) {
                let source = f.source_time().unwrap_or(0);
                assert!(source > clock.now().as_nanos(), "source clock is offset");
                let truth = to_master(source);
                let error = truth.abs_diff(f.timestamp().as_nanos());
                if i > 60 {
                    worst_after_warmup = worst_after_warmup.max(error);
                }
            }
        }
        assert!(
            worst_after_warmup < 1_000,
            "worst conversion error {worst_after_warmup} ns"
        );
    }

    #[test]
    fn resized_window_delivers_new_size_without_allocating_a_new_ring() {
        let factory = TestCaptureFactory::new();
        let clock = TestClock::new();
        let mut session = open(&factory, "test-window-0", &clock);
        assert!(session.start().is_ok());
        clock.advance(Duration::from_millis(20));
        assert_eq!(
            frame(session.as_mut()).map(|f| f.format().size),
            Some(Size::new(640, 360))
        );
        assert!(factory.resize_target(&TargetId::new("test-window-0"), 1000, 600));
        assert!(!factory.resize_target(&TargetId::new("test-window-0"), 4000, 600));
        clock.advance(Duration::from_millis(20));
        assert_eq!(
            frame(session.as_mut()).map(|f| f.format().size),
            Some(Size::new(1000, 600))
        );
        assert_eq!(session.format().size, Size::new(1000, 600));
        assert_eq!(factory.live_buffers(), 1);
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
        let request = CaptureRequest::new(TargetId::new("test-window-0"), Arc::new(clock.clone()))
            .with_pixel_formats(&[PixelFormat::Bgra8])
            .with_memory(&[MemoryKind::System]);
        assert_eq!(
            factory.open(&request).err(),
            Some(CaptureError::TargetNotFound)
        );
    }

    #[test]
    fn camera_picks_mode_by_format_order_size_and_rate() {
        let factory = TestCaptureFactory::new();
        let clock = Arc::new(TestClock::new());
        let request = CaptureRequest::new(TargetId::new("test-camera-0"), clock)
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

    #[test]
    fn opening_allocates_nothing_until_start() {
        let factory = TestCaptureFactory::new();
        let clock = TestClock::new();
        let mut session = open(&factory, "test-display-0", &clock);
        assert_eq!(factory.live_buffers(), 0);
        assert!(session.start().is_ok());
        assert_eq!(factory.live_buffers(), 1);
        clock.advance(Duration::from_millis(20));
        let held = frame(session.as_mut());
        session.stop();
        assert_eq!(factory.live_buffers(), 1, "a held frame keeps its buffer");
        drop(held);
        assert_eq!(
            factory.live_buffers(),
            1,
            "the buffer is retired, not freed on the dropping thread"
        );
        drop(session);
        assert_eq!(factory.live_buffers(), 0);
    }
}
