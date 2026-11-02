//! Node kinds, pins and descriptors.

use std::borrow::Cow;
use std::fmt;

/// What a node does, which fixes the shape of its pins (media graph spec, "Node kinds").
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// Produces video and/or audio; no inputs.
    Source,
    /// Video in, video out.
    VideoFilter,
    /// Audio in, audio out.
    AudioFilter,
    /// Many video inputs, one video output; one per canvas.
    Compositor,
    /// Two video inputs, one video output.
    Transition,
    /// Many audio inputs, audio outputs (buses, mixes).
    AudioMixer,
    /// Video or audio in, packets out.
    Encoder,
    /// Packets in, packets out.
    Muxer,
    /// Packets in; no outputs.
    Output,
}

/// The type of data a link carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkKind {
    /// `rbs_core::media::VideoFrame`s.
    Video,
    /// `rbs_core::media::AudioBlock`s.
    Audio,
    /// `rbs_core::media::EncodedPacket`s.
    Packets,
    /// Events and metadata.
    Data,
}

impl fmt::Display for LinkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LinkKind::Video => "video",
            LinkKind::Audio => "audio",
            LinkKind::Packets => "packets",
            LinkKind::Data => "data",
        })
    }
}

/// One input or output of a node.
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
}

impl fmt::Display for NodeTypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Static description of a node type: its kind and pins. Settings schemas are added
/// with the plugin SDK.
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

/// Why a descriptor's pins do not fit its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptorError {
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
            DescriptorError::DuplicatePin(name) => write!(f, "duplicate pin name {name:?}"),
            DescriptorError::ShapeMismatch { kind, requires } => {
                write!(f, "{kind:?} requires {requires}")
            }
        }
    }
}

impl std::error::Error for DescriptorError {}

impl NodeDescriptor {
    /// Checks pin names are unique and the pins fit the kind (media graph spec table).
    pub fn validate(&self) -> Result<(), DescriptorError> {
        for pins in [&self.inputs, &self.outputs] {
            for (i, pin) in pins.iter().enumerate() {
                if pins[..i].iter().any(|p| p.name == pin.name) {
                    return Err(DescriptorError::DuplicatePin(pin.name.clone()));
                }
            }
        }
        let ins = |k: LinkKind| self.inputs.iter().filter(|p| p.kind == k).count();
        let outs = |k: LinkKind| self.outputs.iter().filter(|p| p.kind == k).count();
        let media_in = ins(LinkKind::Video) + ins(LinkKind::Audio) + ins(LinkKind::Packets);
        let media_out = outs(LinkKind::Video) + outs(LinkKind::Audio) + outs(LinkKind::Packets);
        let (ok, requires) = match self.kind {
            NodeKind::Source => (
                media_in == 0 && outs(LinkKind::Video) + outs(LinkKind::Audio) > 0,
                "no media inputs and at least one video or audio output",
            ),
            NodeKind::VideoFilter => (
                media_in == 1
                    && ins(LinkKind::Video) == 1
                    && media_out == 1
                    && outs(LinkKind::Video) == 1,
                "one video input and one video output",
            ),
            NodeKind::AudioFilter => (
                media_in == 1
                    && ins(LinkKind::Audio) == 1
                    && media_out == 1
                    && outs(LinkKind::Audio) == 1,
                "one audio input and one audio output",
            ),
            NodeKind::Compositor => (
                media_in == ins(LinkKind::Video)
                    && self
                        .inputs
                        .iter()
                        .any(|p| p.kind == LinkKind::Video && p.many)
                    && media_out == 1
                    && outs(LinkKind::Video) == 1,
                "a many-link video input and one video output",
            ),
            NodeKind::Transition => (
                media_in == 2
                    && ins(LinkKind::Video) == 2
                    && media_out == 1
                    && outs(LinkKind::Video) == 1,
                "two video inputs and one video output",
            ),
            NodeKind::AudioMixer => (
                media_in == ins(LinkKind::Audio)
                    && media_in > 0
                    && media_out == outs(LinkKind::Audio)
                    && media_out > 0,
                "audio inputs and audio outputs only",
            ),
            NodeKind::Encoder => (
                media_in == 1
                    && ins(LinkKind::Packets) == 0
                    && media_out == 1
                    && outs(LinkKind::Packets) == 1,
                "one video or audio input and one packet output",
            ),
            NodeKind::Muxer => (
                media_in == ins(LinkKind::Packets)
                    && media_in > 0
                    && media_out == 1
                    && outs(LinkKind::Packets) == 1,
                "packet inputs and one packet output",
            ),
            NodeKind::Output => (
                media_in == ins(LinkKind::Packets) && media_in > 0 && media_out == 0,
                "packet inputs and no media outputs",
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

    fn desc(kind: NodeKind, inputs: Vec<Pin>, outputs: Vec<Pin>) -> NodeDescriptor {
        NodeDescriptor {
            type_id: NodeTypeId::new_static("test.node"),
            kind,
            inputs,
            outputs,
        }
    }

    #[test]
    fn valid_shapes() {
        use LinkKind::*;
        let cases = [
            desc(
                NodeKind::Source,
                vec![],
                vec![Pin::one("video", Video), Pin::one("audio", Audio)],
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
                vec![Pin::one("a", Video), Pin::one("b", Video)],
                vec![Pin::one("out", Video)],
            ),
            desc(
                NodeKind::AudioMixer,
                vec![Pin::many("in", Audio)],
                vec![Pin::one("main", Audio), Pin::one("monitor", Audio)],
            ),
            desc(
                NodeKind::Encoder,
                vec![Pin::one("in", Video)],
                vec![Pin::one("out", Packets)],
            ),
            desc(
                NodeKind::Muxer,
                vec![Pin::one("video", Packets), Pin::one("audio", Packets)],
                vec![Pin::one("out", Packets)],
            ),
            desc(
                NodeKind::Output,
                vec![Pin::many("in", Packets)],
                vec![Pin::one("events", Data)],
            ),
        ];
        for d in cases {
            assert_eq!(d.validate(), Ok(()), "{:?}", d.kind);
        }
    }

    #[test]
    fn invalid_shapes() {
        use LinkKind::*;
        let source_with_input = desc(
            NodeKind::Source,
            vec![Pin::one("in", Video)],
            vec![Pin::one("out", Video)],
        );
        assert!(matches!(
            source_with_input.validate(),
            Err(DescriptorError::ShapeMismatch { .. })
        ));
        let compositor_single = desc(
            NodeKind::Compositor,
            vec![Pin::one("in", Video)],
            vec![Pin::one("out", Video)],
        );
        assert!(compositor_single.validate().is_err());
        let encoder_from_packets = desc(
            NodeKind::Encoder,
            vec![Pin::one("in", Packets)],
            vec![Pin::one("out", Packets)],
        );
        assert!(encoder_from_packets.validate().is_err());
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
}
