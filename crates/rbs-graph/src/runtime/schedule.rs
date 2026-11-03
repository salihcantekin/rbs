//! The compiled form of a plan: node slots, value slots and input wiring.
//!
//! Value slots are laid out in schedule order, so every input of a node reads a slot
//! with a lower index than the node's first output. `split_at_mut` then gives the node
//! shared access to its producers' values and exclusive access to its own outputs,
//! without `unsafe` and without locks.

use std::fmt;

use rbs_core::media::{
    AudioBlock, AudioFormat, AudioSamples, Bytes, EncodedPacket, MAX_CHANNELS, VideoFrame,
};
use rbs_core::time::Timestamp;

use crate::description::NodeId;
use crate::node::LinkKind;

use super::Node;
use super::Runtime;
use super::executor::NodeHealth;

/// How many packets or byte chunks one output pin can carry per tick. The buffers are
/// allocated when the schedule is built; a push beyond this is refused with the value.
pub const PER_TICK_EVENTS: usize = 16;

/// The largest audio block an audio output pin can carry, in frames per channel.
pub const MAX_BLOCK_FRAMES: u32 = 8192;

/// The buffer of one audio output pin: planar, `MAX_CHANNELS` channels of `capacity`
/// frames each, allocated when the schedule is built. `frames == 0` means no block this
/// tick.
pub(crate) struct AudioSlot {
    samples: Box<[f32]>,
    capacity: usize,
    format: AudioFormat,
    timestamp: Timestamp,
    frames: usize,
}

impl AudioSlot {
    pub(crate) fn new(capacity: u32) -> Self {
        let capacity = capacity.clamp(1, MAX_BLOCK_FRAMES) as usize;
        Self {
            samples: vec![0.0; capacity * MAX_CHANNELS].into_boxed_slice(),
            capacity,
            format: AudioFormat::STEREO_48K,
            timestamp: Timestamp::ZERO,
            frames: 0,
        }
    }

    fn block(&self) -> Option<AudioBlock<'_>> {
        if self.frames == 0 {
            return None;
        }
        let n = self.format.channels.channels();
        let mut channels: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
        for (c, chunk) in self.samples.chunks_exact(self.capacity).take(n).enumerate() {
            channels[c] = &chunk[..self.frames];
        }
        AudioBlock::planar(self.format, self.timestamp, &channels[..n]).ok()
    }
}

impl fmt::Debug for AudioSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioSlot")
            .field("capacity", &self.capacity)
            .field("format", &self.format)
            .field("timestamp", &self.timestamp)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

/// A half-open range of indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Span {
    pub(crate) start: u32,
    pub(crate) end: u32,
}

impl Span {
    pub(crate) fn len(self) -> usize {
        (self.end - self.start) as usize
    }

    pub(crate) fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

/// The value on one output pin.
///
/// Video is state: the slot keeps the latest frame until the producer replaces or
/// clears it, so a slower source or a failing node shows its last good frame. Packets
/// and bytes are events: the slot is emptied before the producer runs each tick.
#[derive(Debug)]
pub(crate) enum Slot {
    Video(Option<VideoFrame>),
    Packets(Vec<EncodedPacket>),
    Bytes(Vec<Bytes>),
    Audio(AudioSlot),
    /// Data pins (event-driven, outside the tick) and kinds this executor does not
    /// carry; nothing can be written or read.
    Inert,
}

impl Slot {
    pub(crate) fn for_kind(kind: LinkKind, audio_frames: u32) -> Slot {
        match kind {
            LinkKind::Video => Slot::Video(None),
            LinkKind::Audio => Slot::Audio(AudioSlot::new(audio_frames)),
            LinkKind::Packets => Slot::Packets(Vec::with_capacity(PER_TICK_EVENTS)),
            LinkKind::Bytes => Slot::Bytes(Vec::with_capacity(PER_TICK_EVENTS)),
            _ => Slot::Inert,
        }
    }

    /// Empties event slots; keeps video. Never frees the buffers.
    pub(crate) fn begin_tick(&mut self) {
        match self {
            Slot::Packets(v) => v.clear(),
            Slot::Bytes(v) => v.clear(),
            Slot::Audio(a) => a.frames = 0,
            Slot::Video(_) | Slot::Inert => {}
        }
    }
}

/// One scheduled node.
pub(crate) struct Entry<X: Runtime> {
    pub(crate) id: NodeId,
    /// `None` only while the node is carried over from the running schedule (filled
    /// in at the swap) or after it was moved out.
    pub(crate) node: Option<Box<dyn Node<X>>>,
    /// Output slots, one per output pin.
    pub(crate) outputs: Span,
    /// Into `Schedule::pins`: one span of `Schedule::links` per input pin.
    pub(crate) pins: Span,
    pub(crate) health: NodeHealth,
    /// `warm` succeeded and `cool` has not run since; only such nodes are cooled.
    pub(crate) warmed: bool,
    /// The instance's epoch (bumped by the controller on each `prepare`), or `None`
    /// for a node held in the plan without an instance while it waits for a restart.
    pub(crate) epoch: Option<u64>,
}

/// A compiled plan, sent from the controller to the executor and back.
pub(crate) struct Schedule<X: Runtime> {
    pub(crate) nodes: Vec<Entry<X>>,
    pub(crate) slots: Vec<Slot>,
    /// For each input pin of each node, a span of `links`.
    pub(crate) pins: Vec<Span>,
    /// Producer slot index per incoming link, grouped by input pin.
    pub(crate) links: Vec<u32>,
    /// Producer node per incoming link, parallel to `links`.
    pub(crate) link_sources: Vec<NodeId>,
    /// `(old node index, new node index)` for nodes that stay across the swap.
    pub(crate) carry_nodes: Vec<(u32, u32)>,
    /// `(old slot, new slot)` for video slots of nodes that stay, so their last frame
    /// survives the swap.
    pub(crate) carry_slots: Vec<(u32, u32)>,
    /// Node indices that join with this schedule and must be warmed.
    pub(crate) fresh: Vec<u32>,
}

impl<X: Runtime> Schedule<X> {
    pub(crate) fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            slots: Vec::new(),
            pins: Vec::new(),
            links: Vec::new(),
            link_sources: Vec::new(),
            carry_nodes: Vec::new(),
            carry_slots: Vec::new(),
            fresh: Vec::new(),
        }
    }
}

/// Why writing an output failed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputError {
    /// The node has no output pin with this index.
    NoSuchPin(usize),
    /// The pin carries another kind of value.
    WrongKind {
        /// The pin.
        pin: usize,
        /// What the pin carries.
        expected: LinkKind,
    },
    /// The pin already holds [`PER_TICK_EVENTS`] values this tick.
    Full(usize),
    /// An audio block does not fit the pin's buffer: more frames than its capacity, or
    /// more than `MAX_CHANNELS` channels.
    AudioTooLarge {
        /// The pin.
        pin: usize,
        /// Frames per channel the pin holds.
        capacity: usize,
    },
    /// An audio block's samples are in a layout this version cannot copy.
    UnsupportedSamples(usize),
}

impl fmt::Display for OutputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutputError::NoSuchPin(p) => write!(f, "no output pin {p}"),
            OutputError::WrongKind { pin, expected } => {
                write!(f, "output pin {pin} carries {expected}")
            }
            OutputError::Full(p) => write!(f, "output pin {p} is full this tick"),
            OutputError::AudioTooLarge { pin, capacity } => {
                write!(
                    f,
                    "audio block too large for output pin {pin} ({capacity} frames)"
                )
            }
            OutputError::UnsupportedSamples(p) => {
                write!(f, "unsupported audio sample layout for output pin {p}")
            }
        }
    }
}

impl std::error::Error for OutputError {}

/// A value that could not be written, handed back so the caller decides where it is
/// dropped (dropping the last reference to a buffer frees it).
#[derive(Debug)]
pub struct Rejected<T> {
    /// Why.
    pub error: OutputError,
    /// The value.
    pub value: T,
}

/// The values on a node's inputs for this tick. Cheap to copy.
///
/// On an input pin with several links (compositor layers, mixer inputs) the links are
/// in the order they were connected in the graph description.
#[derive(Clone, Copy)]
pub struct Inputs<'a> {
    pub(crate) slots: &'a [Slot],
    pub(crate) pins: &'a [Span],
    pub(crate) links: &'a [u32],
    pub(crate) sources: &'a [NodeId],
}

impl<'a> Inputs<'a> {
    fn link_slots(&self, pin: usize) -> &'a [u32] {
        match self.pins.get(pin) {
            Some(span) => &self.links[span.range()],
            None => &[],
        }
    }

    fn slot(&self, index: u32) -> &'a Slot {
        &self.slots[index as usize]
    }

    /// Number of input pins.
    pub fn pin_count(&self) -> usize {
        self.pins.len()
    }

    /// The node that produces link `link` of input `pin`.
    pub fn source(&self, pin: usize, link: usize) -> Option<NodeId> {
        let span = self.pins.get(pin)?;
        self.sources[span.range()].get(link).copied()
    }

    /// The index of the link into `pin` whose producer is `node`, if it is scheduled.
    /// Linear in the number of links; nodes that look a layer up every tick check
    /// [`Inputs::source`] at a remembered index first.
    pub fn find(&self, pin: usize, node: NodeId) -> Option<usize> {
        let span = self.pins.get(pin)?;
        self.sources[span.range()].iter().position(|&n| n == node)
    }

    /// Number of scheduled links into `pin` (0 for unknown pins).
    pub fn link_count(&self, pin: usize) -> usize {
        self.link_slots(pin).len()
    }

    /// The current frame on link `link` of video input `pin`, if any.
    pub fn video(&self, pin: usize, link: usize) -> Option<&'a VideoFrame> {
        let index = *self.link_slots(pin).get(link)?;
        match self.slot(index) {
            Slot::Video(frame) => frame.as_ref(),
            _ => None,
        }
    }

    /// The current frames on every link of video input `pin`, in link order; `None`
    /// where the producer has no frame yet.
    pub fn videos(&self, pin: usize) -> impl Iterator<Item = Option<&'a VideoFrame>> + 'a {
        let this = *self;
        this.link_slots(pin)
            .iter()
            .map(move |&index| match this.slot(index) {
                Slot::Video(frame) => frame.as_ref(),
                _ => None,
            })
    }

    /// The packets produced this tick on every link of packet input `pin`.
    pub fn packets(&self, pin: usize) -> impl Iterator<Item = &'a EncodedPacket> + 'a {
        let this = *self;
        this.link_slots(pin)
            .iter()
            .flat_map(move |&index| match this.slot(index) {
                Slot::Packets(v) => v.as_slice(),
                _ => &[],
            })
    }

    /// The audio block committed this tick on link `link` of audio input `pin`, planar.
    /// `None` when the producer committed none (consumers treat that as silence).
    pub fn audio(&self, pin: usize, link: usize) -> Option<AudioBlock<'a>> {
        let index = *self.link_slots(pin).get(link)?;
        match self.slot(index) {
            Slot::Audio(a) => a.block(),
            _ => None,
        }
    }

    /// The audio blocks on every link of audio input `pin`, in link order; `None` where
    /// the producer committed none this tick.
    pub fn audios(&self, pin: usize) -> impl Iterator<Item = Option<AudioBlock<'a>>> + 'a {
        let this = *self;
        this.link_slots(pin)
            .iter()
            .map(move |&index| match this.slot(index) {
                Slot::Audio(a) => a.block(),
                _ => None,
            })
    }

    /// The byte chunks produced this tick on every link of bytes input `pin`.
    pub fn bytes(&self, pin: usize) -> impl Iterator<Item = &'a Bytes> + 'a {
        let this = *self;
        this.link_slots(pin)
            .iter()
            .flat_map(move |&index| match this.slot(index) {
                Slot::Bytes(v) => v.as_slice(),
                _ => &[],
            })
    }
}

/// A node's outputs for this tick.
pub struct Outputs<'a> {
    pub(crate) slots: &'a mut [Slot],
}

impl Outputs<'_> {
    fn slot(&mut self, pin: usize) -> Result<&mut Slot, OutputError> {
        self.slots.get_mut(pin).ok_or(OutputError::NoSuchPin(pin))
    }

    /// Number of output pins.
    pub fn pin_count(&self) -> usize {
        self.slots.len()
    }

    /// Replaces the frame on video output `pin` (`None` clears it) and returns the
    /// previous one. The frame stays until replaced, so a node only needs to call this
    /// when it has a new frame. On error the frame is dropped (that only releases its
    /// lease; pool memory is never freed by a drop).
    pub fn set_video(
        &mut self,
        pin: usize,
        frame: Option<VideoFrame>,
    ) -> Result<Option<VideoFrame>, OutputError> {
        match self.slot(pin)? {
            Slot::Video(current) => Ok(std::mem::replace(current, frame)),
            _ => Err(OutputError::WrongKind {
                pin,
                expected: LinkKind::Video,
            }),
        }
    }

    /// The frame currently on video output `pin`.
    pub fn video(&self, pin: usize) -> Option<&VideoFrame> {
        match self.slots.get(pin) {
            Some(Slot::Video(frame)) => frame.as_ref(),
            _ => None,
        }
    }

    /// The buffer of audio output `pin`, to write this tick's block into.
    pub fn audio(&mut self, pin: usize) -> Result<AudioOut<'_>, OutputError> {
        match self.slot(pin)? {
            Slot::Audio(slot) => Ok(AudioOut { pin, slot }),
            _ => Err(OutputError::WrongKind {
                pin,
                expected: LinkKind::Audio,
            }),
        }
    }

    /// Adds a packet to packet output `pin` for this tick.
    pub fn push_packet(
        &mut self,
        pin: usize,
        packet: EncodedPacket,
    ) -> Result<(), Rejected<EncodedPacket>> {
        let slot = match self.slot(pin) {
            Ok(slot) => slot,
            Err(error) => {
                return Err(Rejected {
                    error,
                    value: packet,
                });
            }
        };
        match slot {
            Slot::Packets(v) if v.len() < v.capacity() => {
                v.push(packet);
                Ok(())
            }
            Slot::Packets(_) => Err(Rejected {
                error: OutputError::Full(pin),
                value: packet,
            }),
            _ => Err(Rejected {
                error: OutputError::WrongKind {
                    pin,
                    expected: LinkKind::Packets,
                },
                value: packet,
            }),
        }
    }

    /// Adds a byte chunk to bytes output `pin` for this tick.
    pub fn push_bytes(&mut self, pin: usize, chunk: Bytes) -> Result<(), Rejected<Bytes>> {
        let slot = match self.slot(pin) {
            Ok(slot) => slot,
            Err(error) => {
                return Err(Rejected {
                    error,
                    value: chunk,
                });
            }
        };
        match slot {
            Slot::Bytes(v) if v.len() < v.capacity() => {
                v.push(chunk);
                Ok(())
            }
            Slot::Bytes(_) => Err(Rejected {
                error: OutputError::Full(pin),
                value: chunk,
            }),
            _ => Err(Rejected {
                error: OutputError::WrongKind {
                    pin,
                    expected: LinkKind::Bytes,
                },
                value: chunk,
            }),
        }
    }
}

/// The buffer of one audio output pin for this tick (from [`Outputs::audio`]).
///
/// Write samples into the channels, then [`commit`](AudioOut::commit) the block's
/// format, timestamp and length. Nothing is published without a commit; a later commit
/// in the same tick replaces the earlier one.
pub struct AudioOut<'s> {
    pin: usize,
    slot: &'s mut AudioSlot,
}

impl AudioOut<'_> {
    /// Frames per channel the buffer holds.
    pub fn capacity(&self) -> usize {
        self.slot.capacity
    }

    /// Channel `channel` (`< MAX_CHANNELS`), `capacity()` frames long, or `None`.
    pub fn channel_mut(&mut self, channel: usize) -> Option<&mut [f32]> {
        if channel >= MAX_CHANNELS {
            return None;
        }
        let cap = self.slot.capacity;
        self.slot
            .samples
            .get_mut(channel * cap..(channel + 1) * cap)
    }

    /// The first `channels` channels (at most `MAX_CHANNELS`), each `capacity()` frames
    /// long, in order.
    pub fn channels_mut(&mut self, channels: usize) -> impl Iterator<Item = &mut [f32]> {
        let cap = self.slot.capacity;
        self.slot
            .samples
            .chunks_exact_mut(cap)
            .take(channels.min(MAX_CHANNELS))
    }

    /// Publishes the first `frames` frames of each of `format`'s channels as this tick's
    /// block at `timestamp`. `frames == 0` publishes nothing.
    pub fn commit(
        &mut self,
        format: AudioFormat,
        timestamp: Timestamp,
        frames: usize,
    ) -> Result<(), OutputError> {
        if frames > self.slot.capacity || format.channels.channels() > MAX_CHANNELS {
            return Err(OutputError::AudioTooLarge {
                pin: self.pin,
                capacity: self.slot.capacity,
            });
        }
        self.slot.format = format;
        self.slot.timestamp = timestamp;
        self.slot.frames = frames;
        Ok(())
    }

    /// Copies `block` (planar or interleaved) into the buffer and commits it.
    pub fn write(&mut self, block: &AudioBlock<'_>) -> Result<(), OutputError> {
        let frames = block.frames();
        let format = block.format();
        let n = format.channels.channels();
        if frames > self.slot.capacity || n > MAX_CHANNELS {
            return Err(OutputError::AudioTooLarge {
                pin: self.pin,
                capacity: self.slot.capacity,
            });
        }
        let cap = self.slot.capacity;
        match block.samples() {
            AudioSamples::Planar(planar) => {
                for (c, src) in planar.iter().enumerate() {
                    self.slot.samples[c * cap..c * cap + frames].copy_from_slice(src);
                }
            }
            AudioSamples::Interleaved(samples) => {
                for (c, dst) in self.slot.samples.chunks_exact_mut(cap).take(n).enumerate() {
                    for (f, d) in dst[..frames].iter_mut().enumerate() {
                        *d = samples[f * n + c];
                    }
                }
            }
            _ => return Err(OutputError::UnsupportedSamples(self.pin)),
        }
        self.commit(format, block.timestamp(), frames)
    }
}
