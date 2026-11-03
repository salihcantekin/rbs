//! File transport adapter for the RBS transport port (`rbs-transport`).
//!
//! Writes the chunks of any muxer (`container.*` input) to a local file. All file I/O,
//! including opening, runs on a thread the session starts in `start` and stops in
//! `finish` or `Drop`; `send` only moves the chunk (a reference-count increment) into
//! a wait-free single-producer single-consumer ring and never blocks.
//!
//! Behaviour:
//! - Files never drop data. When the queue limits are reached (bytes, or media
//!   duration between the newest queued chunk and the last written one), or the ring
//!   is full, `send` returns the recoverable `TransportError::QueueFull` and nothing
//!   is queued; the output worker retries (output-ports spec, "QueueFull").
//! - `Patch` chunks are applied (`transport.seekable`): the writer seeks, overwrites
//!   and returns to the end.
//! - `finish` writes what is queued, flushes, syncs the data to disk (regular files)
//!   and joins the thread within the timeout. If the timeout passes, the rest is discarded, the
//!   thread is stopped after its current write, and `Timeout` is returned.
//! - Write errors are reported once as `TransportEvent::Disconnected`, returned once by
//!   `send` (later calls return `Closed`); the file is not retried.
//! - `finish(timeout)` never blocks longer than `timeout`; `Drop` waits at most 200 ms.
//!   A thread stuck in an OS call past that is detached (output-ports invariant 4).
//!
//! URLs: `file:///absolute/path` or `file://localhost/absolute/path`; on Windows also
//! `file:///C:/path`, `file://C:\path` and `file://host/share/path` (UNC). Percent-escapes
//! are decoded; relative paths and (outside Windows) remote hosts are refused. An
//! existing file is truncated. A secret and a reconnect policy have no meaning for a
//! file and are ignored; adapter options are refused (there are none).

mod session;

use std::path::{Path, PathBuf};

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::ContainerId;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_transport::{
    CreateError, Transport, TransportConfig, TransportFactory, TransportInput, caps,
};

/// Stable adapter id.
pub const ADAPTER_ID: &str = "org.rbs.transport.file";

/// Factory for file transport sessions. Probing is free: no files are touched.
#[derive(Debug, Default, Clone, Copy)]
pub struct FileTransportFactory;

impl AdapterFactory for FileTransportFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "File"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        let inputs = ContainerId::ALL
            .iter()
            .map(|c| c.transport_input().to_owned())
            .collect();
        Ok(CapabilitySet::new()
            .with(caps::SCHEMES, CapabilityValue::Set(vec!["file".to_owned()]))
            .with(caps::INPUT, CapabilityValue::Set(inputs))
            .with(caps::SEEKABLE, CapabilityValue::Flag(true)))
    }
}

impl TransportFactory for FileTransportFactory {
    fn create(&self, config: &TransportConfig) -> Result<Box<dyn Transport>, CreateError> {
        if config.scheme().as_deref() != Some("file") {
            return Err(CreateError::Unsupported {
                what: "URL scheme (this adapter writes file:// URLs)".into(),
            });
        }
        if config.input == TransportInput::Packets {
            return Err(CreateError::Unsupported {
                what: "packet input (files take muxer chunks)".into(),
            });
        }
        if let Some((key, _)) = config.adapter_options.first() {
            return Err(CreateError::Unsupported {
                what: format!("adapter option {key:?} (the file transport has none)"),
            });
        }
        let path = file_path(&config.url).map_err(CreateError::InvalidConfig)?;
        Ok(Box::new(session::FileTransport::new(path, config.queue)))
    }
}

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

/// The path of a `file://` URL (RFC 8089): `file:///abs/path` or
/// `file://localhost/abs/path`; on Windows also `file:///C:/path`, `file://C:\path` and
/// `file://host/share/path` (a UNC path). Percent-escapes are decoded. The path must be
/// absolute.
fn file_path(url: &str) -> Result<PathBuf, String> {
    let rest = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .ok_or("not a URL")?;
    let rest = percent_decode(rest)?;
    let (host, path) = match rest.find('/') {
        Some(0) => ("", rest.as_str()),
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest.as_str(), ""),
    };
    let path = match host {
        "" | "localhost" => strip_drive_slash(path).to_owned(),
        _ if is_drive(host) => rest.clone(),
        _ if cfg!(windows) => format!(r"\\{host}{}", path.replace('/', r"\")),
        _ => return Err(format!("remote host {host:?} in a file URL")),
    };
    if path.is_empty() {
        return Err("file URL without a path".into());
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err("file URL with a relative path".into());
    }
    Ok(path)
}

/// Whether `s` starts with a Windows drive (`C:` then a separator or nothing). Only
/// on Windows; elsewhere `C:` is an ordinary (relative) name.
fn is_drive(s: &str) -> bool {
    let b = s.as_bytes();
    cfg!(windows)
        && b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && b.get(2).is_none_or(|c| matches!(c, b'/' | b'\\'))
}

/// `/C:/x` becomes `C:/x` on Windows.
fn strip_drive_slash(path: &str) -> &str {
    match path.strip_prefix('/') {
        Some(rest) if is_drive(rest) => rest,
        _ => path,
    }
}

fn percent_decode(s: &str) -> Result<String, String> {
    if !s.contains('%') {
        return Ok(s.to_owned());
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or("bad percent-escape in the file URL")?;
            out.push(hex);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "file URL path is not UTF-8".into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
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

    /// `file_url` is the inverse of the URL parsing: the path comes back.
    #[test]
    fn file_urls_round_trip() {
        let dir = std::env::temp_dir().join("rbs url a b%#?");
        let path = dir.join("rec 1.mkv");
        assert_eq!(file_path(&file_url(&path)).unwrap(), path);
    }

    #[test]
    fn parses_file_urls() {
        let ok = |url: &str| file_path(url).unwrap();
        #[cfg(unix)]
        {
            assert_eq!(ok("file:///tmp/a.ts"), PathBuf::from("/tmp/a.ts"));
            assert_eq!(ok("file://localhost/tmp/a.ts"), PathBuf::from("/tmp/a.ts"));
            assert_eq!(
                ok("file:///tmp/my%20rec.ts"),
                PathBuf::from("/tmp/my rec.ts")
            );
            assert!(file_path("file://host/share/a.ts").is_err(), "remote host");
            assert!(file_path("file:///C:/a.ts").is_ok(), "an absolute /C:/a.ts");
            assert!(file_path("file://C:/a.ts").is_err(), "a host named C:");
        }
        #[cfg(windows)]
        {
            assert_eq!(ok("file:///C:/rec/a.mp4"), PathBuf::from("C:/rec/a.mp4"));
            assert_eq!(ok("file://C:\\rec\\a.mp4"), PathBuf::from("C:\\rec\\a.mp4"));
            assert_eq!(ok("file://localhost/C:/a.mp4"), PathBuf::from("C:/a.mp4"));
            assert_eq!(
                ok("file://server/share/a.mp4"),
                PathBuf::from(r"\\server\share\a.mp4")
            );
        }
        assert!(file_path("file://").is_err());
        assert!(file_path("file:///").is_ok() || cfg!(windows));
        assert!(file_path("file://rel/x").is_err() || cfg!(windows));
        assert!(file_path("file:///a%2").is_err(), "bad escape");
        assert!(file_path("file:///a%ff").is_err(), "not UTF-8");
        assert!(file_path("/tmp/a").is_err());
    }
}
