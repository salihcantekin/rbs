//! Engine side of the cross-process preview demo (ADR-0028).
//!
//! Composites an animated canvas at 60 Hz and shares it through a `PreviewPublisher`.
//! Prints the endpoint (`service#token`) as its first line on stdout, then a status line
//! every few seconds on stderr. Stops when stdin closes (the UI demo holds it open) or
//! after `--seconds N`. `RBS_PREVIEW_SIZE=<w>x<h>` sets the preview size (default
//! 1280x720).
//!
//! ```sh
//! cargo run -p rbs-gpu --release --example preview_engine
//! ```
//!
//! The UI side is `cargo run -p rbs-app --release --example remote_preview`.

#[cfg(target_vendor = "apple")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    demo::run()
}

#[cfg(not(target_vendor = "apple"))]
fn main() {
    eprintln!("preview_engine: cross-process preview is implemented on macOS only (ADR-0028)");
}

#[cfg(target_vendor = "apple")]
mod demo {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use rbs_core::time::Timestamp;
    use rbs_gpu::share::{PreviewDesc, PreviewPublisher};
    use rbs_gpu::{
        CanvasDesc, Compositor, GpuContext, GpuError, GpuRequest, Layer, TextureDesc, TextureId,
        TexturePool, readback,
    };

    const CANVAS: (u32, u32) = (1920, 1080);
    const PREVIEW: (u32, u32) = (1280, 720);
    /// `RBS_PREVIEW_SIZE=<w>x<h>`, or the default.
    fn preview_size() -> (u32, u32) {
        std::env::var("RBS_PREVIEW_SIZE")
            .ok()
            .and_then(|v| {
                let (w, h) = v.split_once('x')?;
                Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))
            })
            .unwrap_or(PREVIEW)
    }

    const FRAME: Duration = Duration::from_nanos(16_666_667);

    /// A 2x2 source of one sRGB colour; the `*Srgb` format decodes it to linear light.
    fn solid(
        ctx: &GpuContext,
        pool: &mut TexturePool,
        rgba: [u8; 4],
    ) -> Result<TextureId, GpuError> {
        let id = pool.acquire(
            ctx,
            &TextureDesc::source(2, 2, wgpu::TextureFormat::Rgba8UnormSrgb),
        )?;
        readback::upload(ctx, pool, id, &rgba.repeat(4))?;
        Ok(id)
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let seconds: Option<u64> = std::env::args()
            .skip_while(|a| a != "--seconds")
            .nth(1)
            .and_then(|s| s.parse().ok());
        let ctx = GpuContext::new(&GpuRequest::default())?;
        let mut pool = TexturePool::new();
        let (cw, ch) = CANVAS;
        let mut comp = Compositor::new(
            &ctx,
            CanvasDesc {
                width: cw,
                height: ch,
            },
            8,
        )?;
        let canvas = pool.acquire(&ctx, &TextureDesc::canvas(cw, ch))?;
        let background = solid(&ctx, &mut pool, [12, 14, 20, 255])?;
        let colours = [
            solid(&ctx, &mut pool, [230, 60, 50, 255])?,
            solid(&ctx, &mut pool, [60, 200, 90, 255])?,
            solid(&ctx, &mut pool, [60, 120, 240, 255])?,
        ];
        let bar = solid(&ctx, &mut pool, [240, 240, 240, 255])?;

        let mut publisher = PreviewPublisher::new(
            &ctx,
            PreviewDesc {
                width: preview_size().0,
                height: preview_size().1,
                // What the UI's preview layer needs at 60 fps (`PreviewLayer::surfaces_needed`):
                // one shown, up to three retiring for 34 ms, one being rendered, one in
                // flight. Slint's texture path needs four of them.
                surfaces: 6,
            },
        )?;
        println!("{}", publisher.endpoint());
        std::io::stdout().flush()?;
        eprintln!(
            "preview_engine: {} on {}, surfaces {:?}",
            publisher.endpoint().service,
            ctx.adapter_name(),
            publisher.surface_ids().collect::<Vec<_>>()
        );

        // Stops when stdin closes, so the engine never outlives the UI that started it.
        let stop = Arc::new(AtomicBool::new(false));
        let stdin_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut sink = [0u8; 64];
            while matches!(std::io::stdin().read(&mut sink), Ok(n) if n > 0) {}
            stdin_stop.store(true, Ordering::Relaxed);
        });

        let start = Instant::now();
        let mut next = start;
        let mut report = start;
        let mut sequence = 0u64;
        while !stop.load(Ordering::Relaxed)
            && seconds.is_none_or(|s| start.elapsed() < Duration::from_secs(s))
        {
            let t = start.elapsed().as_secs_f32();
            let mut layers = [Layer::new(background, 0.0, 0.0, cw as f32, ch as f32); 5];
            for (i, &c) in colours.iter().enumerate() {
                let phase = t * (0.6 + 0.25 * i as f32) + i as f32 * 2.1;
                let size = 220.0 + 40.0 * i as f32;
                let x = (cw as f32 - size) * (0.5 + 0.5 * phase.sin());
                let y = (ch as f32 - size) * (0.5 + 0.5 * (phase * 1.3).cos());
                let mut layer = Layer::new(c, x, y, size, size);
                layer.opacity = 0.85;
                layers[i + 1] = layer;
            }
            // A bar that sweeps once per second shows dropped or repeated frames.
            let frac = t.fract();
            layers[4] = Layer::new(bar, frac * cw as f32, ch as f32 - 24.0, 8.0, 24.0);
            comp.render(&ctx, &pool, &layers, canvas)?;
            sequence += 1;
            match publisher.publish(
                &ctx,
                &pool,
                canvas,
                Timestamp::from_nanos(start.elapsed().as_nanos() as u64),
                sequence,
            ) {
                Ok(()) | Err(GpuError::ExportBusy { .. }) => {}
                Err(e) => return Err(e.into()),
            }
            publisher.tick(&ctx)?;
            ctx.poll();

            if report.elapsed() >= Duration::from_secs(5) {
                report = Instant::now();
                eprintln!(
                    "preview_engine: subscribed {}, held {}, {:?}",
                    publisher.subscribed(),
                    publisher.held(),
                    publisher.stats()
                );
            }
            next += FRAME;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
        Ok(())
    }
}
