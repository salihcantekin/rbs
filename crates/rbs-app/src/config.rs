//! Where the desktop app keeps its files.

use std::path::{Path, PathBuf};

/// The app's configuration directory (shared with `rbs-cli`, which manages access
/// tokens in the same place).
pub use rbs_remote_settings::config_dir;

/// Directory of workspace files.
pub fn workspace_dir(config: &Path) -> PathBuf {
    config.join("workspaces")
}

fn last_workspace_file(config: &Path) -> PathBuf {
    config.join("last-workspace")
}

/// Name of the workspace shown when the app last ran.
pub fn last_workspace(config: &Path) -> Option<String> {
    let text = std::fs::read_to_string(last_workspace_file(config)).ok()?;
    let name = text.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Remember the workspace shown now.
pub fn set_last_workspace(config: &Path, name: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    std::fs::write(last_workspace_file(config), name)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn last_workspace_round_trip() {
        let d = std::env::temp_dir().join(format!("rbs-app-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(last_workspace(&d), None);
        set_last_workspace(&d, "Minimal").unwrap();
        assert_eq!(last_workspace(&d).as_deref(), Some("Minimal"));
        assert_eq!(workspace_dir(&d), d.join("workspaces"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
