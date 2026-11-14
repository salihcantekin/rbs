//! File URLs for the media a project refers to (ADR-0089 decision 8).

use std::path::Path;

use crate::ProjectError;

/// The absolute folder of a recording's files, as a `file://` URL ending in `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFolder {
    url: String,
}

impl MediaFolder {
    /// The folder at `path`, which must be absolute.
    pub fn from_path(path: &Path) -> Result<Self, ProjectError> {
        if !path.is_absolute() {
            return Err(ProjectError::NotAbsolute);
        }
        Self::from_text(&path.to_string_lossy())
    }

    /// The folder from a path's text in either platform's form, independent of the
    /// platform this runs on: `C:\Users\x` or `C:/Users/x` (Windows),
    /// `\\server\share\x` (UNC), or `/Users/x` (macOS). Anything else is
    /// [`ProjectError::NotAbsolute`].
    pub fn from_text(path: &str) -> Result<Self, ProjectError> {
        let unified = path.replace('\\', "/");
        let bytes = unified.as_bytes();
        let mut url = String::from("file://");
        let rest: &str;
        if let Some(unc) = unified.strip_prefix("//") {
            // UNC: the host is written as it is, the share and folders are encoded.
            let (host, tail) = unc.split_once('/').ok_or(ProjectError::NotAbsolute)?;
            if host.is_empty() || tail.is_empty() {
                return Err(ProjectError::NotAbsolute);
            }
            url.push_str(host);
            url.push('/');
            rest = tail;
        } else if bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'/'
        {
            // A drive letter: its colon stays literal.
            url.push('/');
            url.push_str(&unified[..2]);
            url.push('/');
            rest = &unified[3..];
        } else if let Some(tail) = unified.strip_prefix('/') {
            url.push('/');
            rest = tail;
        } else {
            return Err(ProjectError::NotAbsolute);
        }
        for segment in rest.split('/').filter(|s| !s.is_empty()) {
            push_encoded(&mut url, segment);
            url.push('/');
        }
        Ok(Self { url })
    }

    /// The folder's URL, ending in `/`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The URL of `file_name` inside the folder.
    pub fn file_url(&self, file_name: &str) -> String {
        let mut url = self.url.clone();
        push_encoded(&mut url, file_name);
        url
    }
}

/// Percent-encodes every UTF-8 byte outside the RFC 3986 unreserved set.
fn push_encoded(out: &mut String, segment: &str) {
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn windows_mac_and_unc_paths_become_file_urls() {
        let win = MediaFolder::from_text(r"C:\Users\Salih Ş\Videos\Take #1").unwrap();
        assert_eq!(
            win.url(),
            "file:///C:/Users/Salih%20%C5%9E/Videos/Take%20%231/"
        );
        assert_eq!(
            win.file_url("Mic 2.wav"),
            "file:///C:/Users/Salih%20%C5%9E/Videos/Take%20%231/Mic%202.wav"
        );
        let mac = MediaFolder::from_text("/Users/x/Movies/a?b%").unwrap();
        assert_eq!(mac.url(), "file:///Users/x/Movies/a%3Fb%25/");
        let unc = MediaFolder::from_text(r"\\nas\media\rec").unwrap();
        assert_eq!(unc.url(), "file://nas/media/rec/");
        let root = MediaFolder::from_text("D:/").unwrap();
        assert_eq!(root.url(), "file:///D:/");
    }

    #[test]
    fn relative_paths_are_refused() {
        for p in ["rec", "./rec", "C:rec", r"\\", r"\\host", ""] {
            assert_eq!(
                MediaFolder::from_text(p),
                Err(ProjectError::NotAbsolute),
                "{p}"
            );
        }
    }
}
