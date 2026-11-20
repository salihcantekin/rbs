//! Cost of the Media Foundation camera callback's NV12 hand-off at 1080p (capture port
//! spec, fd40c68): the copy of one locked picture into a shared texture slot, the
//! fence signal and the flush, against the system-memory ring's one CPU copy that it
//! replaces. The render thread's upload of that system frame, which the shared texture
//! also removes, is `rbs-gpu`'s `upload_1080p/nv12` benchmark.
//!
//! `cargo bench -p rbs-capture-mf --bench gpu_nv12` (Windows; prints SKIPPED elsewhere).

#![allow(missing_docs)] // criterion_group! generates undocumented items.

use criterion::{Criterion, criterion_group, criterion_main};

const W: u32 = 1920;
const H: u32 = 1080;

#[cfg(windows)]
fn bench(c: &mut Criterion) {
    use rbs_capture_mf::bench::Nv12Ring;
    use std::hint::black_box;

    // Cameras deliver a packed picture: luma rows, then interleaved chroma rows.
    let pitch = W;
    let picture = vec![128_u8; (pitch * (H + H / 2)) as usize];
    let mut system = vec![0_u8; picture.len()];
    let slots = 4;
    let Some(ring) = Nv12Ring::open(W, H, slots) else {
        eprintln!("SKIPPED gpu_nv12: the first adapter cannot share NV12 textures");
        return;
    };
    let mut g = c.benchmark_group("camera_nv12_1080p");
    g.bench_function("system_ring_copy", |b| {
        b.iter(|| {
            system.copy_from_slice(black_box(&picture));
            black_box(system[0])
        })
    });
    let mut slot = 0;
    g.bench_function("shared_texture_write", |b| {
        b.iter(|| {
            slot = (slot + 1) % slots;
            assert!(ring.write(slot, black_box(&picture), pitch, H), "write");
        })
    });
    g.finish();
}

#[cfg(not(windows))]
fn bench(_: &mut Criterion) {
    eprintln!("SKIPPED gpu_nv12: Windows only ({W}x{H})");
}

criterion_group!(benches, bench);
criterion_main!(benches);
