//! Local regular files only (ADR-0108 decision 9, ADR-0052).
//!
//! The check runs on the canonical path before any decoder open, so a URL, a UNC path,
//! a device path, a directory, a pipe, or a symlink, junction, or substituted drive that
//! does not resolve to a local regular file never reaches `MediaDecoder::pull`.

use std::fs;
use std::io;
use std::path::{Component, Path, Prefix};

/// Why `path` was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// The string or the canonical target is not a local regular file.
    Invalid(&'static str),
    /// The path is missing or unreadable. The host maps this to `SourceError::Media`.
    Media(String),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::Invalid(text) => f.write_str(text),
            PathError::Media(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for PathError {}

/// Canonicalizes `raw` and accepts it only when the target is a local regular file.
/// Does not read file contents.
pub fn accept_local_media_path(raw: &str) -> Result<std::path::PathBuf, PathError> {
    if let Some(why) = raw_refusal(raw) {
        return Err(PathError::Invalid(why));
    }
    let path = Path::new(raw.trim());
    // `symlink_metadata` does not follow and does not read contents. A symlink or
    // junction is still resolved by `canonicalize` below; the target must be a local
    // regular file or the open is refused.
    let meta = fs::symlink_metadata(path).map_err(io_error)?;
    if is_pipe_or_socket(&meta) {
        return Err(PathError::Invalid(
            "a local regular file, not a pipe or socket",
        ));
    }
    let canonical = fs::canonicalize(path).map_err(io_error)?;
    if !canonical_is_local(&canonical) {
        return Err(PathError::Invalid(
            "a local regular file (not a UNC, device, or substituted-drive target)",
        ));
    }
    let followed = fs::metadata(&canonical).map_err(io_error)?;
    if !followed.is_file() {
        return Err(PathError::Invalid(
            "a local regular file, not a directory or link to one",
        ));
    }
    Ok(canonical)
}

/// True when a canonical path is a local filesystem path. Verbatim `\\?\C:\...` is
/// local (what Windows `canonicalize` returns). `\\?\UNC\`, `\\.\`, and `\\server`
/// are not. A substituted drive that resolves to a UNC share fails here.
pub fn canonical_is_local(path: &Path) -> bool {
    let text = path.to_string_lossy();
    let lower = text.to_ascii_lowercase();
    if lower.starts_with(r"\\?\unc\") || lower.starts_with("//?/unc/") {
        return false;
    }
    if lower.starts_with(r"\\.\") || lower.starts_with("//./") {
        return false;
    }
    if (text.starts_with(r"\\") && !text.starts_with(r"\\?\")) || text.starts_with("//") {
        return false;
    }
    if lower == "/dev" || lower.starts_with("/dev/") || lower.starts_with("/dev\\") {
        return false;
    }
    for component in path.components() {
        if let Component::Prefix(prefix) = component {
            match prefix.kind() {
                Prefix::UNC(..) | Prefix::VerbatimUNC(..) | Prefix::DeviceNS(_) => return false,
                Prefix::VerbatimDisk(..) | Prefix::Disk(_) | Prefix::Verbatim(_) => {}
            }
        }
    }
    true
}

fn is_pipe_or_socket(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let kind = meta.file_type();
        kind.is_fifo() || kind.is_socket()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

fn io_error(error: io::Error) -> PathError {
    if error.kind() == io::ErrorKind::NotFound {
        PathError::Media(format!("missing file: {error}"))
    } else {
        PathError::Media(error.to_string())
    }
}

/// String-level refusal, before `canonicalize` and before any decoder open.
fn raw_refusal(raw: &str) -> Option<&'static str> {
    let text = raw.trim();
    if text.is_empty() {
        return Some("a local absolute path");
    }
    if text.as_bytes().contains(&0) {
        return Some("a local absolute path");
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("://") || scheme_that_is_not_a_drive(&lower) {
        return Some("a local absolute path, not a URL");
    }
    if text.starts_with(r"\\") || text.starts_with("//") {
        // `\\?\C:\...` is a local verbatim path (Windows canonicalize). `\\?\UNC`,
        // `\\.\`, and `\\server\share` are not.
        let rest = text.trim_start_matches(['\\', '/']);
        let mut parts = rest.split(['\\', '/']);
        let head = parts.next().unwrap_or("");
        let after = parts.next().unwrap_or("");
        let verbatim_disk = (head == "?" || head.eq_ignore_ascii_case("??")) && drive_prefix(after);
        if !verbatim_disk {
            return Some("a local absolute path, not a UNC or device path");
        }
    }
    if lower == "/dev" || lower.starts_with("/dev/") {
        return Some("a local absolute path, not a device path");
    }
    if !is_windows_absolute(text) && !is_unix_absolute(text) {
        return Some("a local absolute path");
    }
    for part in text.split(['\\', '/']) {
        if part.is_empty() || part == "." || part == ".." {
            continue;
        }
        if is_device_component(part) {
            return Some("a local absolute path, not a device path");
        }
    }
    None
}

fn scheme_that_is_not_a_drive(lower: &str) -> bool {
    let Some(colon) = lower.find(':') else {
        return false;
    };
    // `c:\...` is a drive. `file:...` and `http:...` are schemes.
    !(colon == 1 && lower.as_bytes()[0].is_ascii_alphabetic())
}

fn drive_prefix(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn is_windows_absolute(text: &str) -> bool {
    let text = text
        .strip_prefix(r"\\?\")
        .or_else(|| text.strip_prefix(r"\\??\\"))
        .unwrap_or(text);
    let bytes = text.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn is_unix_absolute(text: &str) -> bool {
    text.starts_with('/') && !text.starts_with("//")
}

fn is_device_component(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part);
    matches!(
        stem.to_ascii_lowercase().as_str(),
        "con"
            | "prn"
            | "aux"
            | "nul"
            | "com1"
            | "com2"
            | "com3"
            | "com4"
            | "com5"
            | "com6"
            | "com7"
            | "com8"
            | "com9"
            | "lpt1"
            | "lpt2"
            | "lpt3"
            | "lpt4"
            | "lpt5"
            | "lpt6"
            | "lpt7"
            | "lpt8"
            | "lpt9"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use super::*;
    use std::fs;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch() -> std::path::PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("rbs-media-path-{n}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn urls_unc_and_device_paths_are_refused_before_a_content_read() {
        for raw in [
            "https://example.com/a.mp4",
            "http://example.com/a.mp4",
            "file:///C:/a.mp4",
            "file:C:/a.mp4",
            r"\\server\share\a.mp4",
            "//server/share/a.mp4",
            r"\\?\UNC\server\share\a.mp4",
            r"\\.\pipe\rbs",
            r"C:\clips\NUL",
            r"C:\clips\COM1.mp4",
            "/dev/null",
            "clips/a.mp4",
            "",
        ] {
            let err = accept_local_media_path(raw).unwrap_err();
            assert!(matches!(err, PathError::Invalid(_)), "{raw} -> {err:?}");
        }
        assert!(!canonical_is_local(Path::new(r"\\server\share\a.mp4")));
        assert!(!canonical_is_local(Path::new(
            r"\\?\UNC\server\share\a.mp4"
        )));
        // A substituted drive that resolves to a UNC share is this shape.
        assert!(canonical_is_local(Path::new(r"\\?\C:\clips\a.mp4")));
    }

    #[test]
    fn a_regular_file_is_accepted_without_reading_its_bytes() {
        let dir = scratch();
        let file = dir.join("clip.mp4");
        {
            let mut out = fs::File::create(&file).unwrap();
            out.write_all(b"not-decoded").unwrap();
        }
        let got = accept_local_media_path(&file.to_string_lossy()).unwrap();
        assert!(got.is_file());
        assert_eq!(fs::read(&file).unwrap(), b"not-decoded");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_and_a_link_to_one_are_refused() {
        let dir = scratch();
        let nested = dir.join("nested");
        fs::create_dir_all(&nested).unwrap();
        assert!(matches!(
            accept_local_media_path(&nested.to_string_lossy()),
            Err(PathError::Invalid(_))
        ));
        let link = dir.join("junction");
        if link_directory(&link, &nested) {
            let err = accept_local_media_path(&link.to_string_lossy()).unwrap_err();
            assert!(
                matches!(err, PathError::Invalid(_)),
                "symlink or junction to a directory: {err:?}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_media_not_a_setting_typo() {
        let dir = scratch();
        let missing = dir.join("no-such-clip.mp4");
        let err = accept_local_media_path(&missing.to_string_lossy()).unwrap_err();
        assert!(matches!(err, PathError::Media(_)), "{err:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    fn link_directory(link: &Path, target: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            if std::os::windows::fs::symlink_dir(target, link).is_ok() {
                return true;
            }
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .status()
                .is_ok_and(|status| status.success())
        }
    }
}
