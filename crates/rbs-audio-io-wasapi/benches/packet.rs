//! Cost of the work the WASAPI capture thread does per packet besides calling WASAPI:
//! the clock edge, run checks and the push into the sample ring, plus the audio
//! thread's read of the same packet.
//!
//! `cargo bench -p rbs-audio-io-wasapi --bench packet`

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_audio_io_wasapi::bench::Path;
use rbs_core::media::AudioFormat;
use rbs_core::time::Timestamp;

fn packet(c: &mut Criterion) {
    // The default shared-mode period (10 ms) from a stereo device and from an
    // 18-channel interface whose first 8 channels are delivered.
    for (channels, label) in [(2_usize, "stereo"), (18, "18-channel")] {
        let frames = 480;
        let format = if channels == 2 {
            AudioFormat::STEREO_48K
        } else {
            AudioFormat::new(
                rbs_core::timing::SampleRate::HZ_48000,
                rbs_core::media::ChannelLayout::Discrete(8),
            )
            .unwrap_or(AudioFormat::STEREO_48K)
        };
        let mut path = Path::new(format, channels, 16_384);
        let samples = vec![0.25_f32; frames * channels];
        let mut out: Vec<Vec<f32>> = (0..format.channels.channels())
            .map(|_| vec![0.0_f32; frames])
            .collect();
        let mut planes: Vec<&mut [f32]> = out.iter_mut().map(|p| p.as_mut_slice()).collect();
        let period = 10_000_000_u64;
        let mut t = 10_000_000_000_u64;
        c.bench_function(&format!("packet push + read 480 {label} frames"), |b| {
            b.iter(|| {
                t += period;
                path.push_f32(
                    black_box(&samples),
                    frames,
                    t,
                    Timestamp::from_nanos(t + period),
                );
                let read = path.consumer.read(&mut planes);
                black_box(read.map(|r| r.frames).unwrap_or(0))
            })
        });
    }
}

criterion_group!(benches, packet);
criterion_main!(benches);
