//! Splitting a graph description into partitions joined by queues (ADR-0025).
//!
//! Every node belongs to a partition ([`NodeEntry::partition`]); each partition runs
//! on its own executor (render, one per encoder, one per output). The scene and the
//! application edit one *logical* description. A [`Lowering`] turns it into one
//! description per partition: a media link between partitions becomes a queue writer
//! node in the producer's partition and a queue reader node in the consumer's, with
//! ids from the reserved range ([`NodeId::RESERVED_START`]) that stay the same across
//! edits, so the runtime keeps the endpoints running across swaps.
//!
//! The lowering is plain control-thread code. It never touches executors: the
//! application adopts the endpoint nodes it reports into the right controllers and
//! applies each partition's description to its controller.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use rbs_core::lifecycle::Lifecycle;

use crate::description::{GraphDescription, Link, NodeEntry, NodeId, PartitionId, PinRef};
use crate::node::{LinkKind, NodeDescriptor};
use crate::runtime::queue::{
    self, Doorbell, PACKET_QUEUE_PACKETS, PacketReader, PacketWriter, QueueStats, VideoReader,
    VideoWriter,
};
use crate::runtime::{Node, Runtime};

/// A link between partitions and the queue that carries it.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct Crossing {
    /// The logical link.
    pub link: Link,
    /// The producer's partition.
    pub from: PartitionId,
    /// The consumer's partition.
    pub to: PartitionId,
    /// The writer endpoint, in `from`.
    pub writer: NodeId,
    /// The reader endpoint, in `to`.
    pub reader: NodeId,
    /// The queue's counters. Give them to the consumer node: an encoder reads
    /// [`QueueStats::video_drops`], an output must fail when
    /// [`QueueStats::overflows`] grows.
    pub stats: Arc<QueueStats>,
}

/// The endpoint nodes of a new queue, to adopt into the two partitions' controllers.
pub enum QueueEnds {
    /// A video queue (newest frame wins).
    Video(VideoWriter, VideoReader),
    /// A packet queue (never drops silently; rings the consumer's doorbell).
    Packets(PacketWriter, PacketReader),
}

impl QueueEnds {
    /// The writer as a node for the producer partition's runtime `X`.
    pub fn writer<X: Runtime>(self) -> (Box<dyn Node<X>>, QueueReaderEnd) {
        match self {
            QueueEnds::Video(w, r) => (Box::new(w), QueueReaderEnd::Video(r)),
            QueueEnds::Packets(w, r) => (Box::new(w), QueueReaderEnd::Packets(r)),
        }
    }
}

impl fmt::Debug for QueueEnds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueEnds::Video(..) => f.write_str("QueueEnds::Video"),
            QueueEnds::Packets(..) => f.write_str("QueueEnds::Packets"),
        }
    }
}

/// The reader half of [`QueueEnds`], after the writer was taken.
pub enum QueueReaderEnd {
    /// A video reader.
    Video(VideoReader),
    /// A packet reader.
    Packets(PacketReader),
}

impl QueueReaderEnd {
    /// The reader as a node for the consumer partition's runtime `Y`.
    pub fn into_node<Y: Runtime>(self) -> Box<dyn Node<Y>> {
        match self {
            QueueReaderEnd::Video(r) => Box::new(r),
            QueueReaderEnd::Packets(r) => Box::new(r),
        }
    }
}

/// A queue created by this lowering.
#[non_exhaustive]
#[derive(Debug)]
pub struct NewQueue {
    /// Where it sits.
    pub crossing: Crossing,
    /// Its endpoint nodes: adopt the writer into `crossing.from`'s controller under
    /// `crossing.writer` and the reader into `crossing.to`'s under `crossing.reader`.
    pub ends: QueueEnds,
}

/// Why a link between partitions was left out.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LowerProblem {
    /// Only video and packet links can cross partitions; audio runs on the audio
    /// executor and bytes and data links are not carried by queues yet.
    UnsupportedKind(LinkKind),
    /// The reserved id range for queue endpoints is used up (about 134 million
    /// queues created by one lowering).
    IdsExhausted,
}

/// The result of [`Lowering::lower`].
#[non_exhaustive]
#[derive(Debug, Default)]
pub struct Lowered {
    /// One description per partition, for that partition's controller. A partition
    /// that lost its last node is still listed, empty, once, so its controller lets
    /// its nodes go.
    pub parts: BTreeMap<PartitionId, GraphDescription>,
    /// Every queue in use.
    pub crossings: Vec<Crossing>,
    /// Queues created by this call.
    pub created: Vec<NewQueue>,
    /// Queues removed by this call. Their endpoints are no longer in any part; take
    /// them back from their controllers after the next swap and drop them.
    pub retired: Vec<Crossing>,
    /// Nodes whose partition changed since the last call: `(node, old, new)`. Apply
    /// in two phases: first the old partitions' parts (their controllers let the node
    /// go after the swap is collected), then move each instance with `take` and
    /// `adopt`, then apply the new partitions' parts. Applying a new part first fails
    /// with `JoinError::NoInstance` for the moved node.
    pub moved: Vec<(NodeId, PartitionId, PartitionId)>,
    /// Links that could not cross. While this is not empty the parts are not a
    /// faithful split of the logical graph (those links are missing), so their plans
    /// may schedule less than the logical plan.
    pub problems: Vec<(Link, LowerProblem)>,
}

#[derive(Debug, Clone)]
struct Ends {
    writer: NodeId,
    reader: NodeId,
    stats: Arc<QueueStats>,
}

/// Keeps a logical description split into partitions, with stable queue endpoints.
pub struct Lowering {
    ends: HashMap<(Link, PartitionId, PartitionId), Ends>,
    next_offset: u32,
    doorbells: BTreeMap<PartitionId, Arc<Doorbell>>,
    partitions: BTreeSet<PartitionId>,
    assigned: HashMap<NodeId, PartitionId>,
    packet_capacity: usize,
    video_writer: Arc<NodeDescriptor>,
    video_reader: Arc<NodeDescriptor>,
    packet_writer: Arc<NodeDescriptor>,
    packet_reader: Arc<NodeDescriptor>,
}

impl fmt::Debug for Lowering {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lowering")
            .field("queues", &self.ends.len())
            .field("partitions", &self.partitions)
            .finish_non_exhaustive()
    }
}

impl Default for Lowering {
    fn default() -> Self {
        Self::new()
    }
}

impl Lowering {
    /// A lowering with the default packet queue capacity ([`PACKET_QUEUE_PACKETS`]).
    pub fn new() -> Self {
        Self {
            ends: HashMap::new(),
            next_offset: 0,
            doorbells: BTreeMap::new(),
            partitions: BTreeSet::new(),
            assigned: HashMap::new(),
            packet_capacity: PACKET_QUEUE_PACKETS,
            video_writer: Arc::new(queue::writer_descriptor(LinkKind::Video)),
            video_reader: Arc::new(queue::reader_descriptor(LinkKind::Video)),
            packet_writer: Arc::new(queue::writer_descriptor(LinkKind::Packets)),
            packet_reader: Arc::new(queue::reader_descriptor(LinkKind::Packets)),
        }
    }

    /// Capacity of packet queues created from now on (at least 1). Video queues hold
    /// one frame: the newest wins.
    pub fn set_packet_capacity(&mut self, packets: usize) {
        self.packet_capacity = packets.max(1);
    }

    /// The doorbell that packet queues into `partition` ring; its worker waits on it.
    pub fn doorbell(&mut self, partition: PartitionId) -> Arc<Doorbell> {
        Arc::clone(self.doorbells.entry(partition).or_default())
    }

    /// Splits `logical` into one description per partition.
    ///
    /// Lifecycle states are copied from `logical`. A queue endpoint is Active exactly
    /// when its logical link is in `logical.plan()`, so each partition's plan
    /// schedules the same nodes the whole graph's plan does.
    pub fn lower(&mut self, logical: &GraphDescription) -> Lowered {
        let mut out = Lowered::default();
        let global = logical.plan();
        let live: HashSet<Link> = global.links.iter().copied().collect();
        let partition_of: HashMap<NodeId, PartitionId> = logical
            .nodes()
            .iter()
            .map(|n| (n.id, n.partition))
            .collect();

        // Moved nodes.
        for n in logical.nodes() {
            if let Some(&old) = self.assigned.get(&n.id) {
                if old != n.partition {
                    out.moved.push((n.id, old, n.partition));
                }
            }
        }
        self.assigned = partition_of.clone();

        let mut nodes: BTreeMap<PartitionId, Vec<NodeEntry>> = BTreeMap::new();
        let mut links: BTreeMap<PartitionId, Vec<Link>> = BTreeMap::new();
        for n in logical.nodes() {
            nodes.entry(n.partition).or_default().push(n.clone());
        }

        let mut seen: HashSet<(Link, PartitionId, PartitionId)> = HashSet::new();
        for link in logical.links() {
            let (Some(&from), Some(&to)) = (
                partition_of.get(&link.from.node),
                partition_of.get(&link.to.node),
            ) else {
                continue;
            };
            if from == to {
                links.entry(from).or_default().push(*link);
                continue;
            }
            let (writer_desc, reader_desc) = match link.kind {
                LinkKind::Video => (&self.video_writer, &self.video_reader),
                LinkKind::Packets => (&self.packet_writer, &self.packet_reader),
                other => {
                    out.problems
                        .push((*link, LowerProblem::UnsupportedKind(other)));
                    continue;
                }
            };
            let key = (*link, from, to);
            seen.insert(key);
            let ends = match self.ends.get(&key) {
                Some(e) => e.clone(),
                None => {
                    let ids = self.next_offset.checked_add(1).and_then(|r| {
                        Some((NodeId::reserved(self.next_offset)?, NodeId::reserved(r)?))
                    });
                    let Some((writer, reader)) = ids else {
                        seen.remove(&key);
                        out.problems.push((*link, LowerProblem::IdsExhausted));
                        continue;
                    };
                    self.next_offset = self.next_offset.saturating_add(2);
                    let (queue_ends, stats) = match link.kind {
                        LinkKind::Video => {
                            let (w, r, s) = queue::video();
                            (QueueEnds::Video(w, r), s)
                        }
                        _ => {
                            let bell = Arc::clone(self.doorbells.entry(to).or_default());
                            let (w, r, s) = queue::packets(self.packet_capacity, bell);
                            (QueueEnds::Packets(w, r), s)
                        }
                    };
                    let e = Ends {
                        writer,
                        reader,
                        stats,
                    };
                    self.ends.insert(key, e.clone());
                    out.created.push(NewQueue {
                        crossing: crossing(link, from, to, &e),
                        ends: queue_ends,
                    });
                    e
                }
            };
            let state = if live.contains(link) {
                Lifecycle::Active
            } else {
                Lifecycle::Standby
            };
            nodes.entry(from).or_default().push(NodeEntry {
                id: ends.writer,
                descriptor: Arc::clone(writer_desc),
                state,
                partition: from,
            });
            nodes.entry(to).or_default().push(NodeEntry {
                id: ends.reader,
                descriptor: Arc::clone(reader_desc),
                state,
                partition: to,
            });
            links.entry(from).or_default().push(Link {
                from: link.from,
                to: PinRef {
                    node: ends.writer,
                    pin: 0,
                },
                kind: link.kind,
            });
            links.entry(to).or_default().push(Link {
                from: PinRef {
                    node: ends.reader,
                    pin: 0,
                },
                to: link.to,
                kind: link.kind,
            });
            out.crossings.push(crossing(link, from, to, &ends));
        }

        let mut gone: Vec<(Link, PartitionId, PartitionId)> = self
            .ends
            .keys()
            .filter(|k| !seen.contains(k))
            .copied()
            .collect();
        gone.sort_by_key(|(l, f, t)| (l.from.node, l.from.pin, l.to.node, l.to.pin, *f, *t));
        for key in gone {
            if let Some(e) = self.ends.remove(&key) {
                out.retired.push(crossing(&key.0, key.1, key.2, &e));
            }
        }

        let next_id = logical.next_id();
        let mut partitions: BTreeSet<PartitionId> = nodes.keys().copied().collect();
        for p in &partitions {
            let part_nodes = nodes.remove(p).unwrap_or_default();
            let part_links = links.remove(p).unwrap_or_default();
            out.parts.insert(
                *p,
                GraphDescription::from_parts(part_nodes, part_links, next_id),
            );
        }
        // Partitions that just lost their last node get one empty description.
        for p in self.partitions.difference(&partitions.clone()) {
            out.parts.insert(*p, GraphDescription::new());
        }
        std::mem::swap(&mut self.partitions, &mut partitions);
        out
    }
}

fn crossing(link: &Link, from: PartitionId, to: PartitionId, e: &Ends) -> Crossing {
    Crossing {
        link: *link,
        from,
        to,
        writer: e.writer,
        reader: e.reader,
        stats: Arc::clone(&e.stats),
    }
}
