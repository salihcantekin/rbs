//! Installed plugins (ADR-0006, ADR-0013, plugin SDK spec).
//!
//! Reading a directory parses each `rbs-plugin.toml` and stops there. Native
//! libraries are not loaded, WASM is not compiled, and no process is started.
//! Activation is a later change. This crate has no loader dependency on purpose.

use std::fs;
use std::path::{Path, PathBuf};

use rbs_plugin_sdk::Manifest;

/// A plugin whose manifest was read and whose code was not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The directory that holds `rbs-plugin.toml`.
    pub directory: PathBuf,
    /// The manifest.
    pub manifest: Manifest,
}

/// Every plugin directory under `root` that has a readable manifest.
///
/// A directory without `rbs-plugin.toml` is skipped. A manifest that does not parse
/// is returned in the error list and does not fail the others. Other files in the
/// directory, including native libraries, are not opened.
pub fn read_installed(root: &Path) -> (Vec<Installed>, Vec<String>) {
    let mut installed = Vec::new();
    let mut errors = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) => {
            errors.push(format!("{} cannot be read ({e})", root.display()));
            return (installed, errors);
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let directory = entry.path();
        if !directory.is_dir() {
            continue;
        }
        let manifest_path = directory.join("rbs-plugin.toml");
        if !manifest_path.is_file() {
            continue;
        }
        match fs::read_to_string(&manifest_path) {
            Ok(text) => match Manifest::parse(&text) {
                Ok(manifest) => installed.push(Installed {
                    directory,
                    manifest,
                }),
                Err(e) => errors.push(format!("{}: {e}", manifest_path.display())),
            },
            Err(e) => errors.push(format!("{}: {e}", manifest_path.display())),
        }
    }
    installed.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
    (installed, errors)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn scratch() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("rbs-plugins-{nanos}-{n}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn twenty_installed_manifests_load_nothing() {
        let root = scratch();
        for i in 0..20 {
            let dir = root.join(format!("plugin-{i}"));
            fs::create_dir(&dir).unwrap();
            fs::write(
                dir.join("rbs-plugin.toml"),
                format!(
                    r#"
id = "com.example.plugin{i}"
name = "Plugin {i}"
version = "1.0.0"
sdk = "^1.0"
tier = "native"
provides = ["video-filter"]
"#
                ),
            )
            .unwrap();
            // A native library sitting beside the manifest must not be opened.
            fs::write(dir.join("plugin.so"), b"not a real library").unwrap();
        }
        let (installed, errors) = read_installed(&root);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(installed.len(), 20);
        for plugin in &installed {
            let library = fs::read(plugin.directory.join("plugin.so")).unwrap();
            assert_eq!(library, b"not a real library");
        }
        let _ = fs::remove_dir_all(&root);
    }
}
