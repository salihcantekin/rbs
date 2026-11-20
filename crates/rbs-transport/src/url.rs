//! `file://` URLs of local paths (RFC 8089), shared by the file transport and the
//! callers that build its output URLs (ADR-0159 decision 4).

use std::path::Path;

/// Escapes what a URL path must not carry raw: `%`, space, `#`, `?` and `\` (a name
/// character on Unix). The file transport decodes them.
fn escape(part: &str, url: &mut String) {
    for c in part.chars() {
        match c {
            '%' => url.push_str("%25"),
            ' ' => url.push_str("%20"),
            '#' => url.push_str("%23"),
            '?' => url.push_str("%3F"),
            '\\' => url.push_str("%5C"),
            c => url.push(c),
        }
    }
}

/// The `file://` URL of an absolute path (RFC 8089), built from the path's components:
/// `C:\x\a b.mkv` and `\\?\C:\x\a b.mkv` become `file:///C:/x/a%20b.mkv`,
/// `\\srv\share\a.mkv` becomes `file://srv/share/a.mkv`, `/tmp/a.mkv` becomes
/// `file:///tmp/a.mkv`.
pub fn file_url(path: &Path) -> String {
    use std::path::{Component, Prefix};
    let mut host = String::new();
    let mut parts: Vec<String> = Vec::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => match p.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                    parts.push(format!("{}:", char::from(d)));
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    host = server.to_string_lossy().into_owned();
                    parts.push(share.to_string_lossy().into_owned());
                }
                _ => parts.push(p.as_os_str().to_string_lossy().into_owned()),
            },
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => parts.push("..".to_owned()),
            Component::Normal(n) => parts.push(n.to_string_lossy().into_owned()),
        }
    }
    let mut url = format!("file://{host}");
    for part in &parts {
        url.push('/');
        escape(part, &mut url);
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_paths_become_file_urls() {
        for (path, url) in [
            (
                r"C:\Users\me\My Videos\a%1.mkv",
                "file:///C:/Users/me/My%20Videos/a%251.mkv",
            ),
            (r"\\?\C:\x\a.mkv", "file:///C:/x/a.mkv"),
            (r"\\srv\share\rec\a.mkv", "file://srv/share/rec/a.mkv"),
        ] {
            assert_eq!(file_url(Path::new(path)), url, "{path}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_paths_become_file_urls() {
        assert_eq!(file_url(Path::new("/tmp/a.mkv")), "file:///tmp/a.mkv");
        assert_eq!(
            file_url(Path::new("/tmp/a b\\c#?.mkv")),
            "file:///tmp/a%20b%5Cc%23%3F.mkv"
        );
    }
}
