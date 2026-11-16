//! Finding OBS installs, collections and profiles (ADR-0131 decision 3). Runs only when
//! the wizard, the CLI or the control API asks.

use std::fs;
use std::path::{Path, PathBuf};

use crate::ini::Ini;
use crate::{ImportError, MAX_FILE};

/// A collection or profile found in an OBS folder.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The name OBS shows.
    pub name: String,
    /// Its file (a collection) or folder (a profile).
    pub path: PathBuf,
}

/// What an OBS folder holds.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObsInstall {
    /// Scene collections, sorted by name.
    pub collections: Vec<Found>,
    /// Profiles, sorted by name.
    pub profiles: Vec<Found>,
}

/// The OBS folders that exist on this computer: macOS, Windows, Linux and the Linux
/// Flatpak. OBS's portable mode is not detected; the user picks the file instead.
pub fn default_config_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join("Library/Application Support/obs-studio"));
        dirs.push(home.join(".config/obs-studio"));
        dirs.push(home.join(".var/app/com.obsproject.Studio/config/obs-studio"));
    }
    if let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) {
        dirs.push(appdata.join("obs-studio"));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        dirs.push(xdg.join("obs-studio"));
    }
    let mut seen = Vec::new();
    dirs.retain(|d| {
        let keep = d.join("basic").is_dir() && !seen.contains(d);
        if keep {
            seen.push(d.clone());
        }
        keep
    });
    dirs
}

/// Reads a file of at most [`MAX_FILE`] bytes.
pub fn read_limited(path: &Path) -> Result<Vec<u8>, ImportError> {
    let meta = fs::metadata(path).map_err(|e| ImportError::Io(e.kind().to_string()))?;
    if meta.len() > MAX_FILE {
        return Err(ImportError::TooLarge);
    }
    fs::read(path).map_err(|e| ImportError::Io(e.kind().to_string()))
}

/// The display name of a collection file: its `name` key, else the file name.
pub fn collection_name(path: &Path) -> String {
    let from_file = || {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("OBS")
            .to_owned()
    };
    read_limited(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("name")?.as_str().map(str::to_owned))
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(from_file)
}

/// The collections and profiles of an OBS folder (`basic/scenes`, `basic/profiles`).
/// Backup files (`*.bak`) are skipped.
pub fn scan(config_dir: &Path) -> Result<ObsInstall, ImportError> {
    let basic = config_dir.join("basic");
    if !basic.is_dir() {
        return Err(ImportError::NotAnObsFolder);
    }
    let mut install = ObsInstall::default();
    if let Ok(entries) = fs::read_dir(basic.join("scenes")) {
        for e in entries.flatten() {
            let path = e.path();
            let is_json = path
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("json"));
            if !is_json || !path.is_file() {
                continue;
            }
            install.collections.push(Found {
                name: collection_name(&path),
                path,
            });
        }
    }
    if let Ok(entries) = fs::read_dir(basic.join("profiles")) {
        for e in entries.flatten() {
            let path = e.path();
            let ini = path.join("basic.ini");
            if !ini.is_file() {
                continue;
            }
            let name = read_limited(&ini)
                .ok()
                .map(|b| Ini::parse(&String::from_utf8_lossy(&b)))
                .and_then(|i| i.get("General", "Name").map(str::to_owned))
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| e.file_name().to_string_lossy().into_owned());
            install.profiles.push(Found { name, path });
        }
    }
    install.collections.sort_by(|a, b| a.name.cmp(&b.name));
    install.profiles.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(install)
}

/// The profile files of a profile folder: `basic.ini` must exist, the rest may not.
pub fn read_profile(dir: &Path) -> Result<crate::ProfileFiles, ImportError> {
    let opt = |name: &str| {
        let p = dir.join(name);
        if p.is_file() {
            read_limited(&p).map(Some)
        } else {
            Ok(None)
        }
    };
    Ok(crate::ProfileFiles {
        basic_ini: read_limited(&dir.join("basic.ini"))?,
        service_json: opt("service.json")?,
        stream_encoder: opt("streamEncoder.json")?,
        record_encoder: opt("recordEncoder.json")?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn scan_lists_collections_and_profiles_and_skips_backups() {
        let root = std::env::temp_dir().join(format!("rbs-obs-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("basic/scenes")).unwrap();
        fs::create_dir_all(root.join("basic/profiles/Untitled")).unwrap();
        fs::write(root.join("basic/scenes/Show.json"), r#"{"name":"My show"}"#).unwrap();
        fs::write(root.join("basic/scenes/Show.json.bak"), r#"{"name":"Old"}"#).unwrap();
        fs::write(root.join("basic/scenes/Bare.json"), "{}").unwrap();
        fs::write(
            root.join("basic/profiles/Untitled/basic.ini"),
            "[General]\nName=Streaming\n",
        )
        .unwrap();
        let found = scan(&root).unwrap();
        let names: Vec<_> = found.collections.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Bare", "My show"]);
        assert_eq!(found.profiles[0].name, "Streaming");
        assert!(scan(&root.join("nothing")).is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
