//! Catalog entries ([add-ons spec], "Catalog entry").
//!
//! The built-in catalog is a set of TOML entries compiled into the app. An entry for
//! a code-bearing kind pins, per platform, the URL, the exact size, the SHA-256 and
//! the hosts a download may reach (ADR-0145 decision 3).
//!
//! [add-ons spec]: https://github.com/salihcantekin/rbs/blob/main/docs/specs/add-ons.md

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use crate::archive::Format;
use crate::id::AddOnId;
use crate::names::RelPath;
use crate::version::Version;

/// Largest `strip` an entry may ask for.
const MAX_STRIP: u8 = 8;

/// What an add-on is and how it is installed. The set is closed per RBS version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An archive unpacked into a versioned directory that one consumer loads.
    Runtime,
}

/// The artifact of an entry for one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// The `https` URL of the archive.
    pub url: String,
    /// Exact archive size in bytes.
    pub size: u64,
    /// SHA-256 of the archive.
    pub sha256: [u8; 32],
    /// Bytes the unpacked files may take, for the free-space check and the cap.
    pub unpacked_size: u64,
    /// Archive format.
    pub format: Format,
    /// Leading components removed from every entry name.
    pub strip: u8,
    /// Directory (after strip) that holds the library and the files in `required`.
    pub library_dir: RelPath,
    /// Directory (after strip) that holds the other resources.
    pub resources_dir: RelPath,
    /// Files that must be in `library_dir` after install. One that is only in
    /// `resources_dir` is moved into `library_dir`.
    pub required: Vec<RelPath>,
}

/// One catalog entry, read for this computer's target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Stable id, also the directory name.
    pub id: AddOnId,
    /// Kind.
    pub kind: Kind,
    /// Name shown in Settings.
    pub name: String,
    /// One-line description.
    pub summary: String,
    /// The version consumers load.
    pub version: Version,
    /// Every host a request or a redirect may reach, lowercase.
    pub hosts: Vec<String>,
    /// URL of the license notices and sources.
    pub notices: String,
    /// The artifact for this target; `None` when the add-on is not available here.
    pub artifact: Option<Artifact>,
}

/// A catalog entry that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// Not valid TOML for an entry.
    Toml(String),
    /// Valid TOML that breaks a rule.
    Invalid(String),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(e) => write!(f, "catalog entry is not valid TOML: {e}"),
            Self::Invalid(e) => write!(f, "invalid catalog entry: {e}"),
        }
    }
}

impl std::error::Error for CatalogError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    id: String,
    kind: String,
    name: String,
    summary: String,
    version: String,
    hosts: Vec<String>,
    notices: String,
    #[serde(default)]
    platform: BTreeMap<String, RawArtifact>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArtifact {
    url: String,
    size: u64,
    sha256: String,
    unpacked_size: u64,
    format: String,
    #[serde(default)]
    strip: u8,
    library_dir: String,
    resources_dir: String,
    #[serde(default)]
    required: Vec<String>,
}

/// Parses one entry for `target` (a Rust target triple such as
/// `x86_64-pc-windows-msvc`).
///
/// An entry whose `kind` this build does not know is `Ok(None)`: the caller skips
/// it and the other entries still load.
pub fn parse_entry(text: &str, target: &str) -> Result<Option<Entry>, CatalogError> {
    let raw: RawEntry = toml::from_str(text).map_err(|e| CatalogError::Toml(e.to_string()))?;
    let kind = match raw.kind.as_str() {
        "runtime" => Kind::Runtime,
        _ => return Ok(None),
    };
    let id = AddOnId::new(&raw.id)?;
    let invalid = |why: String| CatalogError::Invalid(format!("{id}: {why}"));
    let version = Version::parse(&raw.version)
        .ok_or_else(|| invalid(format!("version `{}` is not dotted numbers", raw.version)))?;
    if raw.name.trim().is_empty() || raw.summary.trim().is_empty() {
        return Err(invalid("name and summary must not be empty".into()));
    }
    let hosts = raw
        .hosts
        .iter()
        .map(|h| check_host(h).map_err(&invalid))
        .collect::<Result<Vec<_>, _>>()?;
    if hosts.is_empty() {
        return Err(invalid("hosts must not be empty".into()));
    }
    https_host(&raw.notices).map_err(&invalid)?;
    let artifact = raw
        .platform
        .get(target)
        .map(|a| parse_artifact(a, &hosts).map_err(&invalid))
        .transpose()?;
    Ok(Some(Entry {
        id,
        kind,
        name: raw.name,
        summary: raw.summary,
        version,
        hosts,
        notices: raw.notices,
        artifact,
    }))
}

fn parse_artifact(raw: &RawArtifact, hosts: &[String]) -> Result<Artifact, String> {
    let host = https_host(&raw.url)?;
    if !hosts.contains(&host) {
        return Err(format!("the host of `{}` is not in `hosts`", raw.url));
    }
    if raw.size == 0 || raw.unpacked_size == 0 {
        return Err("size and unpacked_size must be above zero".into());
    }
    if raw.strip > MAX_STRIP {
        return Err(format!("strip must be at most {MAX_STRIP}"));
    }
    let format = match raw.format.as_str() {
        "tar.gz" => Format::TarGz,
        "tar.bz2" => Format::TarBz2,
        other => return Err(format!("unknown format `{other}`")),
    };
    let required = raw
        .required
        .iter()
        .map(|r| RelPath::parse(r))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Artifact {
        url: raw.url.clone(),
        size: raw.size,
        sha256: parse_sha256(&raw.sha256)?,
        unpacked_size: raw.unpacked_size,
        format,
        strip: raw.strip,
        library_dir: RelPath::parse(&raw.library_dir)?,
        resources_dir: RelPath::parse(&raw.resources_dir)?,
        required,
    })
}

fn parse_sha256(hex: &str) -> Result<[u8; 32], String> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64
        || !bytes
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    {
        return Err("sha256 must be 64 lowercase hex digits".into());
    }
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks(2).enumerate() {
        let text = std::str::from_utf8(pair).map_err(|e| e.to_string())?;
        out[i] = u8::from_str_radix(text, 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// A host name: lowercase letters, digits, `-` and `.`, at most 253 bytes.
fn check_host(host: &str) -> Result<String, String> {
    let ok = !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|l| {
            !l.is_empty()
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
    if ok {
        Ok(host.to_owned())
    } else {
        Err(format!("`{host}` is not a lowercase host name"))
    }
}

/// The host of an `https://` URL with no user info and no port, lowercase.
pub(crate) fn https_host(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("`{url}` is not an https URL"))?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.contains('@') || authority.contains(':') {
        return Err(format!("`{url}`: user info and ports are not allowed"));
    }
    check_host(&authority.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::{CatalogError, Kind, https_host, parse_entry};

    const TARGET: &str = "x86_64-unknown-linux-gnu";

    fn entry(kind: &str, url: &str) -> String {
        format!(
            r#"
id = "org.rbs.browser-engine"
kind = "{kind}"
name = "Browser engine"
summary = "Lets browser sources show web pages."
version = "156.6.0"
hosts = ["cdn.example.org"]
notices = "https://cdn.example.org/notices"

[platform.{TARGET}]
url = "{url}"
size = 10
sha256 = "{}"
unpacked_size = 20
format = "tar.bz2"
strip = 1
library_dir = "Release"
resources_dir = "Resources"
required = ["libcef.so", "icudtl.dat"]
"#,
            "ab".repeat(32)
        )
    }

    #[test]
    fn parses_a_runtime_entry_for_this_target() {
        let text = entry("runtime", "https://cdn.example.org/cef.tar.bz2");
        let e = parse_entry(&text, TARGET).ok().flatten();
        let e = e.unwrap_or_else(|| panic!("entry should parse"));
        assert_eq!(e.kind, Kind::Runtime);
        assert_eq!(e.version.as_str(), "156.6.0");
        let a = e.artifact.unwrap_or_else(|| panic!("artifact for target"));
        assert_eq!(a.sha256, [0xab; 32]);
        assert_eq!(a.required.len(), 2);
        let other = parse_entry(&text, "aarch64-apple-darwin").ok().flatten();
        assert!(other.is_some_and(|e| e.artifact.is_none()));
    }

    #[test]
    fn unknown_kind_is_skipped_not_an_error() {
        let text = entry("plugin", "https://cdn.example.org/cef.tar.bz2");
        assert_eq!(parse_entry(&text, TARGET), Ok(None));
    }

    #[test]
    fn refuses_urls_outside_the_rules() {
        for url in [
            "http://cdn.example.org/cef.tar.bz2",
            "https://other.example.org/cef.tar.bz2",
            "https://user@cdn.example.org/cef.tar.bz2",
            "https://cdn.example.org:8443/cef.tar.bz2",
        ] {
            let text = entry("runtime", url);
            assert!(
                matches!(parse_entry(&text, TARGET), Err(CatalogError::Invalid(_))),
                "{url}"
            );
        }
    }

    #[test]
    fn refuses_unknown_fields_and_bad_values() {
        let base = entry("runtime", "https://cdn.example.org/cef.tar.bz2");
        for (from, to) in [
            ("version = \"156.6.0\"", "version = \"latest\""),
            ("strip = 1", "strip = 9"),
            ("format = \"tar.bz2\"", "format = \"zip\""),
            ("library_dir = \"Release\"", "library_dir = \"../Release\""),
            ("required = [", "required = [\"a/CON\", "),
            ("size = 10", "size = 0"),
            ("summary =", "extra = 1\nsummary ="),
        ] {
            let text = base.replace(from, to);
            assert!(parse_entry(&text, TARGET).is_err(), "{to}");
        }
        let short_hash = base.replace(&"ab".repeat(32), "ab");
        assert!(parse_entry(&short_hash, TARGET).is_err());
    }

    #[test]
    fn https_host_reads_only_the_host() {
        assert_eq!(
            https_host("https://Example.org/a?b#c").as_deref(),
            Ok("example.org")
        );
        assert!(https_host("https://").is_err());
        assert!(https_host("ftp://example.org").is_err());
    }
}
