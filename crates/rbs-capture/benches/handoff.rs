//! Cost of the lock-free hand-off between an OS capture callback thread and the render
//! thread: publishing a slot (producer side) and taking plus releasing it (render side).
//! The OS-specific parts (GPU copy, fence signal, OS callbacks) are outside this
//! measurement.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_capture::handoff::{FrameMeta, Ring};

fn handoff(c: &mut Criterion) {
    let ring = Ring::new(&[1, 2, 3, 4, 5]);
    let meta = FrameMeta {
        timestamp: 1,
        sequence: 1,
        source_time: 1,
        sync_value: 1,
        width: 1920,
        height: 1080,
    };
    c.bench_function("handoff/publish+take+release", |b| {
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
    c.bench_function("handoff/take_none", |b| b.iter(|| black_box(ring.take())));
}

criterion_group!(benches, handoff);
criterion_main!(benches);
