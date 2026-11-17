//! Opening the OS privacy pane for a permission kind (ADR-0141 decision 9), with the
//! OS's own opener; nothing is linked.

use rbs_sources::permissions::PermissionKind;

/// The pane's address for `kind`, where the OS has one.
pub fn pane(kind: PermissionKind) -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    {
        return Some(match kind {
            PermissionKind::Camera => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Camera"
            }
            PermissionKind::Microphone => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
            }
            PermissionKind::Screen => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
            PermissionKind::SystemSound => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_AudioCapture"
            }
        });
    }
    #[cfg(windows)]
    {
        return match kind {
            PermissionKind::Camera => Some("ms-settings:privacy-webcam"),
            PermissionKind::Microphone => Some("ms-settings:privacy-microphone"),
            _ => None,
        };
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = kind;
        None
    }
}

/// Opens the pane for `kind`; false when there is none or the opener did not start.
pub fn open(kind: PermissionKind) -> bool {
    let Some(url) = pane(kind) else {
        return false;
    };
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(url).spawn();
    #[cfg(windows)]
    let spawned = std::process::Command::new("explorer").arg(url).spawn();
    #[cfg(not(any(target_os = "macos", windows)))]
    let spawned: std::io::Result<std::process::Child> = {
        let _ = url;
        Err(std::io::ErrorKind::Unsupported.into())
    };
    match spawned {
        Ok(mut child) => {
            // Reaped off the UI thread; it ends at once.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}
