//! Location values: paths and URLs in requests (ADR-0052 decision 4, protocol 1.79).
//!
//! Without the irreversible-actions permission a token may only name a local absolute
//! file system path, so an agent cannot make RBS contact a host of its choosing through
//! a "file" (an SMB path would also send the user's credentials). The check reads the
//! value only, the same on every platform, and fails closed: anything it cannot place
//! as a local absolute path is refused.

/// True when `value` is a local absolute file system path: `/x` on Unix, `C:\x` or
/// `C:/x` on Windows. URL schemes (`file:` included), UNC paths (`\\host\x`, `//host/x`),
/// Windows device and verbatim paths (`\\?\`, `\\.\`), relative paths and values with
/// control characters are not.
pub fn is_local_path(value: &str) -> bool {
    if value.chars().any(char::is_control) {
        return false;
    }
    let bytes = value.as_bytes();
    match bytes {
        // Unix absolute, but not `//host/share`.
        [b'/', rest @ ..] => !rest.starts_with(b"/"),
        // Windows drive absolute: one letter, a colon, a separator.
        [drive, b':', sep, ..] if drive.is_ascii_alphabetic() && matches!(sep, b'\\' | b'/') => {
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::is_local_path;

    #[test]
    fn local_absolute_paths_pass() {
        for p in ["/home/a/b.png", "/", "C:\\Videos\\x.mkv", "d:/media/a.mp4"] {
            assert!(is_local_path(p), "{p}");
        }
    }

    #[test]
    fn hosts_devices_and_relative_paths_are_refused() {
        for p in [
            "",
            "a.png",
            "./a.png",
            "C:a.png",
            "\\\\server\\share\\a.png",
            "//server/share/a.png",
            "\\\\?\\UNC\\server\\share\\a.png",
            "\\\\?\\C:\\a.png",
            "\\\\.\\PhysicalDrive0",
            "\\Videos\\a.png",
            "file:///home/a.png",
            "http://example.com/a.png",
            "rtsp://cam/stream",
            "smb://server/a.png",
            "/home/a\n.png",
        ] {
            assert!(!is_local_path(p), "{p:?}");
        }
    }
}
