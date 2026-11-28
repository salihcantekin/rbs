//! Hot-path numbers for the Opus adapter: one 20 ms stereo packet encoded, decoded and
//! concealed. Run `cargo bench -p rbs-codec-opus` on release.

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_codec::{AudioDecoderConfig, AudioDecoderFactory, AudioEncoderConfig, AudioEncoderFactory};
use rbs_codec_opus::{OpusDecoderFactory, OpusEncoderFactory};
use rbs_core::media::{AudioBlock, AudioFormat, CodecId};
use rbs_core::time::Timestamp;

const FRAMES: usize = 960;

fn signal() -> (Vec<f32>, Vec<f32>) {
    let mut seed = 0x1234_5678_u32;
    let mut noise = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / 8_388_608.0 - 1.0
    };
    let left = (0..FRAMES)
        .map(|n| (n as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3 + noise() * 0.02)
        .collect();
    let right = (0..FRAMES)
        .map(|n| (n as f32 * 660.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3 + noise() * 0.02)
        .collect();
    (left, right)
}

fn bench(c: &mut Criterion) {
    let (left, right) = signal();
    for complexity in ["5", "9"] {
        let config = AudioEncoderConfig::new(CodecId::Opus, AudioFormat::STEREO_48K, 128_000)
            .with_adapter_option("complexity", complexity);
        let Ok(mut encoder) = OpusEncoderFactory.create(&config) else {
            return;
        };
        let mut packets = Vec::with_capacity(8);
        let mut tick = 0_u64;
        c.bench_function(&format!("encode_20ms_stereo_128k_c{complexity}"), |b| {
            b.iter(|| {
                tick += 1;
                let ts = Timestamp::from_nanos(tick * 20_000_000);
                let Ok(block) = AudioBlock::planar(AudioFormat::STEREO_48K, ts, &[&left, &right])
                else {
                    return;
                };
                let _ = encoder.submit(&block);
                packets.clear();
                let _ = encoder.drain(&mut packets);
            });
        });
    }

    let config = AudioEncoderConfig::new(CodecId::Opus, AudioFormat::STEREO_48K, 128_000);
    let Ok(mut encoder) = OpusEncoderFactory.create(&config) else {
        return;
    };
    let block = AudioBlock::planar(
        AudioFormat::STEREO_48K,
        Timestamp::from_nanos(0),
        &[&left, &right],
    );
    let mut packets = Vec::new();
    if let Ok(block) = block {
        let _ = encoder.submit(&block);
        let _ = encoder.drain(&mut packets);
    }
    let Ok(mut decoder) = OpusDecoderFactory.create(&AudioDecoderConfig::new(
        CodecId::Opus,
        AudioFormat::STEREO_48K,
    )) else {
        return;
    };
    let mut out = Vec::with_capacity(decoder.max_frames() * 2);
    if let Some(packet) = packets.first() {
        c.bench_function("decode_20ms_stereo", |b| {
            b.iter(|| {
                let _ = decoder.decode(packet, &mut out);
            });
        });
    }
    c.bench_function("conceal_20ms_stereo", |b| {
        b.iter(|| {
            let _ = decoder.conceal(FRAMES, &mut out);
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
