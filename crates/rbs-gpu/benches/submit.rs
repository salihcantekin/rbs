//! Cost of one queue submit per GPU node versus one submit for the tick.
//!
//! Four empty command buffers stand in for the compositor, a colour pass, a
//! filter and the preview. The iteration waits until the queue is idle, so the
//! number includes the submit and the wait, not a draw.

#![allow(missing_docs)]

use criterion::{criterion_group, criterion_main, Criterion};
use rbs_gpu::{GpuContext, GpuRequest};

const NODES: usize = 4;

fn context() -> Option<GpuContext> {
    match GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) {
        Ok(ctx) => {
            eprintln!("adapter: {}", ctx.adapter_name());
            Some(ctx)
        }
        Err(error) => {
            eprintln!("SKIPPED submit benchmarks: {error}");
            None
        }
    }
}

fn separate(ctx: &GpuContext) {
    for _ in 0..NODES {
        let encoder = ctx.device().create_command_encoder(&Default::default());
        ctx.queue().submit(Some(encoder.finish()));
    }
    ctx.device().poll(wgpu::PollType::wait_indefinitely()).ok();
}

fn together(ctx: &GpuContext) {
    let mut buffers = Vec::with_capacity(NODES);
    for _ in 0..NODES {
        let encoder = ctx.device().create_command_encoder(&Default::default());
        buffers.push(encoder.finish());
    }
    ctx.queue().submit(buffers);
    ctx.device().poll(wgpu::PollType::wait_indefinitely()).ok();
}

fn bench(c: &mut Criterion) {
    let Some(ctx) = context() else {
        return;
    };
    let mut group = c.benchmark_group("gpu_submit");
    group.bench_function("four_separate_submits", |b| b.iter(|| separate(&ctx)));
    group.bench_function("one_submit_of_four", |b| b.iter(|| together(&ctx)));
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
