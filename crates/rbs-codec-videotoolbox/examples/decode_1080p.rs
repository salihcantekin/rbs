//! Measures the VideoToolbox live H.264 decoder at 1080p30: milliseconds per picture
//! (`submit` plus `drain`, the worker's whole cost) for native IOSurface output and for
//! the system-memory copy. The clip is encoded first with this crate's encoder (High
//! profile, no B-frames, Annex B, SPS and PPS in band), so nothing is committed.
//!
//! `cargo run --release -p rbs-codec-videotoolbox --example decode_1080p [frames]`
//!
//! Each picture is released at once, as the studio does after its GPU work.

#[cfg(target_os = "macos")]
#[path = "../tests/decode_support/mod.rs"]
mod decode_support;

#[cfg(target_os = "macos")]
fn main() {
    use std::time::Instant;

    use rbs_codec::{DecodeOutput, VideoDecoder, VideoDecoderConfig};
    use rbs_codec_videotoolbox::VideoToolboxDecoderFactory;
    use rbs_core::media::{MemoryKind, Size};

    let frames: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(300);
    let packets = match decode_support::clip(
        1920,
        1080,
        frames,
        decode_support::FIRST,
        decode_support::STEP_NS,
    ) {
        Ok(p) => p,
        Err(e) => {
            println!("no H.264 encoder: {e}");
            return;
        }
    };
    let bytes: usize = packets.iter().map(|p| p.data().len()).sum();
    println!(
        "1080p30 clip: {} packets, {:.1} kB per packet",
        packets.len(),
        bytes as f64 / packets.len() as f64 / 1000.0
    );
    let targets = [
        (
            "native (IOSurface)",
            DecodeOutput::system().accepting(MemoryKind::Metal),
        ),
        ("system (NV12 copy)", DecodeOutput::system()),
    ];
    for (name, target) in targets {
        let config = VideoDecoderConfig::h264_annex_b(Size::new(1920, 1088)).with_target(target);
        let Ok(mut d) = VideoToolboxDecoderFactory::new().create_session(&config) else {
            println!("{name}: no decoder");
            continue;
        };
        let mut out = Vec::with_capacity(4);
        let mut ms = Vec::with_capacity(packets.len());
        for p in &packets {
            let start = Instant::now();
            if let Err(e) = d.submit(p) {
                println!("{name}: submit failed: {e}");
                break;
            }
            let _ = d.drain(&mut out);
            ms.push(start.elapsed().as_secs_f64() * 1000.0);
            out.clear();
        }
        let _ = d.flush(&mut out);
        if ms.is_empty() {
            continue;
        }
        let mean = ms.iter().sum::<f64>() / ms.len() as f64;
        let slowest = (0..ms.len())
            .max_by(|&a, &b| ms[a].total_cmp(&ms[b]))
            .unwrap_or(0);
        let first = ms[0];
        ms.sort_by(f64::total_cmp);
        let pick = |q: f64| ms[((ms.len() - 1) as f64 * q).round() as usize];
        let st = d.stats();
        println!(
            "{name}: hardware {}, {} pictures, ms per picture mean {mean:.3} p50 {:.3} p95 {:.3} max {:.3} at picture {slowest} (first {first:.3} includes session creation), cpu_copies {}",
            d.is_hardware(),
            st.pictures,
            pick(0.5),
            pick(0.95),
            pick(1.0),
            st.cpu_copies
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("VideoToolbox is only available on macOS");
}
