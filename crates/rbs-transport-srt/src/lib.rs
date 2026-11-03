//! SRT transport adapter for the RBS transport port (`rbs-transport`).
//!
//! Sends the chunks of an MPEG-TS muxer (`container.mpegts` input) to an SRT
//! listener in caller mode and live mode, as encoders send to SRT ingest servers,
//! relays and decoders: `srt://host:port` with the stream id in
//! `TransportConfig::secret`. Contract: `docs/specs/output-ports.md`, section "SRT
//! transport".
//!
//! Behaviour:
//! - `send` stages the chunk (a reference-count increment, no copy) and never
//!   touches the socket. The transport never parses the container: a `Media` chunk
//!   marked `keyframe` is the GOP boundary, `Header` chunks are never dropped, and
//!   `Patch` chunks are ignored (a live stream cannot be rewritten). A short hand-off
//!   (8 chunks) carries staged chunks to the I/O thread; the queue limits cover
//!   staging plus hand-off, and when they are reached the GOP drop policy of the
//!   output-ports spec applies at chunk granularity (whole old GOPs first, then
//!   cutting the current GOP until the next keyframe chunk with `KeyframeRequested`).
//! - One I/O thread per session, started in `start`, owns a connected
//!   `std::net::UdpSocket`: the handshake version 5 caller exchange (induction, then
//!   conclusion with the HSREQ extension and the stream id), then data packets of at
//!   most 1316 bytes (seven TS packets), one message per packet, never spanning two
//!   chunks. Payloads are slices of the queued chunk, so nothing is copied before the
//!   datagram is written.
//! - Reliability as a live-mode sender: sent packets are kept until acknowledged and
//!   resent (with the retransmission flag) when the receiver reports them lost;
//!   packets older than the peer's latency (at least one second) are forgotten, and
//!   a loss report for them is answered with a drop request. ACKs are answered with
//!   ACKACKs (the receiver's RTT measurement, reported in `TransportStats::rtt`), the
//!   peer's flow window and free buffer bound the packets in flight, keep-alives are
//!   sent after one idle second, and five silent seconds lose the connection.
//! - Reconnect follows `ReconnectPolicy`: `Reconnecting { attempt }`, then
//!   `Connected`, the `Header` chunks resent first, resuming at the next keyframe
//!   chunk (`KeyframeRequested` is raised when the connection drops). Handshake
//!   rejections by the listener (bad stream id, required passphrase) are not retried.
//! - `finish(timeout)` sends what is queued, waits until the listener has
//!   acknowledged it, and sends a shutdown. It never blocks longer than `timeout`;
//!   `Drop` waits at most 200 ms. A thread stuck in DNS resolution past that is
//!   detached (output-ports invariant 4).
//! - The stream id is never in `Debug` output, events or errors.
//!
//! Not yet: encryption (passphrase, AES key material exchange), listener and
//! rendezvous modes, packet filters (FEC), bandwidth estimation, other containers.
//! Pure Rust on `std`; no libsrt, no FFmpeg.

mod io;
mod packet;
mod session;
mod staging;
mod url;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::ContainerId;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_transport::{
    CreateError, ReconnectPolicy, Transport, TransportConfig, TransportFactory, TransportInput,
    caps,
};

/// Stable adapter id.
pub const ADAPTER_ID: &str = "org.rbs.transport.srt";

/// The adapter option that sets the latency (TSBPD delay) in milliseconds that the
/// sender asks the listener for. The listener uses the larger of its own and this.
pub const OPTION_LATENCY_MS: &str = "latency_ms";

/// Default latency in milliseconds, as libsrt.
pub const DEFAULT_LATENCY_MS: u16 = 120;

/// Factory for SRT transport sessions. Probing is free: no network access.
#[derive(Debug, Default, Clone, Copy)]
pub struct SrtTransportFactory;

impl AdapterFactory for SrtTransportFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "SRT"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new()
            .with(caps::SCHEMES, CapabilityValue::Set(vec!["srt".to_owned()]))
            .with(
                caps::INPUT,
                CapabilityValue::Set(vec![ContainerId::MpegTs.transport_input().to_owned()]),
            )
            .with(caps::RECONNECT, CapabilityValue::Flag(true)))
    }
}

impl TransportFactory for SrtTransportFactory {
    fn create(&self, config: &TransportConfig) -> Result<Box<dyn Transport>, CreateError> {
        if config.scheme().as_deref() != Some("srt") {
            return Err(CreateError::Unsupported {
                what: "URL scheme (this adapter sends to srt:// URLs)".into(),
            });
        }
        if config.input != TransportInput::Container(ContainerId::MpegTs) {
            return Err(CreateError::Unsupported {
                what: "input (the SRT transport takes MPEG-TS chunks)".into(),
            });
        }
        let url = url::parse(&config.url).map_err(CreateError::InvalidConfig)?;
        let mut latency_ms = DEFAULT_LATENCY_MS;
        for (key, value) in &config.adapter_options {
            match key.as_str() {
                OPTION_LATENCY_MS => {
                    latency_ms = value.parse().map_err(|_| {
                        CreateError::InvalidConfig(format!(
                            "{OPTION_LATENCY_MS} must be 0 to 65535 milliseconds"
                        ))
                    })?;
                }
                _ => {
                    return Err(CreateError::Unsupported {
                        what: format!(
                            "adapter option {key:?} (the SRT transport has {OPTION_LATENCY_MS:?})"
                        ),
                    });
                }
            }
        }
        let stream_id = match &config.secret {
            Some(secret) if !secret.expose().is_empty() => {
                if secret.expose().len() > packet::MAX_STREAM_ID {
                    return Err(CreateError::InvalidConfig(format!(
                        "the stream id is longer than {} bytes",
                        packet::MAX_STREAM_ID
                    )));
                }
                Some(secret.expose().as_bytes().to_vec())
            }
            _ => None,
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
        Ok(Box::new(session::SrtTransport::new(session::Settings {
            url,
            stream_id,
            latency_ms,
            limits: config.queue,
            reconnect: config.reconnect,
        })))
    }
}
