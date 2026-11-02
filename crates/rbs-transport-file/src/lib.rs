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
//! - `finish` writes what is queued, flushes, syncs the data to disk and joins the
//!   thread within the timeout. If the timeout passes, the rest is discarded, the
//!   thread is stopped after its current write, and `Timeout` is returned.
//! - Write errors are reported once as `TransportEvent::Disconnected` and returned by
//!   later `send` calls; the file is not retried.
//!
//! URLs: `file:///absolute/path`, `file:///C:/path` or `file://C:\path` on Windows.
//! Percent-encoding is not decoded. An existing file is truncated.

mod session;

use std::path::PathBuf;

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
        let path = file_path(&config.url)
            .ok_or_else(|| CreateError::InvalidConfig("file URL without a path".into()))?;
        Ok(Box::new(session::FileTransport::new(path, config.queue)))
    }
}

/// The path of a `file://` URL.
fn file_path(url: &str) -> Option<PathBuf> {
    let rest = url.get(url.find("://")? + 3..)?;
    // `file:///C:/x`: drop the slash before a drive letter.
    let bytes = rest.as_bytes();
    let rest = if bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[2] == b':'
        && bytes[1].is_ascii_alphabetic()
    {
        &rest[1..]
    } else {
        rest
    };
    (!rest.is_empty()).then(|| PathBuf::from(rest))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn parses_file_urls() {
        assert_eq!(
            file_path("file:///tmp/a.ts"),
            Some(PathBuf::from("/tmp/a.ts"))
        );
        assert_eq!(
            file_path("file:///C:/rec/a.mp4"),
            Some(PathBuf::from("C:/rec/a.mp4"))
        );
        assert_eq!(
            file_path("file://C:\\rec\\a.mp4"),
            Some(PathBuf::from("C:\\rec\\a.mp4"))
        );
        assert_eq!(file_path("file://"), None);
        assert_eq!(file_path("/tmp/a"), None);
    }
}
