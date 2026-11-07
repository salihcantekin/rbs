//! Filter node cost at 1920x1080 (spec `video-filters.md`, "Performance requirements").
//!
//! `record_submit`: one render tick of feed -> filter -> probe, CPU time to record and
//! submit. `frame`: the same tick and a wait for the GPU. The values alternate between
//! two sets every tick so the filter renders each time; `unchanged_input` is a tick on
//! the same input and values (the skip path: one `touch` per target). Skips without
//! an adapter.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_filters::conformance::{Picture, Rig};
use rbs_filters::{FieldKind, Value, Values, catalogue};
use rbs_gpu::{GpuContext, GpuError, GpuRequest};

fn bench(c: &mut Criterion) {
    let ctx = match GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) {
        Ok(ctx) => Arc::new(ctx),
        Err(GpuError::NoAdapter) => {
            eprintln!("SKIPPED: no GPU adapter");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let (w, h) = (1920, 1080);
    for ty in catalogue() {
        let name = ty
            .type_id()
            .as_str()
            .trim_start_matches("org.rbs.filter.")
            .to_owned();
        let values = Values::defaults(*ty);
        let other = ty
            .fields()
            .iter()
            .find_map(|f| match f.kind {
                FieldKind::Number { min, max, .. } => Some((f.key, (min + max) / 2.0)),
                _ => None,
            })
            .map(|(k, v)| values.with(*ty, [(k, Value::Number(v))]).unwrap())
            .unwrap();
        let mut flip = false;
        let mut next = move || {
            flip = !flip;
            if flip { other } else { values }
        };
        let mut rig = Rig::new(Arc::clone(&ctx), Arc::new(*ty), values, Some([w, h])).unwrap();
        rig.set_input(Picture::solid(w, h, [0.2, 0.6, 0.3, 1.0]));
        for _ in 0..3 {
            rig.tick();
        }
        ctx.wait_idle().unwrap();
        c.bench_function(&format!("{name}/unchanged_input"), |b| {
            b.iter(|| {
                rig.tick();
                black_box(());
            });
        });
        c.bench_function(&format!("{name}/record_submit"), |b| {
            b.iter(|| {
                rig.set_values(0, next()).unwrap();
                rig.tick();
                black_box(());
            });
            ctx.wait_idle().unwrap();
        });
        c.bench_function(&format!("{name}/frame"), |b| {
            b.iter(|| {
                rig.set_values(0, next()).unwrap();
                rig.tick();
                ctx.wait_idle().unwrap();
            });
        });
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20);
    targets = bench
}
criterion_main!(benches);
