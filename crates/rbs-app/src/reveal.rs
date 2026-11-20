//! Showing a file in the system's file manager (Show in folder).

use std::path::Path;

/// Opens the folder of `path` in the system's file manager with the file selected where
/// the system can. When `path` does not exist, opens the nearest folder above it that
/// does (a tool's configuration folder before the tool made its file). Does nothing
/// when no such folder exists.
pub fn show_in_folder(path: &Path) {
    let spawned = if path.is_file() {
        select(path)
    } else {
        match path.ancestors().find(|p| p.is_dir()) {
            Some(dir) => open(dir),
            None => return,
        }
    };
    if let Ok(mut child) = spawned {
        // Reaped off the UI thread; it ends at once.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

fn select(path: &Path) -> std::io::Result<std::process::Child> {
    #[cfg(target_os = "macos")]
    return std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(windows)]
    return std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
    #[cfg(not(any(target_os = "macos", windows)))]
    return open(path.parent().unwrap_or(path));
}

fn open(dir: &Path) -> std::io::Result<std::process::Child> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(not(any(target_os = "macos", windows)))]
    let program = "xdg-open";
    std::process::Command::new(program).arg(dir).spawn()
}

/// Opens `url` in the default browser; false when the opener did not start.
pub fn open_url(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(url).spawn();
    // `explorer <url>` hands it to the default browser without a console window.
    #[cfg(windows)]
    let spawned = std::process::Command::new("explorer").arg(url).spawn();
    #[cfg(not(any(target_os = "macos", windows)))]
    let spawned = std::process::Command::new("xdg-open").arg(url).spawn();
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
