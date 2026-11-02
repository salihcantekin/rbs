//! Transport factory and session traits.

use std::time::Duration;

use rbs_core::media::{EncodedPacket, MuxChunk, StreamDescriptor, StreamIndex};
use rbs_core::registry::AdapterFactory;

use crate::config::TransportConfig;
use crate::error::{CreateError, TransportError};

/// One item handed to [`Transport::send`], lent for the call. A transport that queues
/// it clones the packet or chunk (an O(1) reference-count increment).
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub enum TransportItem<'a> {
    /// An encoded packet, for `TransportInput::Packets`.
    Packet {
        /// Its stream.
        stream: StreamIndex,
        /// The packet.
        packet: &'a EncodedPacket,
    },
    /// A muxer chunk, for `TransportInput::Container`.
    Chunk(&'a MuxChunk),
}

/// Why data was dropped.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The send queue reached its byte or duration limit.
    QueueFull,
    /// Not connected and the policy discards data while reconnecting.
    Disconnected,
}

/// Result of a successful [`Transport::send`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The item was queued and nothing was dropped.
    Queued,
    /// The item was queued after older queued items were dropped.
    QueuedAfterDrops {
        /// Older items dropped by this call.
        dropped: u32,
        /// Why.
        reason: DropReason,
    },
    /// The item itself was not queued (for example a non-keyframe while the
    /// transport waits for the next keyframe after a drop).
    Dropped {
        /// Why.
        reason: DropReason,
    },
}

/// Something that happened in the background, reported by
/// [`Transport::poll_events`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportEvent {
    /// Connected (or the file is open) and delivering.
    Connected,
    /// The connection dropped; reconnect attempt `attempt` is scheduled.
    Reconnecting {
        /// 1 for the first attempt.
        attempt: u32,
    },
    /// The connection is gone for good.
    Disconnected {
        /// Why.
        error: TransportError,
    },
    /// The peer asked for a keyframe (for example a WebRTC PLI/FIR, or a new viewer
    /// that needs a decoding entry point). The engine forwards it to the encoder as
    /// `EncoderChange::RequestKeyframe`, rate-limited when the encoder has several
    /// consumers.
    KeyframeRequested,
    /// The transport's congestion control suggests a new total bitrate for this
    /// output. The engine applies it to the encoder only when this output is the
    /// encoder's only adaptive consumer; otherwise it is reported in diagnostics.
    BitrateHint {
        /// Suggested bits per second for all streams of this output.
        bps: u64,
    },
    /// Data was dropped, once per burst.
    Dropped {
        /// Items dropped in the burst.
        items: u32,
        /// Why.
        reason: DropReason,
    },
}

/// Counters of a session. Cheap to read. Adapters start from
/// `TransportStats::default()` and set fields.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransportStats {
    /// Bytes handed to the network or file.
    pub bytes_sent: u64,
    /// Bytes waiting in the send queue.
    pub queued_bytes: u64,
    /// Items dropped since start.
    pub items_dropped: u64,
    /// Reconnects since start.
    pub reconnects: u32,
    /// Estimated available bandwidth, when `transport.bandwidth_estimate` is declared.
    pub bandwidth_bps: Option<u64>,
    /// Round-trip time, when the protocol reports it.
    pub rtt: Option<Duration>,
}

/// Creates transport sessions. One per adapter, registered in a
/// `Registry<dyn TransportFactory>`.
pub trait TransportFactory: AdapterFactory {
    /// Creates a session. Validates the configuration; opens no connection and starts
    /// no thread.
    fn create(&self, config: &TransportConfig) -> Result<Box<dyn Transport>, CreateError>;
}

/// One transport session, driven by one output worker at a time.
///
/// Call order: `start`, any number of `send` and `poll_events`, then `finish`.
pub trait Transport: Send {
    /// Starts delivery: opens the file or starts connecting in the background.
    /// Returns without waiting for the network. `streams` are the descriptors of the
    /// output in [`StreamIndex`] order (used by packet transports for their headers).
    fn start(&mut self, streams: &[StreamDescriptor]) -> Result<(), TransportError>;

    /// **Hot.** Queues one item. Never blocks, whatever the network or disk does:
    /// I/O runs on a thread or runtime the session owns, and only the producer side
    /// (this call) decides what to drop, sharing nothing but atomics and a
    /// single-producer queue with the I/O side.
    ///
    /// When a queue limit is reached, network transports drop video from the oldest
    /// queued non-keyframe up to (not including) the next keyframe, so the stream
    /// never continues from a broken reference, and refuse new non-keyframes until a
    /// keyframe arrives; audio is dropped oldest first only when dropping video did
    /// not free enough; headers are never dropped. Transports that must not drop
    /// (files) return [`TransportError::QueueFull`] instead.
    fn send(&mut self, item: TransportItem<'_>) -> Result<SendOutcome, TransportError>;

    /// Moves pending events to `out`.
    fn poll_events(&mut self, out: &mut Vec<TransportEvent>);

    /// Current counters.
    fn stats(&self) -> TransportStats;

    /// Delivers what is queued, closes cleanly and stops every thread, socket and
    /// handle the session owns. May block up to `timeout`; never called from a hot
    /// path. Returns [`TransportError::Timeout`] if data was left undelivered.
    fn finish(&mut self, timeout: Duration) -> Result<(), TransportError>;
}
