//! Per-frame cost of the picture mapping hand-off (ADR-0165 decision 5) that the
//! capture source node adds: the switch check when off, and publishing a mapping into
//! the source entry's cell plus a filter's read of it when on.

#![allow(missing_docs)] // Benchmark harness.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_capture::{MappingCell, MappingSwitch, PictureMapping};
use rbs_core::media::Size;
use rbs_core::time::Timestamp;

fn mapping(c: &mut Criterion) {
    let switch = MappingSwitch::new();
    c.bench_function("mapping/switch_off", |b| {
        b.iter(|| black_box(black_box(&switch).is_on()))
    });
    let cell = MappingCell::new();
    let m = PictureMapping::new(
        Timestamp::from_nanos(1),
        [-1920.0, 0.0],
        [2.0, 2.0],
        Size::new(3840, 2160),
    );
    c.bench_function("mapping/publish", |b| {
        b.iter(|| black_box(&cell).publish(black_box(Some(m))))
    });
    c.bench_function("mapping/get", |b| {
        b.iter(|| black_box(black_box(&cell).get()))
    });
}

criterion_group!(benches, mapping);
criterion_main!(benches);
