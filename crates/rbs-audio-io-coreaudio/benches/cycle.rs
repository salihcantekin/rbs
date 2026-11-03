//! Cost of the work the CoreAudio IO proc does per cycle besides calling CoreAudio:
//! the clock edge, run checks and the push into the sample ring, plus the audio
//! thread's read of the same cycle.
//!
//! `cargo bench -p rbs-audio-io-coreaudio --bench cycle`

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_audio_io_coreaudio::bench::Path;
use rbs_core::media::AudioFormat;
use rbs_core::time::Timestamp;

fn cycle(c: &mut Criterion) {
    // The default HAL buffer (512 frames) and a small one (64 frames), stereo.
    for frames in [512_usize, 64] {
        let mut path = Path::new(AudioFormat::STEREO_48K, 16_384);
        let samples = vec![0.25_f32; frames * 2];
        let mut left = vec![0.0_f32; frames];
        let mut right = vec![0.0_f32; frames];
        let period = frames as u64 * 1_000_000_000 / 48_000;
        let mut host = 10_000_000_000_u64;
        let mut sample = 0.0_f64;
        c.bench_function(&format!("cycle push + read {frames} stereo frames"), |b| {
            b.iter(|| {
                host += period;
                sample += frames as f64;
                path.push(
                    black_box(&[(&samples[..], 2)]),
                    frames,
                    host,
                    sample,
                    Timestamp::from_nanos(host + period),
                );
                let read = path.consumer.read(&mut [&mut left, &mut right]);
                black_box(read.map(|r| r.frames).unwrap_or(0))
            })
        });
    }
}

criterion_group!(benches, cycle);
criterion_main!(benches);
