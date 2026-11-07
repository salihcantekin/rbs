//! `rbs-cli vcam ...`: add, check, reformat and remove the RBS virtual camera
//! (ADR-0066, ADR-0070) without the desktop app.

use std::io::Write;

use rbs_core::media::PixelFormat;
use rbs_core::timing::FrameRate;
use rbs_vcam::{CameraFormat, InstallState, VirtualCameraFactory};

use crate::args::{VcamCommand, VcamFormat};

/// The format `f` names, NV12.
pub fn camera_format(f: VcamFormat) -> Result<CameraFormat, String> {
    let rate = FrameRate::new(f.fps, 1).ok_or("not a frame rate")?;
    Ok(CameraFormat::new(
        f.size.width,
        f.size.height,
        rate,
        PixelFormat::Nv12,
    ))
}

/// The camera's state in words.
pub fn describe(state: &InstallState) -> String {
    match state {
        InstallState::Unsupported { reason } => {
            format!("not available on this computer: {reason}")
        }
        InstallState::NotInstalled => "not added".into(),
        InstallState::NeedsUpdate { reason } => format!("update needed: {reason}"),
        InstallState::Installed => "added".into(),
        _ => "unknown".into(),
    }
}

fn set_format(
    factory: &dyn VirtualCameraFactory,
    f: VcamFormat,
    out: &mut dyn Write,
) -> Result<(), String> {
    let applied = factory
        .set_format(&camera_format(f)?)
        .map_err(|e| e.to_string())?;
    let (w, h) = (f.size.width, f.size.height);
    let when = if applied {
        ""
    } else {
        " (applies when no app is using the camera)"
    };
    writeln!(out, "format: {w}x{h} at {} fps{when}", f.fps).map_err(|e| e.to_string())
}

/// Runs `command` on `factory`, `None` on a platform without the camera.
pub fn run(
    factory: Option<&dyn VirtualCameraFactory>,
    command: VcamCommand,
    out: &mut dyn Write,
) -> Result<(), String> {
    let factory = factory.ok_or("the virtual camera is not available on this platform")?;
    let say = |out: &mut dyn Write, state: &InstallState| {
        writeln!(out, "virtual camera: {}", describe(state)).map_err(|e| e.to_string())
    };
    match command {
        VcamCommand::Status => {
            let state = factory.status().map_err(|e| e.to_string())?;
            say(out, &state)
        }
        VcamCommand::Add(f) => {
            // Checked before the administrator prompt.
            camera_format(f)?;
            let state = factory.install().map_err(|e| e.to_string())?;
            say(out, &state)?;
            if state != InstallState::Installed {
                return Err("the camera was not added".into());
            }
            set_format(factory, f, out)
        }
        VcamCommand::Format(f) => set_format(factory, f, out),
        VcamCommand::Remove => {
            let state = factory.uninstall().map_err(|e| e.to_string())?;
            say(out, &state)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use rbs_core::media::Size;
    use rbs_vcam_test::TestVcamFactory;

    use super::*;

    fn text(f: Option<&dyn VirtualCameraFactory>, c: VcamCommand) -> Result<String, String> {
        let mut out = Vec::new();
        run(f, c, &mut out).map(|()| String::from_utf8(out).unwrap())
    }

    #[test]
    fn add_format_and_remove() {
        let f = TestVcamFactory::new();
        assert_eq!(
            text(Some(&f), VcamCommand::Status).unwrap(),
            "virtual camera: not added\n"
        );
        let hd = VcamFormat {
            size: Size::new(1920, 1080),
            fps: 60,
        };
        assert_eq!(
            text(Some(&f), VcamCommand::Add(hd)).unwrap(),
            "virtual camera: added\nformat: 1920x1080 at 60 fps\n"
        );
        assert_eq!(f.format().map(|c| (c.width, c.height)), Some((1920, 1080)));
        f.consumer().set_reading(true);
        assert_eq!(
            text(Some(&f), VcamCommand::Format(VcamFormat::default())).unwrap(),
            "format: 1280x720 at 30 fps (applies when no app is using the camera)\n"
        );
        f.consumer().set_reading(false);
        assert_eq!(
            text(Some(&f), VcamCommand::Remove).unwrap(),
            "virtual camera: not added\n"
        );
    }

    #[test]
    fn no_camera_on_this_platform() {
        assert!(text(None, VcamCommand::Status).is_err());
    }
}
