//! Decodes every picture and sound chunk of a file and prints the cost per picture,
//! for the measurements ADR-0108 decision 4 asks for (1080p60, 4K60).
//!
//! `cargo run --release -p rbs-media-avf --example decode_bench -- <file> [seconds] [copy]`
//!
//! Native `IOSurface` pictures (`zero-copy`) by default; `copy` measures the `Rgba8`
//! copy (`cpu-upload`). Each picture is dropped at once, as the picture node does.
//!
//! Stops after `seconds` of media (default 10). Measure CPU outside the process
//! (`/usr/bin/time -l` on macOS, Task Manager or `Get-Process` on Windows).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use rbs_core::registry::AdapterFactory;
use rbs_media_decode::MediaDecoderFactory;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = std::path::absolute(PathBuf::from(args.next().expect("a file path")))
        .expect("an absolute path");
    let limit = Duration::from_secs(args.next().map_or(10, |s| s.parse().expect("seconds")));
    let factory = if args.next().as_deref() == Some("copy") {
        rbs_media_avf::AvfMediaFactory::cpu_upload()
    } else {
        rbs_media_avf::AvfMediaFactory::new()
    };
    let path =
        rbs_media_decode::accept_local_media_path(&path.to_string_lossy()).expect("a local file");

    let opened = Instant::now();
    let mut dec = factory.open(&path).expect("open");
    let open_time = opened.elapsed();

    let mut times = Vec::new();
    let mut size = (0, 0);
    let mut last_pts = Duration::ZERO;
    let started = Instant::now();
    loop {
        let t = Instant::now();
        let Some(p) = dec.next_picture().expect("picture") else {
            break;
        };
        times.push(t.elapsed());
        size = (p.picture.width, p.picture.height);
        last_pts = p.pts.unwrap_or(last_pts);
        if last_pts >= limit {
            break;
        }
    }
    let pictures_wall = started.elapsed();
    let sound_started = Instant::now();
    let mut sound_frames = 0usize;
    while let Some(s) = dec.next_sound().expect("sound") {
        sound_frames += s.pcm.samples.len() / usize::from(s.pcm.channels.max(1));
        if s.pts.is_some_and(|p| p >= limit) {
            break;
        }
    }
    let sound_wall = sound_started.elapsed();

    times.sort();
    let n = times.len().max(1);
    let mean = times.iter().sum::<Duration>() / n as u32;
    let p99 = times[(n * 99 / 100).min(n - 1)];
    let media = last_pts.as_secs_f64().max(f64::EPSILON);
    println!(
        "adapter      {} ({})",
        factory.id_text(),
        if factory
            .probe()
            .is_ok_and(|c| format!("{c:?}").contains("zero-copy"))
        {
            "zero-copy"
        } else {
            "cpu-upload"
        }
    );
    println!("picture      {}x{}", size.0, size.1);
    println!("open         {open_time:?}");
    println!("pictures     {} over {:.2} s of media", times.len(), media);
    println!(
        "wall         {pictures_wall:?} ({:.1}x real time)",
        media / pictures_wall.as_secs_f64()
    );
    println!(
        "per picture  mean {mean:?}, p99 {p99:?}, max {:?}",
        times[n - 1]
    );
    println!("sound        {sound_frames} frames in {sound_wall:?}");
}

trait IdText {
    fn id_text(&self) -> String;
}

impl<T: rbs_core::registry::AdapterFactory> IdText for T {
    fn id_text(&self) -> String {
        self.id().as_str().to_owned()
    }
}
