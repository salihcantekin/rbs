//! Cost of the lock-free hand-off between the WGC callback thread and the render
//! thread: publishing a slot (producer side) and taking plus releasing it (render side).
//! Runs on every platform; the Windows-only parts (texture copy, fence signal) are GPU
//! work outside this measurement.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_capture_wgc::handoff::{FrameMeta, Ring};

fn handoff(c: &mut Criterion) {
    let ring = Ring::new(&[1, 2, 3, 4, 5]);
    let meta = FrameMeta {
        timestamp: 1,
        sequence: 1,
        source_time: 1,
        fence_value: 1,
        width: 1920,
        height: 1080,
    };
    c.bench_function("wgc/publish+take+release", |b| {
        b.iter(|| {
            if let Some(slot) = ring.claim() {
                ring.publish(slot, black_box(meta));
            }
            if let Some((slot, m)) = ring.take() {
                black_box(m);
                ring.release(slot as u64);
            }
        })
    });
    c.bench_function("wgc/take_none", |b| b.iter(|| black_box(ring.take())));
}

criterion_group!(benches, handoff);
criterion_main!(benches);
