//! The downloaded browser engine as the runner sees it (ADR-0145 decisions 7 and 8,
//! add-ons spec "Browser engine consumer").
//!
//! The studio names the installed runtime directory in [`RUNTIME_ENV`]. Every CEF
//! process of the runner reads it, then the marker `<runtime>/.complete` that the
//! add-on store wrote last, to find the library and resource directories. The marker
//! is TOML with four string keys; this crate reads that small subset itself so it
//! adds no dependency.

use std::path::{Component, Path, PathBuf};

/// Environment variable that names the installed runtime directory.
pub const RUNTIME_ENV: &str = "RBS_CEF_RUNTIME";

/// Name of the marker file inside the runtime directory.
pub const MARKER: &str = ".complete";

/// Largest marker read, so a damaged file cannot allocate without bound.
const MAX_MARKER: u64 = 4096;

/// What the marker says, with its directories resolved under the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runtime {
    /// The runtime directory itself.
    pub root: PathBuf,
    /// CEF version the add-on was installed as (`156.0.30`).
    pub version: String,
    /// Directory that holds the library (`libcef.so`, `libcef.dll` or the framework).
    pub library_dir: PathBuf,
    /// Directory that holds `icudtl.dat`, the `.pak` files and `locales`.
    pub resources_dir: PathBuf,
}

impl Runtime {
    /// The `locales` directory under the resources.
    pub fn locales_dir(&self) -> PathBuf {
        self.resources_dir.join("locales")
    }

    /// True when `major.minor.patch` is the version in the marker.
    pub fn is_version(&self, major: u32, minor: u32, patch: u32) -> bool {
        self.version == format!("{major}.{minor}.{patch}")
    }
}

/// Why a runtime could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// [`RUNTIME_ENV`] is not set, or empty.
    NotSet,
    /// The marker is missing or unreadable: the install did not finish.
    NoMarker,
    /// The marker does not have the four keys, or a directory leaves the runtime.
    BadMarker,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotSet => "RBS_CEF_RUNTIME is not set",
            Self::NoMarker => "the browser engine's marker is missing",
            Self::BadMarker => "the browser engine's marker is damaged",
        })
    }
}

impl std::error::Error for RuntimeError {}

/// Reads the runtime named by [`RUNTIME_ENV`].
pub fn from_env() -> Result<Runtime, RuntimeError> {
    let root = std::env::var_os(RUNTIME_ENV)
        .filter(|v| !v.is_empty())
        .ok_or(RuntimeError::NotSet)?;
    read(Path::new(&root))
}

/// Reads `<root>/.complete` and resolves its directories under `root`.
pub fn read(root: &Path) -> Result<Runtime, RuntimeError> {
    use std::io::Read as _;
    let file = std::fs::File::open(root.join(MARKER)).map_err(|_| RuntimeError::NoMarker)?;
    let mut text = String::new();
    file.take(MAX_MARKER)
        .read_to_string(&mut text)
        .map_err(|_| RuntimeError::NoMarker)?;
    parse(root, &text)
}

/// Parses marker text. Keys other than the four are ignored; each of the four must
/// appear once as a basic string.
pub fn parse(root: &Path, text: &str) -> Result<Runtime, RuntimeError> {
    let mut version = None;
    let mut library_dir = None;
    let mut resources_dir = None;
    let mut sha256 = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or(RuntimeError::BadMarker)?;
        let value = basic_string(value.trim()).ok_or(RuntimeError::BadMarker)?;
        let slot = match key.trim() {
            "version" => &mut version,
            "library_dir" => &mut library_dir,
            "resources_dir" => &mut resources_dir,
            "sha256" => &mut sha256,
            _ => continue,
        };
        if slot.replace(value).is_some() {
            return Err(RuntimeError::BadMarker);
        }
    }
    let (Some(version), Some(lib), Some(res), Some(_)) =
        (version, library_dir, resources_dir, sha256)
    else {
        return Err(RuntimeError::BadMarker);
    };
    Ok(Runtime {
        root: root.to_path_buf(),
        library_dir: beneath(root, &lib)?,
        resources_dir: beneath(root, &res)?,
        version,
    })
}

/// A TOML basic string without escapes other than `\"` and `\\`.
fn basic_string(value: &str) -> Option<String> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                _ => return None,
            },
            '"' => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

/// `rel` joined under `root`; only plain names separated by `/` are allowed.
fn beneath(root: &Path, rel: &str) -> Result<PathBuf, RuntimeError> {
    let mut path = root.to_path_buf();
    for part in rel.split('/') {
        let mut parts = Path::new(part).components();
        match (parts.next(), parts.next()) {
            (Some(Component::Normal(name)), None) if !part.contains('\\') => path.push(name),
            _ => return Err(RuntimeError::BadMarker),
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
    use super::*;

    const MARKER_TEXT: &str = "version = \"156.0.30\"\n\
        sha256 = \"356d\"\n\
        library_dir = \"Release\"\n\
        resources_dir = \"Release/Chromium Embedded Framework.framework/Resources\"\n";

    #[test]
    fn the_marker_names_the_library_and_resources_under_the_runtime() {
        let root = Path::new("/a/org.rbs.browser-engine/156.0.30");
        let rt = parse(root, MARKER_TEXT).unwrap();
        assert_eq!(rt.version, "156.0.30");
        assert_eq!(rt.library_dir, root.join("Release"));
        assert_eq!(
            rt.resources_dir,
            root.join("Release")
                .join("Chromium Embedded Framework.framework")
                .join("Resources")
        );
        assert_eq!(rt.locales_dir(), rt.resources_dir.join("locales"));
        assert!(rt.is_version(156, 0, 30));
        assert!(!rt.is_version(156, 0, 31));
    }

    #[test]
    fn a_marker_without_every_key_or_with_a_path_out_is_refused() {
        let root = Path::new("/r");
        let missing = "version = \"1.2.3\"\nlibrary_dir = \"a\"\n";
        assert_eq!(parse(root, missing), Err(RuntimeError::BadMarker));
        for bad in ["..", "a/../b", "/abs", "a\\b", "", "a//b"] {
            let text = MARKER_TEXT.replace("\"Release\"", &format!("\"{bad}\""));
            assert_eq!(parse(root, &text), Err(RuntimeError::BadMarker), "{bad}");
        }
        let twice = format!("{MARKER_TEXT}version = \"1.0.0\"\n");
        assert_eq!(parse(root, &twice), Err(RuntimeError::BadMarker));
        let bare = MARKER_TEXT.replace("\"156.0.30\"", "156");
        assert_eq!(parse(root, &bare), Err(RuntimeError::BadMarker));
    }

    #[test]
    fn unknown_keys_comments_and_escapes_are_read() {
        let text = format!("# written last\nextra = \"x\"\n{MARKER_TEXT}");
        assert!(parse(Path::new("/r"), &text).is_ok());
        assert_eq!(basic_string(r#""a\"b\\c""#).as_deref(), Some("a\"b\\c"));
        assert_eq!(basic_string(r#""a\nb""#), None);
    }

    #[test]
    fn a_missing_marker_is_no_marker() {
        let dir = std::env::temp_dir().join(format!("rbs-ipc-rt-{}", std::process::id()));
        assert_eq!(read(&dir), Err(RuntimeError::NoMarker));
    }
}
