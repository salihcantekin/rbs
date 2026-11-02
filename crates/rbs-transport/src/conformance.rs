//! Conformance suite for transport adapters.
//!
//! ```ignore
//! let mut dest = TempFileDestination::new("my-adapter");
//! rbs_transport::conformance::transport(&FileTransportFactory, &mut dest).assert_passed();
//! ```
//!
//! The caller supplies a [`Destination`]: something that accepts what the transport
//! sends and lets the suite read it back (a temporary file, a local SRT listener, a
//! local RTMP server). Network cases are skipped when the destination cannot provide
//! them. Test support only.

use std::time::{Duration, Instant};

use rbs_core::capability::CapabilitySet;
pub use rbs_core::conformance::{Case, Outcome, PROBE_BUDGET, Report};
use rbs_core::conformance::{flag, guarded, probe_case, set_values};
use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, ChunkKind, CodecId, ColorSpace, ContainerId,
    EncodedPacket, MuxChunk, PixelFormat, Size, StreamDescriptor, StreamIndex, StreamParams,
    VideoFormat,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::FrameRate;

use crate::caps;
use crate::config::{QueueLimits, Secret, TransportConfig, TransportInput};
use crate::error::TransportError;
use crate::transport::{SendOutcome, TransportEvent, TransportFactory, TransportItem};

/// The longest a single `send` may take in the stalled-destination case. Generous on
/// purpose: a non-blocking `send` takes microseconds, a blocking one waits for the
/// network (seconds), so a high ceiling separates them without flaking on a busy CI
/// machine.
pub const SEND_BUDGET: Duration = Duration::from_millis(250);

const SECRET: &str = "rbs-conformance-secret-7f3a";

/// Where the suite sends data, supplied by the adapter's test.
pub trait Destination {
    /// A URL whose received data the suite can read back with [`Destination::received`].
    fn url(&mut self) -> String;

    /// A URL that accepts the connection (or file) but never makes progress, to check
    /// that `send` never blocks. `None` skips that case (files cannot stall).
    fn stalled_url(&mut self) -> Option<String> {
        None
    }

    /// The bytes received at the last [`Destination::url`], after `finish`. `None`
    /// when the destination cannot be read back; the delivery case is then skipped.
    fn received(&mut self) -> Option<Vec<u8>>;
}

/// A destination in the system temporary directory, for `file` transports.
#[derive(Debug)]
pub struct TempFileDestination {
    path: std::path::PathBuf,
    counter: u32,
}

impl TempFileDestination {
    /// Files are named after `tag` and the process id, so parallel tests do not clash.
    pub fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("rbs-conformance-{tag}-{}", std::process::id()));
        Self { path, counter: 0 }
    }

    fn current(&self) -> std::path::PathBuf {
        self.path.with_extension(format!("{}.bin", self.counter))
    }
}

impl Destination for TempFileDestination {
    fn url(&mut self) -> String {
        self.counter += 1;
        let path = self.current();
        let _ = std::fs::remove_file(&path);
        format!("file://{}", path.display())
    }

    fn received(&mut self) -> Option<Vec<u8>> {
        let path = self.current();
        let data = std::fs::read(&path).ok();
        let _ = std::fs::remove_file(&path);
        data
    }
}

fn streams() -> Vec<StreamDescriptor> {
    vec![
        StreamDescriptor::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            Bytes::new(),
            StreamParams::video(
                VideoFormat::new(
                    Size::new(320, 240),
                    PixelFormat::Nv12,
                    ColorSpace::BT709_LIMITED,
                ),
                FrameRate::FPS_30,
            ),
        ),
        StreamDescriptor::new(
            CodecId::Aac,
            BitstreamFormat::Adts,
            Bytes::new(),
            StreamParams::audio(AudioFormat::STEREO_48K, 1024),
        ),
    ]
}

fn media_chunk(index: u64, size: usize) -> MuxChunk {
    MuxChunk::new(
        ChunkKind::Media {
            keyframe: index.is_multiple_of(30),
            timestamp: Timestamp::from_nanos(1_000_000_000 + index * 33_333_333),
        },
        Bytes::from(vec![(index % 251) as u8; size]),
    )
}

fn video_packet(index: u64, size: usize) -> EncodedPacket {
    let ts = Timestamp::from_nanos(1_000_000_000 + index * 33_333_333);
    let mut data = vec![
        0,
        0,
        0,
        1,
        if index.is_multiple_of(30) { 0x65 } else { 0x41 },
    ];
    data.resize(size.max(5), (index % 251) as u8);
    EncodedPacket::new(
        CodecId::H264,
        BitstreamFormat::AnnexB,
        ts,
        ts,
        index.is_multiple_of(30),
        Bytes::from(data),
    )
    .unwrap_or_else(|_| unreachable!("synthetic packets have dts == pts"))
    .with_duration(Duration::from_nanos(33_333_333))
}

/// Runs the transport suite against `factory`, sending to `destination`.
pub fn transport(factory: &dyn TransportFactory, destination: &mut dyn Destination) -> Report {
    let mut report = Report::default();
    let Some(caps) = probe_case(&mut report, factory) else {
        return report;
    };
    let inputs = set_values(&caps, &caps::INPUT);
    if inputs.is_empty() {
        report.record(
            "transport.input",
            Outcome::Failed("no input declared".into()),
        );
    }
    if set_values(&caps, &caps::SCHEMES).is_empty() {
        report.record(
            "transport.schemes",
            Outcome::Failed("no scheme declared".into()),
        );
    }
    for name in &inputs {
        let input = if name == caps::INPUT_PACKETS {
            TransportInput::Packets
        } else if let Some(c) = name
            .strip_prefix("container.")
            .and_then(ContainerId::from_name)
        {
            TransportInput::Container(c)
        } else {
            report.record(name.clone(), Outcome::Failed("unknown input value".into()));
            continue;
        };
        not_started_case(&mut report, name, factory, destination, input);
        wrong_input_case(&mut report, name, factory, destination, input);
        delivery_case(&mut report, name, factory, destination, input, &caps);
        stalled_case(&mut report, name, factory, destination, input);
    }
    report
}

fn config(url: String, input: TransportInput) -> TransportConfig {
    TransportConfig::new(url, input).with_secret(Secret::new(SECRET))
}

fn item_for<'a>(
    input: TransportInput,
    chunk: &'a MuxChunk,
    packet: &'a EncodedPacket,
) -> TransportItem<'a> {
    match input {
        TransportInput::Packets => TransportItem::Packet {
            stream: StreamIndex(0),
            packet,
        },
        TransportInput::Container(_) => TransportItem::Chunk(chunk),
    }
}

fn header_chunk() -> MuxChunk {
    MuxChunk::new(ChunkKind::Header, Bytes::from_static(b"RBS-HEADER"))
}

fn not_started_case(
    report: &mut Report,
    name: &str,
    factory: &dyn TransportFactory,
    destination: &mut dyn Destination,
    input: TransportInput,
) {
    let url = destination.url();
    let result = guarded(|| {
        let mut t = factory
            .create(&config(url, input))
            .map_err(|e| format!("create: {e}"))?;
        let chunk = header_chunk();
        let packet = video_packet(0, 64);
        match t.send(item_for(input, &chunk, &packet)) {
            Err(TransportError::NotStarted) => Ok(()),
            other => Err(format!("send before start returned {other:?}")),
        }
    });
    destination.received();
    report.check(format!("{name}/send-before-start"), result);
}

fn wrong_input_case(
    report: &mut Report,
    name: &str,
    factory: &dyn TransportFactory,
    destination: &mut dyn Destination,
    input: TransportInput,
) {
    let url = destination.url();
    let result = guarded(|| {
        let mut t = factory
            .create(&config(url, input))
            .map_err(|e| format!("create: {e}"))?;
        t.start(&streams()).map_err(|e| format!("start: {e}"))?;
        let chunk = header_chunk();
        let packet = video_packet(0, 64);
        let wrong = match input {
            TransportInput::Packets => TransportItem::Chunk(&chunk),
            TransportInput::Container(_) => TransportItem::Packet {
                stream: StreamIndex(0),
                packet: &packet,
            },
        };
        let result = match t.send(wrong) {
            Err(TransportError::InvalidInput(_)) => Ok(()),
            other => Err(format!("wrong item kind returned {other:?}")),
        };
        let _ = t.finish(Duration::from_secs(1));
        result
    });
    destination.received();
    report.check(format!("{name}/wrong-input"), result);
}

fn delivery_case(
    report: &mut Report,
    name: &str,
    factory: &dyn TransportFactory,
    destination: &mut dyn Destination,
    input: TransportInput,
    caps: &CapabilitySet,
) {
    let case = format!("{name}/delivery");
    let url = destination.url();
    let config = config(url, input);
    let seekable = flag(caps, &caps::SEEKABLE);
    let mut expected = Vec::new();
    let mut events = Vec::new();
    let mut error_text = String::new();
    let run = guarded(|| {
        let mut t = factory
            .create(&config)
            .map_err(|e| format!("create: {e}"))?;
        t.start(&streams()).map_err(|e| format!("start: {e}"))?;
        let header = header_chunk();
        let mut chunks = vec![header];
        chunks.extend((0..60).map(|i| media_chunk(i, 1000 + i as usize)));
        chunks.push(MuxChunk::new(
            ChunkKind::Trailer,
            Bytes::from_static(b"RBS-TRAILER"),
        ));
        chunks.push(MuxChunk::new(
            ChunkKind::Patch { offset: 4 },
            Bytes::from_static(b"PATCHED"),
        ));
        for (i, chunk) in chunks.iter().enumerate() {
            let packet = video_packet(i as u64, 1000 + i);
            match t.send(item_for(input, chunk, &packet)) {
                Ok(SendOutcome::Queued) => {}
                Ok(other) => return Err(format!("item {i}: {other:?} with a healthy destination")),
                Err(e) => {
                    error_text.push_str(&e.to_string());
                    return Err(format!("item {i}: {e}"));
                }
            }
            if chunk.is_sequential() {
                expected.extend_from_slice(&chunk.data);
            }
        }
        if seekable {
            expected[4..11].copy_from_slice(b"PATCHED");
        }
        if let Err(e) = t.finish(Duration::from_secs(5)) {
            error_text.push_str(&e.to_string());
            return Err(format!("finish: {e}"));
        }
        t.poll_events(&mut events);
        Ok(())
    });
    let received = destination.received();
    if let Err(e) = run {
        report.record(case, Outcome::Failed(e));
        return;
    }
    report.check(format!("{name}/secret-redaction"), {
        let shown = format!("{config:?} {events:?} {error_text}");
        if shown.contains(SECRET) {
            Err("the secret appears in Debug output, events or errors".into())
        } else {
            Ok(())
        }
    });
    match (input, received) {
        (TransportInput::Packets, _) => report.record(
            case,
            Outcome::Skipped("packet framing is checked by the adapter's own tests".into()),
        ),
        (_, None) => report.record(
            case,
            Outcome::Skipped("destination cannot be read back".into()),
        ),
        (_, Some(bytes)) => report.check(
            case,
            if bytes == expected {
                Ok(())
            } else {
                Err(format!(
                    "received {} bytes, expected {} (byte-exact header, media and trailer{})",
                    bytes.len(),
                    expected.len(),
                    if seekable {
                        " with patches applied"
                    } else {
                        ""
                    }
                ))
            },
        ),
    }
}

fn stalled_case(
    report: &mut Report,
    name: &str,
    factory: &dyn TransportFactory,
    destination: &mut dyn Destination,
    input: TransportInput,
) {
    let case = format!("{name}/never-blocks");
    let Some(url) = destination.stalled_url() else {
        report.record(case, Outcome::Skipped("no stalled destination".into()));
        return;
    };
    let config =
        config(url, input).with_queue(QueueLimits::new(1024 * 1024, Duration::from_secs(1)));
    let mut shown = String::new();
    let result = guarded(|| {
        let mut t = factory
            .create(&config)
            .map_err(|e| format!("create: {e}"))?;
        t.start(&streams()).map_err(|e| format!("start: {e}"))?;
        let header = header_chunk();
        let packet = video_packet(0, 64);
        t.send(item_for(input, &header, &packet))
            .map_err(|e| format!("header: {e}"))?;
        let mut dropped = 0_u64;
        let mut refused = 0_u64;
        // After a non-keyframe is refused, the rest of its GOP must be refused too.
        let mut gop_cut = false;
        let mut slowest = Duration::ZERO;
        for i in 0..200 {
            let chunk = media_chunk(i, 64 * 1024);
            let packet = video_packet(i, 64 * 1024);
            let keyframe = packet.keyframe();
            if keyframe {
                gop_cut = false;
            }
            let started = Instant::now();
            let outcome = t.send(item_for(input, &chunk, &packet));
            slowest = slowest.max(started.elapsed());
            match outcome {
                Ok(SendOutcome::Dropped { .. }) => {
                    refused += 1;
                    dropped += 1;
                    gop_cut |= !keyframe;
                }
                Ok(other) if gop_cut => {
                    return Err(format!(
                        "item {i} continues a GOP that lost frames ({other:?}); it must be refused until the next keyframe"
                    ));
                }
                Ok(SendOutcome::QueuedAfterDrops { dropped: n, .. }) => dropped += u64::from(n),
                Ok(_) => {}
                Err(e) if e.is_recoverable() => shown.push_str(&e.to_string()),
                Err(e) => return Err(format!("send {i}: {e}")),
            }
        }
        if refused == 0 {
            return Err(
                "GOPs of 1.9 MiB into a 1 MiB queue: the current GOP must be cut and its remaining non-keyframes refused, but nothing was refused".into(),
            );
        }
        if slowest > SEND_BUDGET {
            return Err(format!(
                "slowest send took {slowest:?}, budget {SEND_BUDGET:?}"
            ));
        }
        if dropped == 0 && t.stats().items_dropped == 0 {
            return Err(
                "12.5 MiB sent to a stalled destination with a 1 MiB queue, nothing dropped".into(),
            );
        }
        let mut events = Vec::new();
        t.poll_events(&mut events);
        shown.push_str(&format!("{events:?}"));
        if !events
            .iter()
            .any(|e| matches!(e, TransportEvent::Dropped { .. }))
        {
            return Err("drops were not reported as an event".into());
        }
        let started = Instant::now();
        if let Err(e) = t.finish(Duration::from_millis(200)) {
            shown.push_str(&e.to_string());
        }
        if started.elapsed() > Duration::from_secs(5) {
            return Err(format!("finish(200 ms) took {:?}", started.elapsed()));
        }
        Ok(())
    });
    if let Err(e) = &result {
        shown.push_str(e);
    }
    report.check(case, result);
    report.check(
        format!("{name}/secret-redaction-stalled"),
        if shown.contains(SECRET) {
            Err("the secret appears in errors or events of a stalled session".into())
        } else {
            Ok(())
        },
    );
}
