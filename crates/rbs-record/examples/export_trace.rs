//! A diagnostic of *Test this computer*'s export part (ADR-0087 step D2): runs
//! `run_exports` with the real rig on a throwaway device of this computer's GPU, paced
//! in real time as in a run, and prints each class's result. Nothing is written.
//!
//! ```sh
//! cargo run --release -p rbs-record --example export_trace -- [fps]
//! ```
//!
//! Classes: 1080 NV12, 2160 NV12 and, when the device supports P010, 1080 P010.

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::expect_used)] // A demo: setup errors end it.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rbs_core::media::ColorSpace;
use rbs_core::time::{Clock, MonotonicClock, Timestamp};
use rbs_core::timing::FrameRate;
use rbs_gpu::{GpuContext, GpuRequest, YuvFormat};
use rbs_record::calibration::{
    CalibrationEvent, ExportClass, ExportRig, GpuExportRig, Pacer, RigError, run_exports,
};
use rbs_record::estimate::SizeClass;

struct RealPacer(Arc<dyn Clock>);

impl Pacer for RealPacer {
    fn yield_now(&self) {
        std::thread::yield_now();
    }

    fn wait_until(&self, t: Timestamp) {
        let left = t.saturating_since(self.0.now());
        if !left.is_zero() {
            std::thread::sleep(left);
        }
    }
}

fn main() {
    let fps: u32 = std::env::args()
        .nth(1)
        .map_or(60, |a| a.parse().expect("fps is a whole number"));
    let rate = FrameRate::new(fps, 1).expect("a valid frame rate");
    let ctx = GpuContext::new(&GpuRequest::default()).expect("a GPU");
    println!("adapter: {} ({})", ctx.adapter_name(), ctx.adapter_key());
    let p010 = ctx.capabilities().get(&rbs_gpu::context::CAP_P010)
        == Some(&rbs_core::capability::CapabilityValue::Flag(true));
    let class = |size, width, height, format| ExportClass {
        size,
        width,
        height,
        format,
        color: ColorSpace::BT709_LIMITED,
        open_shadows: false,
    };
    let mut classes = vec![
        class(SizeClass::L2160, 3840, 2160, YuvFormat::Nv12),
        class(SizeClass::L1080, 1920, 1080, YuvFormat::Nv12),
    ];
    if p010 {
        classes.push(class(SizeClass::L1080, 1920, 1080, YuvFormat::P010));
    }
    let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::new());
    let pacer = RealPacer(Arc::clone(&clock));
    for c in &classes {
        let started = std::time::Instant::now();
        let mut open = || -> Result<Box<dyn ExportRig>, RigError> {
            let sibling = ctx.sibling().map_err(|_| RigError::Failed)?;
            Ok(Box::new(GpuExportRig::new(Arc::new(sibling), rate)))
        };
        let (rig, end) = run_exports(
            std::slice::from_ref(c),
            rate.frame_duration(),
            &mut open,
            &mut || ctx.metrics().device_lost(),
            clock.as_ref(),
            &pacer,
            &AtomicBool::new(false),
            clock.now(),
            &mut |e| match e {
                CalibrationEvent::Export(r) => println!(
                    "{:?} {:?} {}x{}: time_us {} gpu_us {:?} kept_up {}",
                    r.class.size,
                    r.class.format,
                    r.class.width,
                    r.class.height,
                    r.time.as_micros(),
                    r.gpu.map(|g| g.as_micros()),
                    r.kept_up
                ),
                CalibrationEvent::ExportSkipped(s) => println!("skipped: {s:?}"),
                _ => {}
            },
        );
        drop(rig);
        println!("  end {end:?}, {:.2} s", started.elapsed().as_secs_f64());
    }
}
