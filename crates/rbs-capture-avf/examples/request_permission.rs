//! Asks macOS for camera access once, so the hardware tests can run.
//!
//! The hardware tests never ask (a prompt would block an unattended run). Run this
//! from the terminal you will test in; macOS shows the camera prompt for that terminal
//! app, and the answer is remembered:
//!
//! ```sh
//! cargo run -p rbs-capture-avf --example request_permission
//! RBS_CAPTURE_HW=1 cargo test --release -p rbs-capture-avf
//! ```

use rbs_capture::{CaptureFactory, Permission, TargetKind};
use rbs_capture_avf::AvfCaptureFactory;

fn main() {
    let factory = AvfCaptureFactory::new();
    match factory.request_permission(TargetKind::Camera) {
        Ok(Permission::Granted) => println!("camera permission: granted"),
        Ok(Permission::Denied) => {
            println!(
                "camera permission: denied. Allow this terminal in System Settings > Privacy & Security > Camera, then run again."
            );
            std::process::exit(1);
        }
        Ok(other) => {
            println!("camera permission: {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("camera permission request failed: {e}");
            std::process::exit(1);
        }
    }
}
