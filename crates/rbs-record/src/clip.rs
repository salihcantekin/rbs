//! Writes a replay clip to a file (ADR-0088 decision 4): the packets are muxed as they
//! are, without re-encoding, by a throwaway muxer and file transport.
//!
//! [`write_clip`] blocks until the file is closed, so run it on a worker thread, never
//! on a real-time path or the control thread. It touches no output, the buffer or the
//! graph: capture and every running output go on while it writes.

use std::fmt;
use std::time::{Duration, Instant};

use rbs_core::media::{ContainerId, MediaError, MuxChunk, StreamDescriptor};
use rbs_mux::{MuxError, MuxerConfig};
use rbs_nodes::replay::{Clip, ClipBounds};
use rbs_transport::{TransportConfig, TransportError, TransportInput, TransportItem};

use crate::recording::{Port, RecordAdapters, RecordError};
use crate::select::{self, Chosen};

/// How long `write_clip` waits for room in the file transport's queue, and for the file
/// to close, before it gives up.
pub const CLIP_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Pause between two tries while the file transport's queue is full.
const RETRY_PAUSE: Duration = Duration::from_millis(2);

/// What `write_clip` wrote.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ClipFile {
    /// Packets handed to the muxer.
    pub packets: u64,
    /// Packets the muxer refused as out of order (should stay 0).
    pub mux_drops: u64,
    /// Where the clip starts and ends on the master clock; the clip's file starts at 0.
    pub bounds: ClipBounds,
    /// The muxer used.
    pub muxer: Chosen,
    /// The transport used.
    pub transport: Chosen,
}

/// Why a clip could not be written.
#[derive(Debug)]
#[non_exhaustive]
pub enum ClipWriteError {
    /// No muxer or transport can serve the container or URL.
    Adapter(RecordError),
    /// A packet could not be rebased (an inconsistent encoder timestamp).
    Media(MediaError),
    /// The muxer failed.
    Mux(MuxError),
    /// The transport failed, or its queue stayed full past the timeout.
    Transport(TransportError),
}

impl fmt::Display for ClipWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Adapter(e) => write!(f, "{e}"),
            Self::Media(e) => write!(f, "a packet cannot be rebased: {e}"),
            Self::Mux(e) => write!(f, "muxer: {e}"),
            Self::Transport(e) => write!(f, "transport: {e}"),
        }
    }
}

impl std::error::Error for ClipWriteError {}

/// Writes `clip` to `url` in `container`. `streams` are the buffer's stream descriptors
/// ([`Studio::replay_streams`](crate::Studio::replay_streams)). Timestamps are rebased
/// so the earliest sample is 0; nothing is re-encoded.
pub fn write_clip(
    adapters: &RecordAdapters,
    clip: &Clip,
    streams: &[StreamDescriptor],
    url: &str,
    container: ContainerId,
) -> Result<ClipFile, ClipWriteError> {
    let entries = clip.rebased().map_err(ClipWriteError::Media)?;
    let (muxer_factory, muxer_chosen) =
        select::muxer(&adapters.muxers, container).map_err(ClipWriteError::Adapter)?;
    let (transport_factory, transport_chosen) = select::transport(
        &adapters.transports,
        url,
        TransportInput::Container(container),
    )
    .map_err(ClipWriteError::Adapter)?;
    let mut muxer = muxer_factory
        .create(&MuxerConfig::new(container, streams.to_vec()))
        .map_err(|e| ClipWriteError::Adapter(refused(Port::Muxer, &muxer_chosen, &e)))?;
    let mut transport = transport_factory
        .create(&TransportConfig::new(
            url,
            TransportInput::Container(container),
        ))
        .map_err(|e| ClipWriteError::Adapter(refused(Port::Transport, &transport_chosen, &e)))?;
    transport
        .start(streams)
        .map_err(ClipWriteError::Transport)?;

    let deadline = Instant::now() + CLIP_WRITE_TIMEOUT;
    let mut chunks: Vec<MuxChunk> = Vec::new();
    let mut send_all = |chunks: &mut Vec<MuxChunk>| -> Result<(), ClipWriteError> {
        for chunk in chunks.drain(..) {
            loop {
                match transport.send(TransportItem::Chunk(&chunk)) {
                    Ok(_) => break,
                    Err(TransportError::QueueFull) if Instant::now() < deadline => {
                        std::thread::sleep(RETRY_PAUSE);
                    }
                    Err(e) => return Err(ClipWriteError::Transport(e)),
                }
            }
        }
        Ok(())
    };
    let mut packets = 0_u64;
    let mut mux_drops = 0_u64;
    let result = (|| {
        muxer.begin(&mut chunks).map_err(ClipWriteError::Mux)?;
        send_all(&mut chunks)?;
        for e in &entries {
            match muxer.write(e.stream, &e.packet, &mut chunks) {
                Ok(()) => packets += 1,
                Err(MuxError::NonMonotonic { .. }) => mux_drops += 1,
                Err(err) => return Err(ClipWriteError::Mux(err)),
            }
            send_all(&mut chunks)?;
        }
        muxer.finish(&mut chunks).map_err(ClipWriteError::Mux)?;
        send_all(&mut chunks)
    })();
    // Close the file whatever happened: a half-written clip is still a valid
    // fragmented file, and the transport's threads and handles must end.
    let finished = transport
        .finish(deadline.saturating_duration_since(Instant::now()))
        .map_err(ClipWriteError::Transport);
    result?;
    finished?;
    Ok(ClipFile {
        packets,
        mux_drops,
        bounds: clip.bounds,
        muxer: muxer_chosen,
        transport: transport_chosen,
    })
}

/// An adapter that was chosen but refused to create its session.
fn refused(port: Port, chosen: &Chosen, e: &dyn fmt::Display) -> RecordError {
    RecordError::NoAdapter {
        port,
        tried: vec![(chosen.adapter.clone(), e.to_string())],
    }
}
