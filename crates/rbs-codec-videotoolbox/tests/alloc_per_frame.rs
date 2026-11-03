//! Heap allocations on the encode path (output-ports spec, hot-path rules): at most one
//! per packet, the exact-size payload `Vec` built in VideoToolbox's output callback.
//! `submit` (cached IOSurface wrapper or pool buffer copy, lease clone, slot hand-off)
//! and `drain` (ring pops into a vector with spare capacity) allocate nothing in RBS
//! code. The counter is global, so allocations on VideoToolbox's callback thread count
//! too. CoreFoundation's own allocations (`malloc`) bypass the Rust allocator and are
//! not counted.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_codec::conformance::{FrameSource, SystemFrameSource};
use rbs_codec::{EncodeError, KeyframeInterval, RateControl, VideoEncoder, VideoEncoderConfig};
use rbs_codec_videotoolbox::{VideoToolboxFactory, VideoToolboxSession};
use rbs_core::capability::CapabilityValue;
use rbs_core::media::{
    CodecId, ColorSpace, EncodedPacket, MemoryKind, PixelFormat, Size, VideoFormat,
};
use rbs_core::registry::AdapterFactory;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use support::IoSurfaceSource;

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards every call unchanged to the system allocator; only counts calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's, forwarded to `System`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc` with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `ptr` came from `System.alloc` with this `layout`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The counter is process-wide, so the tests of this file must not overlap: one
/// test's frame generation would be counted in the other's window.
static SERIAL: Mutex<()> = Mutex::new(());

const WARM_UP: u64 = 40;
const MEASURED: u64 = 120;

fn measure(memory: MemoryKind, frames: &mut dyn FrameSource) {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let factory = VideoToolboxFactory::with_software();
    let has_h264 = matches!(
        factory.probe().unwrap().get(&rbs_codec::caps::VIDEO_ENCODE),
        Some(CapabilityValue::Set(s)) if s.iter().any(|c| c == "h264")
    );
    if !has_h264 {
        assert!(
            !std::env::var("RBS_REQUIRE_VIDEOTOOLBOX").is_ok_and(|v| v == "1"),
            "no VideoToolbox H.264 encoder"
        );
        eprintln!("skipped: no VideoToolbox H.264 encoder");
        return;
    }
    let format = VideoFormat::new(
        Size::new(1280, 720),
        PixelFormat::Nv12,
        ColorSpace::BT709_LIMITED,
    );
    let config = VideoEncoderConfig::new(
        CodecId::H264,
        format,
        FrameRate::FPS_30,
        memory,
        RateControl::Cbr {
            bitrate_bps: 4_000_000,
        },
    )
    .with_keyframe_interval(KeyframeInterval::Frames(60));
    let mut session = factory.create_session(&config).unwrap();
    let cadence = Cadence::frames(Timestamp::from_nanos(1_000_000_000), FrameRate::FPS_30);
    // Frames are produced before measuring: the source is test support, not the path.
    let inputs: Vec<_> = (0..WARM_UP + MEASURED)
        .map(|i| (i, cadence.time_of(i)))
        .collect();
    let mut packets = Vec::with_capacity((WARM_UP + MEASURED) as usize);
    for &(i, ts) in &inputs[..WARM_UP as usize] {
        step(&mut session, frames, &format, i, ts, &mut packets);
    }
    session.flush(&mut packets).unwrap();
    let packets_before = packets.len() as u64;
    let mut allocs = 0;
    // The source fills frames outside the counted window; only submit and drain (and
    // the callback thread meanwhile) are counted.
    for &(i, ts) in &inputs[WARM_UP as usize..] {
        allocs += step(&mut session, frames, &format, i, ts, &mut packets);
    }
    let before_flush = ALLOCS.load(Ordering::Relaxed);
    session.flush(&mut packets).unwrap();
    allocs += ALLOCS.load(Ordering::Relaxed) - before_flush;
    let produced = packets.len() as u64 - packets_before;
    assert_eq!(produced, MEASURED);
    eprintln!("{memory}: {allocs} allocations for {produced} packets");
    assert!(
        allocs <= produced,
        "{allocs} allocations for {produced} packets ({memory} input)"
    );
}

/// Submits one frame (draining while busy) and returns the allocations counted.
fn step(
    session: &mut VideoToolboxSession,
    frames: &mut dyn FrameSource,
    format: &VideoFormat,
    index: u64,
    ts: Timestamp,
    packets: &mut Vec<EncodedPacket>,
) -> u64 {
    let frame = frames.frame(format, index, ts).unwrap();
    let before = ALLOCS.load(Ordering::Relaxed);
    loop {
        match session.submit(&frame) {
            Ok(()) => break,
            Err(EncodeError::Busy) => session.drain(packets).unwrap(),
            Err(e) => panic!("submit: {e}"),
        }
    }
    session.drain(packets).unwrap();
    drop(frame);
    ALLOCS.load(Ordering::Relaxed) - before
}

#[test]
fn io_surface_input_allocates_only_payloads() {
    measure(MemoryKind::Metal, &mut IoSurfaceSource::new());
}

#[test]
fn system_input_allocates_only_payloads() {
    measure(MemoryKind::System, &mut SystemFrameSource::new());
}
