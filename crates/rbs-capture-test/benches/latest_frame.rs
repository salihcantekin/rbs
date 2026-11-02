//! Cost of the capture hand-off on the render thread: one `latest_frame` call that
//! returns a frame, plus dropping it (lease release). Uses the synthetic adapter, so it
//! measures the port's per-frame overhead (trait call, lease, frame construction), not
//! an OS API.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_capture::{CaptureFactory, CaptureRequest, TargetId};
use rbs_capture_test::TestCaptureFactory;
use rbs_core::media::{MemoryKind, PixelFormat};
use rbs_core::time::TestClock;

fn latest_frame(c: &mut Criterion) {
    let factory = TestCaptureFactory::new();
    let clock = TestClock::new();
    let request = CaptureRequest::new(TargetId::new("test-display-0"), Arc::new(clock.clone()))
        .with_pixel_formats(&[PixelFormat::Bgra8])
        .with_memory(&[MemoryKind::System]);
    let Ok(mut session) = factory.open(&request) else {
        return;
    };
    if session.start().is_err() {
        return;
    }
    let period = Duration::from_micros(16_667);
    c.bench_function("capture/latest_frame+drop", |b| {
        b.iter(|| {
            clock.advance(period);
            black_box(session.latest_frame().ok().flatten());
        })
    });
    c.bench_function("capture/latest_frame_none", |b| {
        b.iter(|| black_box(session.latest_frame().ok().flatten()))
    });
}

criterion_group!(benches, latest_frame);
criterion_main!(benches);
