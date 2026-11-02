//! Checks the transport suite with two fakes: a buffered file writer (container input)
//! and an in-memory "network" transport (packet input) with a bounded queue.

use std::collections::VecDeque;
use std::time::Duration;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{ChunkKind, StreamDescriptor};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_transport::caps;
use rbs_transport::conformance::{self, Destination, Report, TempFileDestination};
use rbs_transport::{
    CreateError, DropReason, SendOutcome, Transport, TransportConfig, TransportError,
    TransportEvent, TransportFactory, TransportInput, TransportItem, TransportStats,
};

fn set(values: &[&str]) -> CapabilityValue {
    CapabilityValue::Set(values.iter().map(|v| v.to_string()).collect())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    None,
    LosesTrailer,
    IgnoresPatches,
    Blocks,
    NeverDrops,
    AcceptsBeforeStart,
    DropsSingleFrames,
    NoKeyframeRequest,
}

struct FileFactory(Fault);

impl AdapterFactory for FileFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new("test.transport.file")
    }
    fn display_name(&self) -> &str {
        "test file"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new()
            .with(caps::SCHEMES, set(&["file"]))
            .with(
                caps::INPUT,
                set(&["container.mpegts", "container.matroska"]),
            )
            .with(caps::SEEKABLE, CapabilityValue::Flag(true)))
    }
}

impl TransportFactory for FileFactory {
    fn create(&self, config: &TransportConfig) -> Result<Box<dyn Transport>, CreateError> {
        let path = config
            .url
            .strip_prefix("file://")
            .ok_or_else(|| CreateError::Unsupported {
                what: "scheme".into(),
            })?;
        Ok(Box::new(FileSink {
            fault: self.0,
            path: path.into(),
            started: false,
            data: Vec::new(),
        }))
    }
}

/// Buffers in memory and writes on finish: fine for a test fake.
struct FileSink {
    fault: Fault,
    path: std::path::PathBuf,
    started: bool,
    data: Vec<u8>,
}

impl Transport for FileSink {
    fn start(&mut self, _: &[StreamDescriptor]) -> Result<(), TransportError> {
        self.started = true;
        Ok(())
    }
    fn send(&mut self, item: TransportItem<'_>) -> Result<SendOutcome, TransportError> {
        if !self.started {
            return Err(TransportError::NotStarted);
        }
        let TransportItem::Chunk(chunk) = item else {
            return Err(TransportError::InvalidInput("expected chunks".into()));
        };
        match chunk.kind {
            ChunkKind::Patch { offset } => {
                if self.fault != Fault::IgnoresPatches {
                    let start = offset as usize;
                    let end = start + chunk.data.len();
                    if end > self.data.len() {
                        return Err(TransportError::Io("patch past end".into()));
                    }
                    self.data[start..end].copy_from_slice(&chunk.data);
                }
            }
            ChunkKind::Trailer if self.fault == Fault::LosesTrailer => {}
            _ => self.data.extend_from_slice(&chunk.data),
        }
        Ok(SendOutcome::Queued)
    }
    fn poll_events(&mut self, _: &mut Vec<TransportEvent>) {}
    fn stats(&self) -> TransportStats {
        TransportStats::default()
    }
    fn finish(&mut self, _: Duration) -> Result<(), TransportError> {
        std::fs::write(&self.path, &self.data).map_err(|e| TransportError::Io(e.to_string()))
    }
}

struct NetFactory(Fault);

impl AdapterFactory for NetFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new("test.transport.net")
    }
    fn display_name(&self) -> &str {
        "test network"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new()
            .with(caps::SCHEMES, set(&["fake"]))
            .with(caps::INPUT, set(&[caps::INPUT_PACKETS]))
            .with(caps::CODECS, set(&["h264", "aac"])))
    }
}

impl TransportFactory for NetFactory {
    fn create(&self, config: &TransportConfig) -> Result<Box<dyn Transport>, CreateError> {
        if config.input != TransportInput::Packets {
            return Err(CreateError::Unsupported {
                what: "container input".into(),
            });
        }
        Ok(Box::new(Net {
            fault: self.0,
            stalled: config.url.ends_with("stalled"),
            max_bytes: config.queue.max_bytes,
            started: false,
            queue: VecDeque::new(),
            gop: 0,
            cut: false,
            keyframe_wanted: false,
            queued_bytes: 0,
            stats: TransportStats::default(),
            pending_drop: 0,
            stalled_sends: 0,
        }))
    }
}

impl Net {
    fn count_drop(&mut self, n: u32) {
        self.stats.items_dropped += u64::from(n);
        self.pending_drop += n;
    }
}

struct Net {
    fault: Fault,
    stalled: bool,
    max_bytes: usize,
    started: bool,
    queue: VecDeque<(u64, bool, usize)>,
    gop: u64,
    cut: bool,
    keyframe_wanted: bool,
    queued_bytes: usize,
    stats: TransportStats,
    pending_drop: u32,
    stalled_sends: u32,
}

impl Transport for Net {
    fn start(&mut self, _: &[StreamDescriptor]) -> Result<(), TransportError> {
        self.started = true;
        Ok(())
    }
    fn send(&mut self, item: TransportItem<'_>) -> Result<SendOutcome, TransportError> {
        if !self.started && self.fault != Fault::AcceptsBeforeStart {
            return Err(TransportError::NotStarted);
        }
        let TransportItem::Packet { packet, .. } = item else {
            return Err(TransportError::InvalidInput("expected packets".into()));
        };
        if !self.stalled {
            self.stats.bytes_sent += packet.data().len() as u64;
            return Ok(SendOutcome::Queued);
        }
        self.stalled_sends += 1;
        if self.fault == Fault::Blocks && self.stalled_sends == 10 {
            // Simulates a send that waits for the network. This sleep is the behaviour
            // under test, not a wait for timing: the suite must flag it.
            std::thread::sleep(conformance::SEND_BUDGET * 2);
        }
        let keyframe = packet.keyframe();
        let size = packet.data().len();
        if keyframe {
            self.gop += 1;
            self.cut = false;
        } else if self.cut && self.fault != Fault::DropsSingleFrames {
            // The current GOP lost frames: refuse the rest of it.
            self.count_drop(1);
            return Ok(SendOutcome::Dropped {
                reason: DropReason::QueueFull,
            });
        }
        self.queue.push_back((self.gop, keyframe, size));
        self.queued_bytes += size;
        let mut dropped = 0;
        let mut refused_current = false;
        while self.queued_bytes > self.max_bytes && self.fault != Fault::NeverDrops {
            let oldest_gop = self.queue.front().map_or(self.gop, |(g, _, _)| *g);
            let index = if self.fault == Fault::DropsSingleFrames {
                // The broken policy: drop single oldest non-keyframes.
                self.queue.iter().position(|(_, k, _)| !k).unwrap_or(0)
            } else if oldest_gop < self.gop {
                // A later keyframe is queued: drop the whole oldest GOP.
                while self.queue.front().is_some_and(|(g, _, _)| *g == oldest_gop) {
                    if let Some((_, _, size)) = self.queue.pop_front() {
                        self.queued_bytes -= size;
                        dropped += 1;
                    }
                }
                continue;
            } else {
                // Only the current GOP is queued: cut its newest non-keyframe tail.
                if !self.cut {
                    self.keyframe_wanted = true;
                }
                self.cut = true;
                match self.queue.iter().rposition(|(_, k, _)| !k) {
                    Some(i) => i,
                    None => break,
                }
            };
            if index + 1 == self.queue.len() && self.cut {
                refused_current = true;
            }
            if let Some((_, _, size)) = self.queue.remove(index) {
                self.queued_bytes -= size;
                dropped += 1;
            }
        }
        self.stats.queued_bytes = self.queued_bytes as u64;
        if dropped > 0 {
            self.count_drop(dropped);
            if refused_current {
                return Ok(SendOutcome::Dropped {
                    reason: DropReason::QueueFull,
                });
            }
            return Ok(SendOutcome::QueuedAfterDrops {
                dropped,
                reason: DropReason::QueueFull,
            });
        }
        Ok(SendOutcome::Queued)
    }
    fn poll_events(&mut self, out: &mut Vec<TransportEvent>) {
        if self.pending_drop > 0 {
            out.push(TransportEvent::Dropped {
                items: self.pending_drop,
                reason: DropReason::QueueFull,
            });
            self.pending_drop = 0;
        }
        if self.keyframe_wanted && self.fault != Fault::NoKeyframeRequest {
            out.push(TransportEvent::KeyframeRequested);
            self.keyframe_wanted = false;
        }
    }
    fn stats(&self) -> TransportStats {
        self.stats
    }
    fn finish(&mut self, _: Duration) -> Result<(), TransportError> {
        if self.stalled && !self.queue.is_empty() {
            return Err(TransportError::Timeout);
        }
        Ok(())
    }
}

struct NetDestination;

impl Destination for NetDestination {
    fn url(&mut self) -> String {
        "fake://ok".into()
    }
    fn stalled_url(&mut self) -> Option<String> {
        Some("fake://stalled".into())
    }
    fn received(&mut self) -> Option<Vec<u8>> {
        None
    }
}

fn failed(report: &Report) -> Vec<String> {
    report.failures().map(|c| c.name.clone()).collect()
}

fn file_report(fault: Fault) -> Report {
    let tag = format!("{fault:?}").to_lowercase();
    conformance::transport(&FileFactory(fault), &mut TempFileDestination::new(&tag))
}

#[test]
fn correct_file_transport_passes() {
    let report = file_report(Fault::None);
    report.assert_ran(&["container.mpegts/delivery", "container.matroska/delivery"]);
    let passed = report
        .cases
        .iter()
        .filter(|c| c.name.ends_with("/delivery"))
        .count();
    assert_eq!(passed, 2, "{report}");
}

#[test]
fn file_faults_are_caught() {
    for fault in [Fault::LosesTrailer, Fault::IgnoresPatches] {
        let report = file_report(fault);
        assert!(
            failed(&report)
                .iter()
                .any(|c| c == "container.mpegts/delivery"),
            "{fault:?}\n{report}"
        );
    }
}

#[test]
fn correct_network_transport_passes() {
    let report = conformance::transport(&NetFactory(Fault::None), &mut NetDestination);
    report.assert_passed();
    assert!(
        report
            .cases
            .iter()
            .any(|c| c.name == "packets/never-blocks" && c.outcome == conformance::Outcome::Passed),
        "{report}"
    );
}

#[test]
fn network_faults_are_caught() {
    let expectations = [
        (Fault::Blocks, "packets/never-blocks"),
        (Fault::NeverDrops, "packets/never-blocks"),
        (Fault::AcceptsBeforeStart, "packets/send-before-start"),
        (Fault::DropsSingleFrames, "packets/never-blocks"),
        (Fault::NoKeyframeRequest, "packets/never-blocks"),
    ];
    for (fault, case) in expectations {
        let report = conformance::transport(&NetFactory(fault), &mut NetDestination);
        assert!(
            failed(&report).iter().any(|c| c == case),
            "{fault:?}: expected {case}\n{report}"
        );
    }
}
