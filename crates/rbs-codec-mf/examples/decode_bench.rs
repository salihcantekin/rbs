//! Milliseconds per picture of the Media Foundation live H.264 decoder at 1080p30,
//! for PR notes: `cargo run --release -p rbs-codec-mf --features testing --example
//! decode_bench`. Encodes a 1080p Baseline clip at start (Microsoft's H.264 encoder),
//! then times `submit` + `drain` per packet, for system-memory output and for native
//! output on the first DXGI adapter. Windows only.

#[cfg(windows)]
fn main() {
    use std::time::Instant;

    use rbs_codec::{DecodeOutput, VideoDecoder, VideoDecoderConfig};
    use rbs_codec_mf::MfH264DecoderFactory;
    use rbs_codec_mf::testing::h264;
    use rbs_core::media::{MemoryKind, Size};
    use rbs_core::time::Timestamp;

    const FRAMES: usize = 120;
    let packets = match h264::clip(1920, 1080, FRAMES, Timestamp::from_nanos(7), 33_333_337) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("no H.264 encoder here: {e}");
            return;
        }
    };
    let factory = MfH264DecoderFactory::new();
    let mut targets = vec![("system", DecodeOutput::system())];
    if let Some(luid) = h264::default_adapter_luid() {
        targets.push((
            "d3d11",
            DecodeOutput::system()
                .accepting(MemoryKind::D3d11)
                .on_adapter(luid),
        ));
    }
    for (name, target) in targets {
        let config = VideoDecoderConfig::h264_annex_b(Size::new(1920, 1088)).with_target(target);
        let Ok(mut d) = factory.create_session(&config) else {
            eprintln!("{name}: create failed");
            continue;
        };
        let mut out = Vec::with_capacity(8);
        let mut ms = Vec::with_capacity(FRAMES);
        let created = Instant::now();
        for p in &packets {
            let t = Instant::now();
            let submitted = d.submit(p);
            let drained = d.drain(&mut out);
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
            if submitted.is_err() || drained.is_err() {
                eprintln!("{name}: {submitted:?} {drained:?}");
            }
            out.clear(); // releases leases at once
        }
        let total = created.elapsed().as_secs_f64() * 1000.0;
        let mut sorted = ms.clone();
        sorted.sort_by(f64::total_cmp);
        let pick = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize];
        let mean = ms.iter().sum::<f64>() / ms.len() as f64;
        let st = d.stats();
        println!(
            "{name:>6} native={} pictures={} cpu_copies={} dropped={}: mean {mean:.2} ms, p50 {:.2}, p95 {:.2}, max {:.2}, first {:.2}, total {total:.0} ms for {FRAMES}",
            d.is_native(),
            st.pictures,
            st.cpu_copies,
            st.dropped_no_surface,
            pick(0.5),
            pick(0.95),
            pick(1.0),
            ms[0],
        );
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Media Foundation is only available on Windows");
}
