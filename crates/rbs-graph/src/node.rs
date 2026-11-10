//! Node kinds, pins and descriptors.

use std::borrow::Cow;
use std::fmt;

/// What a node does, which fixes the shape of its pins (media graph spec, "Node kinds").
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// Produces media or data; no media inputs.
    Source,
    /// Video in, video out.
    VideoFilter,
    /// Audio in, audio out.
    AudioFilter,
    /// Many video inputs, one video output: a canvas, or a nested scene's render target.
    Compositor,
    /// Two or more video inputs (stinger: a third), one video output; audio optional.
    Transition,
    /// Audio inputs, audio outputs (buses, mixes).
    AudioMixer,
    /// One video or audio input, packets out.
    Encoder,
    /// Packets in, packets or bytes out.
    Muxer,
    /// A sink: packets, bytes, video or audio in; no media outputs (stream, recording,
    /// virtual camera, NDI or SDI out, monitoring device, preview).
    Output,
}

/// The type of data a link carries.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkKind {
    /// `rbs_core::media::VideoFrame`s.
    Video,
    /// `rbs_core::media::AudioBlock`s.
    Audio,
    /// `rbs_core::media::EncodedPacket`s.
    Packets,
    /// Muxed byte stream (container output).
    Bytes,
    /// Events and metadata.
    Data,
}

impl LinkKind {
    /// True for every kind except `Data`.
    pub const fn is_media(self) -> bool {
        !matches!(self, LinkKind::Data)
    }
}

impl fmt::Display for LinkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LinkKind::Video => "video",
            LinkKind::Audio => "audio",
            LinkKind::Packets => "packets",
            LinkKind::Bytes => "bytes",
            LinkKind::Data => "data",
        })
    }
}

/// One input or output of a node. Pin names are the stable identity of a pin; indices
/// may change between versions of a node type.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    /// Name, unique among the node's inputs (or outputs), for example `"in"` or `"layer"`.
    pub name: Cow<'static, str>,
    /// Data type.
    pub kind: LinkKind,
    /// For inputs: whether more than one link may connect (compositor layers, mixer
    /// inputs). Outputs may always feed any number of links.
    pub many: bool,
}

impl Pin {
    /// A pin that takes at most one link (when an input).
    pub const fn one(name: &'static str, kind: LinkKind) -> Self {
        Self {
            name: Cow::Borrowed(name),
            kind,
            many: false,
        }
    }

    /// An input pin that takes any number of links.
    pub const fn many(name: &'static str, kind: LinkKind) -> Self {
        Self {
            name: Cow::Borrowed(name),
            kind,
            many: true,
        }
    }

    /// A pin with an owned name (for example from a plugin manifest).
    pub fn named(name: impl Into<String>, kind: LinkKind, many: bool) -> Self {
        Self {
            name: Cow::Owned(name.into()),
            kind,
            many,
        }
    }
}

/// A stable, reverse-DNS style id of a node type, for example `org.rbs.filter.chroma-key`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeTypeId(Cow<'static, str>);

impl NodeTypeId {
    /// Creates an id from a static string.
    pub const fn new_static(id: &'static str) -> Self {
        Self(Cow::Borrowed(id))
    }

    /// Creates an id from an owned string (for example from a plugin manifest).
    pub fn new(id: impl Into<String>) -> Self {
        Self(Cow::Owned(id.into()))
    }

    /// The id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True for at least two non-empty dot-separated segments of lowercase ASCII
    /// letters, digits and `-`.
    pub fn is_well_formed(&self) -> bool {
        let mut segments = 0;
        for segment in self.0.split('.') {
            if segment.is_empty()
                || !segment
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return false;
            }
            segments += 1;
        }
        segments >= 2
    }
}

impl fmt::Display for NodeTypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Static description of a node type: its kind and pins. Settings schemas are added
/// with the plugin SDK; build with [`NodeDescriptor::new`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeDescriptor {
    /// Node type id.
    pub type_id: NodeTypeId,
    /// Kind.
    pub kind: NodeKind,
    /// Input pins, in order.
    pub inputs: Vec<Pin>,
    /// Output pins, in order.
    pub outputs: Vec<Pin>,
}

/// Why a descriptor is invalid.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptorError {
    /// The type id is not reverse-DNS style.
    BadTypeId(NodeTypeId),
    /// Two inputs or two outputs share a name.
    DuplicatePin(Cow<'static, str>),
    /// The pins do not match the shape required by the kind.
    ShapeMismatch {
        /// The kind.
        kind: NodeKind,
        /// What the kind requires.
        requires: &'static str,
    },
}

impl fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DescriptorError::BadTypeId(id) => write!(f, "node type id {id:?} is not reverse-DNS"),
            DescriptorError::DuplicatePin(name) => write!(f, "duplicate pin name {name:?}"),
            DescriptorError::ShapeMismatch { kind, requires } => {
                write!(f, "{kind:?} requires {requires}")
            }
        }
    }
}

impl std::error::Error for DescriptorError {}

impl NodeDescriptor {
    /// Creates a descriptor. Call [`NodeDescriptor::validate`] (or add it to a graph,
    /// which validates) before use.
    pub fn new(type_id: NodeTypeId, kind: NodeKind, inputs: Vec<Pin>, outputs: Vec<Pin>) -> Self {
        Self {
            type_id,
            kind,
            inputs,
            outputs,
        }
    }

    /// Checks the type id, that pin names are unique, and that the media pins fit the
    /// kind (media graph spec, "Node kinds"). `Data` pins are allowed on every kind.
    pub fn validate(&self) -> Result<(), DescriptorError> {
        if !self.type_id.is_well_formed() {
            return Err(DescriptorError::BadTypeId(self.type_id.clone()));
        }
        for pins in [&self.inputs, &self.outputs] {
            for (i, pin) in pins.iter().enumerate() {
                if pins[..i].iter().any(|p| p.name == pin.name) {
                    return Err(DescriptorError::DuplicatePin(pin.name.clone()));
                }
            }
        }
        let count = |pins: &[Pin], k: LinkKind| pins.iter().filter(|p| p.kind == k).count();
        let media = |pins: &[Pin]| pins.iter().filter(|p| p.kind.is_media()).count();
        let ins = |k| count(&self.inputs, k);
        let outs = |k| count(&self.outputs, k);
        let (media_in, media_out) = (media(&self.inputs), media(&self.outputs));
        use LinkKind::{Audio, Bytes, Packets, Video};

        let (ok, requires) = match self.kind {
            NodeKind::Source => (
                media_in == 0 && !self.outputs.is_empty(),
                "no media inputs and at least one output",
            ),
            NodeKind::VideoFilter => (
                media_in == 1 && ins(Video) == 1 && media_out == 1 && outs(Video) == 1,
                "one video input and one video output",
            ),
            // A second audio input named `key` carries another source's signal (ADR-0095).
            NodeKind::AudioFilter => (
                media_in == ins(Audio)
                    && match ins(Audio) {
                        1 => true,
                        2 => self
                            .inputs
                            .iter()
                            .any(|p| p.kind == Audio && p.name == "key"),
                        _ => false,
                    }
                    && media_out == 1
                    && outs(Audio) == 1,
                "one audio input and one audio output, and optionally a second audio input named key",
            ),
            NodeKind::Compositor => (
                media_in == ins(Video)
                    && self.inputs.iter().any(|p| p.kind == Video && p.many)
                    && media_out == 1
                    && outs(Video) == 1,
                "a many-link video input and one video output",
            ),
            NodeKind::Transition => (
                ins(Video) >= 2
                    && media_in == ins(Video) + ins(Audio)
                    && outs(Video) == 1
                    && media_out == 1 + outs(Audio)
                    && outs(Audio) <= 1,
                "two or more video inputs, one video output, optional audio",
            ),
            NodeKind::AudioMixer => (
                media_in == ins(Audio) && media_in > 0 && media_out == outs(Audio) && media_out > 0,
                "audio inputs and audio outputs only",
            ),
            NodeKind::Encoder => (
                media_in == 1
                    && ins(Video) + ins(Audio) == 1
                    && media_out == 1
                    && outs(Packets) == 1,
                "one video or audio input and one packet output",
            ),
            NodeKind::Muxer => (
                media_in == ins(Packets)
                    && media_in > 0
                    && media_out == 1
                    && outs(Packets) + outs(Bytes) == 1,
                "packet inputs and one packet or byte output",
            ),
            NodeKind::Output => (
                media_in > 0 && media_out == 0,
                "media inputs and no media outputs",
            ),
        };
        if ok {
            Ok(())
        } else {
            Err(DescriptorError::ShapeMismatch {
                kind: self.kind,
                requires,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use LinkKind::*;

    fn desc(kind: NodeKind, inputs: Vec<Pin>, outputs: Vec<Pin>) -> NodeDescriptor {
        NodeDescriptor::new(NodeTypeId::new_static("test.node"), kind, inputs, outputs)
    }

    #[test]
    fn valid_shapes() {
        let cases = [
            desc(
                NodeKind::Source,
                vec![],
                vec![Pin::one("video", Video), Pin::one("audio", Audio)],
            ),
            desc(
                NodeKind::Source,
                vec![Pin::one("cmd", Data)],
                vec![Pin::one("events", Data)],
            ),
            desc(
                NodeKind::VideoFilter,
                vec![Pin::one("in", Video)],
                vec![Pin::one("out", Video)],
            ),
            desc(
                NodeKind::AudioFilter,
                vec![Pin::one("in", Audio)],
                vec![Pin::one("out", Audio)],
            ),
            desc(
                NodeKind::Compositor,
                vec![Pin::many("layers", Video)],
                vec![Pin::one("out", Video)],
            ),
            desc(
                NodeKind::Transition,
                vec![
                    Pin::one("a", Video),
                    Pin::one("b", Video),
                    Pin::one("stinger", Video),
                    Pin::one("a-audio", Audio),
                ],
                vec![Pin::one("out", Video), Pin::one("audio", Audio)],
            ),
            desc(
                NodeKind::AudioMixer,
                vec![Pin::many("in", Audio)],
                vec![Pin::one("main", Audio), Pin::one("monitor", Audio)],
            ),
            desc(
                NodeKind::Encoder,
                vec![Pin::one("in", Audio)],
                vec![Pin::one("out", Packets)],
            ),
            desc(
                NodeKind::Muxer,
                vec![Pin::one("video", Packets), Pin::one("audio", Packets)],
                vec![Pin::one("out", Bytes)],
            ),
            desc(
                NodeKind::Output,
                vec![Pin::many("in", Packets)],
                vec![Pin::one("events", Data)],
            ),
            desc(
                NodeKind::Output,
                vec![Pin::one("video", Video), Pin::one("audio", Audio)],
                vec![],
            ),
        ];
        for d in cases {
            assert_eq!(d.validate(), Ok(()), "{:?}", d.kind);
        }
    }

    #[test]
    fn invalid_shapes() {
        let invalid = [
            desc(
                NodeKind::Source,
                vec![Pin::one("in", Video)],
                vec![Pin::one("out", Video)],
            ),
            desc(
                NodeKind::Compositor,
                vec![Pin::one("in", Video)],
                vec![Pin::one("out", Video)],
            ),
            desc(
                NodeKind::Encoder,
                vec![Pin::one("in", Packets)],
                vec![Pin::one("out", Packets)],
            ),
            desc(
                NodeKind::Transition,
                vec![Pin::one("a", Video)],
                vec![Pin::one("out", Video)],
            ),
            desc(
                NodeKind::Muxer,
                vec![Pin::one("in", Video)],
                vec![Pin::one("out", Packets)],
            ),
            desc(
                NodeKind::Muxer,
                vec![Pin::one("in", Packets)],
                vec![Pin::one("a", Packets), Pin::one("b", Bytes)],
            ),
            desc(
                NodeKind::Output,
                vec![Pin::one("in", Packets)],
                vec![Pin::one("out", Packets)],
            ),
            desc(NodeKind::Output, vec![Pin::one("in", Data)], vec![]),
        ];
        for d in invalid {
            assert!(
                matches!(d.validate(), Err(DescriptorError::ShapeMismatch { .. })),
                "{:?} {:?}",
                d.kind,
                d.inputs
            );
        }
        let duplicate = desc(
            NodeKind::Source,
            vec![],
            vec![Pin::one("out", Video), Pin::one("out", Audio)],
        );
        assert_eq!(
            duplicate.validate(),
            Err(DescriptorError::DuplicatePin("out".into()))
        );
    }

    #[test]
    fn type_ids_must_be_reverse_dns() {
        for good in ["org.rbs.filter.chroma-key", "com.example.x2"] {
            assert!(NodeTypeId::new(good).is_well_formed(), "{good}");
        }
        for bad in ["", "single", "org..rbs", "Org.rbs", "org.rbs.", "org.r bs"] {
            assert!(!NodeTypeId::new(bad).is_well_formed(), "{bad}");
        }
        let d = NodeDescriptor::new(
            NodeTypeId::new("nope"),
            NodeKind::Source,
            vec![],
            vec![Pin::one("v", Video)],
        );
        assert!(matches!(d.validate(), Err(DescriptorError::BadTypeId(_))));
    }
}
