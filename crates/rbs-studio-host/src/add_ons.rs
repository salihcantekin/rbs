//! The built-in add-on catalog and where add-ons are kept (ADR-0145, add-ons spec).
//!
//! The catalog is compiled in: today one entry, the browser engine, from the runner's
//! pin file. Nothing here reads the disk or starts a thread until the studio is asked
//! about an add-on (P13).

use std::path::PathBuf;
use std::sync::Arc;

use rbs_add_ons::{AddOnFs, Entry, Fetch, Unpack};
use rbs_core::time::Clock;

/// The browser engine (CEF runtime) the `rbs-browser-cef` runner loads.
pub const BROWSER_ENGINE: &str = "org.rbs.browser-engine";

/// Every compiled-in catalog entry, as TOML.
pub const BUILT_IN: &[&str] = &[include_str!("../../rbs-browser-cef/cef-pin.toml")];

/// This build's Rust target triple, as the catalog's `[platform.<triple>]` names it;
/// `"unsupported"` where no catalog entry can have an artifact.
pub const fn target() -> &'static str {
    if cfg!(all(
        target_os = "windows",
        target_arch = "x86_64",
        target_env = "msvc"
    )) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "gnu"
    )) {
        "x86_64-unknown-linux-gnu"
    } else {
        "unsupported"
    }
}

/// The built-in catalog for `target`, in order. An entry whose kind this build does
/// not know is skipped; an entry that does not parse is skipped with a log line (the
/// `every_built_in_entry_parses` test keeps that from shipping).
pub fn catalog(target: &str) -> Vec<Entry> {
    BUILT_IN
        .iter()
        .filter_map(|text| match rbs_add_ons::parse_entry(text, target) {
            Ok(entry) => entry,
            Err(e) => {
                rbs_core::error!(
                    rbs_core::log::Area::AddOns,
                    "a built-in add-on entry does not parse: {e}"
                );
                None
            }
        })
        .collect()
}

/// Where add-ons are kept (add-ons spec, "Storage"): `%LOCALAPPDATA%\RBS\add-ons`,
/// `~/Library/Application Support/RBS/add-ons` or `$XDG_DATA_HOME/rbs/add-ons`
/// (default `~/.local/share/rbs/add-ons`). `None` when the environment names no home.
pub fn default_root() -> Option<PathBuf> {
    root_from(|key| std::env::var_os(key).filter(|v| !v.is_empty()))
}

fn root_from(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    if cfg!(windows) {
        env("LOCALAPPDATA").map(|d| PathBuf::from(d).join("RBS").join("add-ons"))
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| {
            PathBuf::from(h)
                .join("Library")
                .join("Application Support")
                .join("RBS")
                .join("add-ons")
        })
    } else if let Some(d) = env("XDG_DATA_HOME") {
        Some(PathBuf::from(d).join("rbs").join("add-ons"))
    } else {
        env("HOME").map(|h| {
            PathBuf::from(h)
                .join(".local")
                .join("share")
                .join("rbs")
                .join("add-ons")
        })
    }
}

/// What the studio needs to list, install and remove add-ons.
#[derive(Clone)]
pub struct AddOnsSetup {
    /// The catalog, in order (`addon.list` order; an entry's place is its
    /// `addon.changed` object id).
    pub catalog: Vec<Entry>,
    /// Where add-ons are kept.
    pub root: PathBuf,
    /// File system operations.
    pub fs: Arc<dyn AddOnFs>,
    /// HTTPS downloads.
    pub fetch: Arc<dyn Fetch>,
    /// Archive decoding.
    pub unpack: Arc<dyn Unpack>,
    /// Throttles progress (a `TestClock` in tests).
    pub clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for AddOnsSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddOnsSetup")
            .field("root", &self.root)
            .field("catalog", &self.catalog.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn every_built_in_entry_parses_with_a_known_kind() {
        for target in [
            "x86_64-pc-windows-msvc",
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-unknown-linux-gnu",
        ] {
            for text in BUILT_IN {
                let entry = rbs_add_ons::parse_entry(text, target);
                assert!(
                    matches!(&entry, Ok(Some(e)) if e.artifact.is_some()),
                    "{target}: {entry:?}"
                );
            }
        }
        let engine = catalog("x86_64-unknown-linux-gnu");
        assert_eq!(engine.len(), 1);
        assert_eq!(engine[0].id.as_str(), BROWSER_ENGINE);
        assert!(catalog("riscv64gc-unknown-linux-gnu")[0].artifact.is_none());
    }

    #[test]
    fn the_root_follows_the_platform() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        let root = root_from(env(&[
            ("LOCALAPPDATA", "C:/Users/z/AppData/Local"),
            ("HOME", "/home/z"),
            ("XDG_DATA_HOME", "/data"),
        ]));
        let want = if cfg!(windows) {
            PathBuf::from("C:/Users/z/AppData/Local/RBS/add-ons")
        } else if cfg!(target_os = "macos") {
            PathBuf::from("/home/z/Library/Application Support/RBS/add-ons")
        } else {
            PathBuf::from("/data/rbs/add-ons")
        };
        assert_eq!(root, Some(want));
        if !cfg!(any(windows, target_os = "macos")) {
            assert_eq!(
                root_from(env(&[("HOME", "/home/z")])),
                Some(PathBuf::from("/home/z/.local/share/rbs/add-ons"))
            );
        }
        assert_eq!(root_from(env(&[])), None);
    }
}
