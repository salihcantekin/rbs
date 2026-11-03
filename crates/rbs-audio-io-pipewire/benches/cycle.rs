//! Cost of the work the PipeWire process callback does per graph cycle besides calling
//! PipeWire: the clock edge, run pacing and the push into the sample ring, plus the
//! audio thread's read of the same packet.
//!
//! `cargo bench -p rbs-audio-io-pipewire --bench cycle`

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_audio_io_pipewire::bench::Cycle;
use rbs_core::media::AudioFormat;
use rbs_core::time::Timestamp;

fn cycle(c: &mut Criterion) {
    // The default PipeWire quantum (1024 frames, 21.3 ms) and a 10 ms one.
    for frames in [1_024_usize, 480] {
        let mut cycle = Cycle::new(AudioFormat::STEREO_48K, 16_384);
        let left = vec![0.25_f32; frames];
        let right = vec![-0.25_f32; frames];
        let mut out_left = vec![0.0_f32; frames];
        let mut out_right = vec![0.0_f32; frames];
        let period = frames as u64 * 1_000_000_000 / 48_000;
        let mut os = 10_000_000_000_u64;
        c.bench_function(&format!("cycle push + read {frames} stereo frames"), |b| {
            b.iter(|| {
                os += period;
                cycle.push(
                    black_box(&[&left, &right]),
                    os,
                    os + 50_000,
                    Timestamp::from_nanos(os + 50_000),
                );
                let read = cycle.consumer.read(&mut [&mut out_left, &mut out_right]);
                black_box(read.map(|r| r.frames).unwrap_or(0))
            })
        });
    }
}

criterion_group!(benches, cycle);
criterion_main!(benches);
