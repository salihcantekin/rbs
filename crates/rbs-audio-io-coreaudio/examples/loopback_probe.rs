//! TEMPORARY diagnostic for #109: reads the default output's loopback for 8 s and
//! prints what the IO proc saw each second.

#[cfg(target_os = "macos")]
fn main() {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use rbs_audio_io::{AudioBackend, EndpointKind, InputRequest};
    use rbs_audio_io_coreaudio::{CoreAudioBackend, DIAG};
    use rbs_core::time::MonotonicClock;

    let backend = CoreAudioBackend::new();
    let endpoints = backend.enumerate(EndpointKind::OutputLoopback).expect("enumerate");
    for e in &endpoints {
        println!("endpoint {} {:?}", e.id.as_str(), e.name);
    }
    let id = std::env::args()
        .nth(1)
        .map(rbs_audio_io::EndpointId::new)
        .unwrap_or_else(|| endpoints[0].id.clone());
    println!("probing {}", id.as_str());
    let clock = Arc::new(MonotonicClock::new());
    let mut stream = backend.open_input(&InputRequest::new(id, clock)).expect("open");
    stream.start().expect("start");
    let mut l = vec![0.0_f32; 48_000];
    let mut r = vec![0.0_f32; 48_000];
    for second in 0..8 {
        let mut frames = 0;
        let mut peak = 0.0_f32;
        let mut first = None;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(10));
            if let Ok(read) = stream.read(&mut [&mut l, &mut r]) {
                if read.frames > 0 && first.is_none() {
                    first = Some((read.timestamp, read.discontinuity));
                }
                frames += read.frames;
                peak = l[..read.frames].iter().fold(peak, |p, s| p.max(s.abs()));
            }
        }
        let d: Vec<u64> = DIAG.iter().map(|a| a.load(Ordering::Relaxed)).collect();
        println!(
            "t={second}s read={frames} peak={peak:.4} first={first:?} stats={:?} calls={} nbuf={} tapch={} tapbytes={} tapnull={} skipped={} pushed={} flags={:#x}",
            stream.stats(), d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        );
    }
    stream.stop();
}

#[cfg(not(target_os = "macos"))]
fn main() {}
