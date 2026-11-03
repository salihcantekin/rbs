#![allow(missing_docs)] // criterion_group! generates undocumented items.
#![allow(clippy::expect_used)] // A benchmark that cannot set up should stop loudly.

//! Cross-process preview benchmarks (spec docs/specs/gpu-compositor.md, "Preview export
//! (cross-process)"). macOS and Windows; elsewhere it prints a skip message.
//!
//! - `preview/publish_cpu`: CPU time of `publish` (record and submit the scale pass).
//! - `preview/tick_cpu`: CPU time of `tick` with one completed frame to send and one
//!   release to receive (two Mach calls, or on Windows one overlapped write and read
//!   collection, plus a non-blocking poll), and the subscriber's
//!   receive and release.
//! - `preview/frame_gpu`: publish and wait for the GPU, the frame time of the pass.
//!
//! The subscriber runs in the same process here; the Mach and pipe calls are the same as across
//! processes.

#[cfg(any(target_vendor = "apple", windows))]
mod shared {
    use std::thread;
    use std::time::{Duration, Instant};

    use criterion::Criterion;
    use rbs_core::time::Timestamp;
    use rbs_gpu::share::{PreviewDesc, PreviewPublisher, PreviewSubscriber};
    use rbs_gpu::{GpuContext, GpuRequest, TextureDesc, TexturePool};

    const CANVAS: (u32, u32) = (1920, 1080);
    const PREVIEW: (u32, u32) = (1280, 720);

    pub fn bench(c: &mut Criterion) {
        let ctx = match GpuContext::new(&GpuRequest::default()) {
            Ok(ctx) => ctx,
            Err(e) => {
                eprintln!("SKIPPED preview benchmarks: {e}");
                return;
            }
        };
        eprintln!("adapter: {}", ctx.adapter_name());
        let mut pool = TexturePool::new();
        let canvas = pool
            .acquire(&ctx, &TextureDesc::canvas(CANVAS.0, CANVAS.1))
            .expect("canvas");
        let mut publisher = PreviewPublisher::new(
            &ctx,
            PreviewDesc {
                width: PREVIEW.0,
                height: PREVIEW.1,
                surfaces: 3,
            },
        )
        .expect("publisher");
        let endpoint = publisher.endpoint().clone();
        let handle =
            thread::spawn(move || PreviewSubscriber::connect(&endpoint, Duration::from_secs(5)));
        while !handle.is_finished() {
            publisher.tick(&ctx).expect("tick");
            thread::yield_now();
        }
        let mut sub = handle.join().expect("join").expect("subscribe");

        let mut g = c.benchmark_group("preview");
        let mut seq = 0u64;
        g.bench_function("publish_cpu", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    seq += 1;
                    let t = Instant::now();
                    publisher
                        .publish(&ctx, &pool, canvas, Timestamp::from_nanos(seq), seq)
                        .expect("publish");
                    total += t.elapsed();
                    ctx.wait_idle().expect("wait");
                    publisher.tick(&ctx).expect("tick");
                    drop(sub.recv(Duration::ZERO).expect("recv"));
                }
                total
            });
        });
        g.bench_function("tick_cpu", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    seq += 1;
                    publisher
                        .publish(&ctx, &pool, canvas, Timestamp::from_nanos(seq), seq)
                        .expect("publish");
                    ctx.wait_idle().expect("wait");
                    let t = Instant::now();
                    publisher.tick(&ctx).expect("tick");
                    let frame = sub.recv(Duration::ZERO).expect("recv").expect("frame");
                    drop(frame);
                    total += t.elapsed();
                }
                total
            });
        });
        g.bench_function("frame_gpu", |b| {
            b.iter(|| {
                seq += 1;
                publisher
                    .publish(&ctx, &pool, canvas, Timestamp::from_nanos(seq), seq)
                    .expect("publish");
                ctx.wait_idle().expect("wait");
                publisher.tick(&ctx).expect("tick");
                drop(sub.recv(Duration::ZERO).expect("recv"));
            });
        });
        g.finish();
        eprintln!("preview stats: {:?}", publisher.stats());
    }
}

#[cfg(any(target_vendor = "apple", windows))]
criterion::criterion_group!(benches, shared::bench);
#[cfg(any(target_vendor = "apple", windows))]
criterion::criterion_main!(benches);

#[cfg(not(any(target_vendor = "apple", windows)))]
fn main() {
    eprintln!("SKIPPED preview benchmarks: macOS and Windows only (ADR-0028, ADR-0035)");
}
