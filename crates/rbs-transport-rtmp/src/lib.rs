//! RTMP transport adapter for the RBS transport port (`rbs-transport`).
//!
//! Publishes H.264 and AAC packets (`packets` input) to an RTMP server, as encoders
//! publish to YouTube, Twitch or nginx-rtmp: `rtmp://host[:port]/app` with the stream
//! key in `TransportConfig::secret`. Contract: `docs/specs/output-ports.md`, section
//! "RTMP transport".
//!
//! Behaviour:
//! - `send` checks the packet with `rbs_media_format::flv::FlvTracks::plan`, stamps it
//!   and stages it (a reference-count increment, no copy). It never blocks and never
//!   touches the socket. A short hand-off (8 items, about 100 ms) carries staged
//!   packets to the I/O thread; the session's queue limits cover staging plus
//!   hand-off, and when they are reached the GOP drop policy of the output-ports spec
//!   applies (whole old GOPs first, then cutting the current GOP until the next
//!   keyframe with `KeyframeRequested`, then audio).
//! - One I/O thread per session, started in `start`, owns a `std::net::TcpStream`:
//!   simple handshake, Set Chunk Size 4096, `connect` (AMF0, `objectEncoding: 0`),
//!   `releaseStream`, `FCPublish`, `createStream`, `publish`. Then, on every
//!   connection, `@setDataFrame onMetaData` and the AVC and AAC sequence headers are
//!   sent before the first packet, and the connection starts at a video keyframe.
//!   FLV bodies are built on the I/O thread from the packet (Annex B re-framed with
//!   4-byte lengths), so the producer copies nothing.
//! - Timestamps: 32-bit milliseconds from one origin (the first packet's decode time
//!   minus one second), wrapping as RTMP defines; type 0 headers on every message,
//!   with the extended timestamp field from 0xFFFFFF up.
//! - The server's Window Acknowledgement Size is honoured, pings are answered, and
//!   server messages larger than 64 KiB are a protocol error before they are decoded.
//! - Reconnect follows `ReconnectPolicy`: `Reconnecting { attempt }`, then
//!   `Connected`, headers resent, resuming at the next keyframe (`KeyframeRequested` is
//!   raised when the connection drops). Rejections by the server (bad stream key) are
//!   not retried.
//! - `finish(timeout)` sends what is queued, then `FCUnpublish` and `deleteStream`,
//!   and closes. It never blocks longer than `timeout`; `Drop` waits at most 200 ms.
//!   A thread stuck in DNS or `connect` past that is detached (output-ports
//!   invariant 4).
//! - The stream key is never in `Debug` output, events or errors; server text that
//!   becomes an error has it removed.
//!
//! Not yet: `rtmps` (TLS), enhanced RTMP (HEVC, AV1, Opus), AMF3, the digest
//! handshake. Pure Rust on `std`; no FFmpeg.

mod chunk;
mod command;
mod io;
mod session;
mod staging;
mod url;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::CodecId;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_transport::{
    CreateError, ReconnectPolicy, Transport, TransportConfig, TransportFactory, TransportInput,
    caps,
};

/// Stable adapter id.
pub const ADAPTER_ID: &str = "org.rbs.transport.rtmp";

/// Factory for RTMP transport sessions. Probing is free: no network access.
#[derive(Debug, Default, Clone, Copy)]
pub struct RtmpTransportFactory;

impl AdapterFactory for RtmpTransportFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "RTMP"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new()
            .with(caps::SCHEMES, CapabilityValue::Set(vec!["rtmp".to_owned()]))
            .with(
                caps::INPUT,
                CapabilityValue::Set(vec![caps::INPUT_PACKETS.to_owned()]),
            )
            .with(
                caps::CODECS,
                CapabilityValue::Set(vec![
                    CodecId::H264.as_str().to_owned(),
                    CodecId::Aac.as_str().to_owned(),
                ]),
            )
            .with(caps::RECONNECT, CapabilityValue::Flag(true)))
    }
}

impl TransportFactory for RtmpTransportFactory {
    fn create(&self, config: &TransportConfig) -> Result<Box<dyn Transport>, CreateError> {
        if config.scheme().as_deref() != Some("rtmp") {
            return Err(CreateError::Unsupported {
                what: "URL scheme (this adapter publishes to rtmp:// URLs; rtmps is not supported yet)"
                    .into(),
            });
        }
        if config.input != TransportInput::Packets {
            return Err(CreateError::Unsupported {
                what: "container input (RTMP takes encoded packets)".into(),
            });
        }
        if let Some((key, _)) = config.adapter_options.first() {
            return Err(CreateError::Unsupported {
                what: format!("adapter option {key:?} (the RTMP transport has none)"),
            });
        }
        let url = url::parse(&config.url).map_err(CreateError::InvalidConfig)?;
        let key = match &config.secret {
            Some(secret) if !secret.expose().is_empty() => secret.expose().to_owned(),
            _ => {
                return Err(CreateError::InvalidConfig(
                    "RTMP needs the stream key in the secret".into(),
                ));
            }
        };
        if config.queue.max_bytes == 0 || config.queue.max_duration.is_zero() {
            return Err(CreateError::InvalidConfig(
                "queue limits of 0 bytes or 0 seconds".into(),
            ));
        }
        if let ReconnectPolicy::Backoff { initial, .. } = config.reconnect {
            if initial.is_zero() {
                return Err(CreateError::InvalidConfig(
                    "reconnect back-off starting at 0 (it would retry in a tight loop)".into(),
                ));
            }
        }
        Ok(Box::new(session::RtmpTransport::new(session::Settings {
            url,
            key,
            limits: config.queue,
            reconnect: config.reconnect,
        })))
    }
}
