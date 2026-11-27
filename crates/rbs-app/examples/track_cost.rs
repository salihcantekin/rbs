//! What one to six AAC track encoders cost per audio block (ADR-0170 decisions 7 and
//! 11, stage (c)): a multi-track recording runs one encoder per track in the output's
//! partition, so the encoders together must stay well inside one block's budget
//! (480 frames at 48 kHz, 10 ms).
//!
//! Feeds the same synthetic stereo signal to N encoders on one thread, block by block,
//! for 1 and 6 encoders (and the counts in between), and prints the time per block
//! (mean, p99, max) and its share of the budget. No device and no app.
//!
//! Windows: Media Foundation AAC. macOS: AudioToolbox AAC.
//!
//! ```sh
//! cargo run --release -p rbs-app --example track_cost -- 30
//! ```
#![allow(clippy::disallowed_methods)] // A measurement, not the app shell.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::expect_used)] // Setup errors end it.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use rbs_codec::{AudioEncoderConfig, AudioEncoderFactory};
use rbs_core::media::{AudioBlock, AudioFormat, CodecId, DEFAULT_BLOCK_FRAMES};
use rbs_core::time::Timestamp;

#[cfg(windows)]
fn factory() -> Option<Box<dyn AudioEncoderFactory>> {
    Some(Box::new(rbs_codec_mf::MfAacFactory::new()))
}

#[cfg(target_os = "macos")]
fn factory() -> Option<Box<dyn AudioEncoderFactory>> {
    Some(Box::new(rbs_codec_audiotoolbox::AacFactory::new()))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn factory() -> Option<Box<dyn AudioEncoderFactory>> {
    None
}

/// Per-block times of `tracks` encoders over `seconds` of audio.
fn run(factory: &dyn AudioEncoderFactory, tracks: usize, seconds: u64) -> Vec<Duration> {
    let format = AudioFormat::STEREO_48K;
    let mut encoders: Vec<_> = (0..tracks)
        .map(|_| {
            factory
                .create(&AudioEncoderConfig::new(CodecId::Aac, format, 160_000))
                .expect("an AAC encoder")
        })
        .collect();
    let frames = DEFAULT_BLOCK_FRAMES as usize;
    let block_ns = 1_000_000_000 * frames as u64 / 48_000;
    let blocks = seconds * 1_000_000_000 / block_ns;
    let mut left = vec![0.0f32; frames];
    let mut right = vec![0.0f32; frames];
    let mut packets = Vec::with_capacity(16);
    let mut times = Vec::with_capacity(blocks as usize);
    for b in 0..blocks {
        // A tone with some noise-like content, so the encoders do real work.
        for i in 0..frames {
            let t = (b as usize * frames + i) as f32 / 48_000.0;
            let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.4
                + (t * 3_517.0 * std::f32::consts::TAU).sin() * 0.1;
            left[i] = s;
            right[i] = -s;
        }
        let block = AudioBlock::planar(
            format,
            Timestamp::from_nanos(b * block_ns),
            &[&left, &right],
        )
        .expect("a block");
        let start = Instant::now();
        for e in &mut encoders {
            e.submit(&block).expect("submit");
            packets.clear();
            e.drain(&mut packets).expect("drain");
        }
        times.push(start.elapsed());
    }
    times
}

fn main() -> ExitCode {
    let seconds = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let Some(factory) = factory() else {
        eprintln!("no AAC encoder on this platform");
        return ExitCode::FAILURE;
    };
    let budget = Duration::from_nanos(1_000_000_000 * u64::from(DEFAULT_BLOCK_FRAMES) / 48_000);
    println!("block budget {budget:?}, {seconds} s of audio per run, warm-up run first");
    let _ = run(factory.as_ref(), 1, 2);
    println!("tracks  mean_us  p99_us  max_us  mean_share  p99_share");
    for tracks in 1..=6 {
        let mut times = run(factory.as_ref(), tracks, seconds);
        times.sort_unstable();
        let mean = times.iter().sum::<Duration>() / times.len() as u32;
        let p99 = times[times.len() * 99 / 100];
        let max = times[times.len() - 1];
        let share = |d: Duration| 100.0 * d.as_secs_f64() / budget.as_secs_f64();
        println!(
            "{tracks:>6}  {:>7.1}  {:>6.1}  {:>6.1}  {:>9.2}%  {:>8.2}%",
            mean.as_secs_f64() * 1e6,
            p99.as_secs_f64() * 1e6,
            max.as_secs_f64() * 1e6,
            share(mean),
            share(p99)
        );
    }
    ExitCode::SUCCESS
}
