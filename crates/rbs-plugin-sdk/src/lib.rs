//! The plugin manifest (`rbs-plugin.toml`, [plugin SDK spec](https://github.com/salihcantekin/rbs/blob/main/docs/specs/plugin-sdk.md), ADR-0006).
//!
//! This crate is the contract a plugin is written against for its identity. It does
//! not load native code, WASM or a process.

use serde::Deserialize;

/// How a plugin runs. The host chooses the loader from this; none of them exist yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// A trusted `cdylib`. Not loaded while the plugin is Installed.
    Native,
    /// A WASM sandbox. Not compiled while the plugin is Installed.
    Sandbox,
    /// A separate process. Not started while the plugin is Installed.
    Process,
}

/// `rbs-plugin.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Reverse-DNS id.
    pub id: String,
    /// The name shown to the user.
    pub name: String,
    /// The plugin's own version.
    pub version: String,
    /// The SDK range it supports (`^1.0`).
    pub sdk: String,
    /// Which host loads it, once loading exists.
    pub tier: Tier,
    /// What it adds (`video-filter`, and the other extension points in the spec).
    pub provides: Vec<String>,
    /// Permissions the user approves at install. Absent means none.
    #[serde(default)]
    pub permissions: Vec<String>,
}

/// Why a manifest was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestError(pub String);

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Manifest {
    /// Parses `rbs-plugin.toml` text. Does not touch the filesystem.
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let manifest: Manifest = toml::from_str(text)
            .map_err(|e| ManifestError(format!("the manifest is not valid ({e})")))?;
        if manifest.id.is_empty() || !manifest.id.contains('.') {
            return Err(ManifestError(
                "id is a reverse-DNS name, such as com.example.chroma".into(),
            ));
        }
        if manifest.name.is_empty() || manifest.version.is_empty() || manifest.sdk.is_empty() {
            return Err(ManifestError("name, version and sdk are required".into()));
        }
        if manifest.provides.is_empty() {
            return Err(ManifestError(
                "provides names at least one extension point".into(),
            ));
        }
        Ok(manifest)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_parses_without_loading_anything() {
        let text = r#"
id = "com.example.chroma"
name = "Chroma"
version = "1.0.0"
sdk = "^1.0"
tier = "sandbox"
provides = ["video-filter"]
permissions = ["gpu-shader"]
"#;
        let m = Manifest::parse(text).unwrap();
        assert_eq!(m.tier, Tier::Sandbox);
        assert_eq!(m.provides, ["video-filter"]);
    }

    #[test]
    fn an_empty_id_is_refused() {
        let text = r#"
id = "chroma"
name = "Chroma"
version = "1.0.0"
sdk = "^1.0"
tier = "native"
provides = ["video-filter"]
"#;
        assert!(Manifest::parse(text).is_err());
    }
}
