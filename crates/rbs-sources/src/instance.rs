//! The node instances [`SourceTypes::instantiate`](crate::SourceTypes::instantiate)
//! makes for a bound source, one variant per source class (`scene-sources.md`).

use std::fmt;
use std::sync::Arc;

use rbs_audio::{InputNode, InputParams, InputStats};
use rbs_audio_io::EndpointId;
use rbs_capture::TargetId;
use rbs_core::registry::AdapterId;
use rbs_graph::runtime::param::ParamSender;
use rbs_nodes::capture::CaptureNodeStats;
use rbs_nodes::{CaptureSourceNode, StillSourceNode};

/// A capture source node for the render partition (`Node<GpuRuntime>`).
#[non_exhaustive]
pub struct VideoSource {
    /// The node; it opens the session in `prepare`.
    pub node: CaptureSourceNode,
    /// Its counters.
    pub stats: Arc<CaptureNodeStats>,
    /// The adapter it captures with.
    pub adapter: AdapterId,
    /// The target it captures (the default one if the settings named none).
    pub target: TargetId,
}

/// An audio input node for the audio partition (`Node<X>` for any runtime).
#[non_exhaustive]
pub struct AudioSource {
    /// The node, holding an opened stream in Standby (no device activity until
    /// `prepare`, audio input port contract).
    pub node: InputNode,
    /// The sync offset channel (ADR-0023).
    pub offset: ParamSender<InputParams>,
    /// Its counters.
    pub stats: Arc<InputStats>,
    /// The adapter it reads with.
    pub adapter: AdapterId,
    /// The endpoint it reads (the default one if the settings named none).
    pub endpoint: EndpointId,
}

/// A still source node for the render partition (ADR-0074).
#[non_exhaustive]
pub struct StillSource {
    /// The node; it uploads its picture in `warm` and costs nothing per frame.
    pub node: StillSourceNode,
    /// Its counters (`picture_size` is known from the start).
    pub stats: Arc<CaptureNodeStats>,
}

/// A media-file source (ADR-0108 decision 1): a picture node and a sound node fed by
/// one decode worker.
#[non_exhaustive]
pub struct MediaSource {
    /// The picture node: adopt into the render partition. Standby holds no decoder
    /// and no rings.
    pub picture: rbs_nodes::MediaPictureNode,
    /// The sound node: adopt into the audio partition at the binder's sound node
    /// (`BindReport::picture_sound`). Drop it when the binder has no audio partition;
    /// the worker then decodes no sound.
    pub sound: rbs_nodes::MediaSoundNode,
    /// Play, pause, seek, and restart (ADR-0108 decision 10). Control path only.
    pub control: rbs_nodes::MediaControl,
    /// Upload, drop, repeat, and underrun counters.
    pub stats: Arc<rbs_nodes::MediaStats>,
    /// The decoder factory chosen for this node.
    pub adapter: AdapterId,
}

/// A browser source (ADR-0109): a picture node and a sound node fed by one helper. No
/// helper until the picture node is activated with a slot.
#[non_exhaustive]
pub struct BrowserSource {
    /// The picture node: adopt into the render partition.
    pub node: crate::browser::BrowserSourceNode,
    /// The sound node: adopt into the audio partition at the binder's sound node
    /// (`BindReport::picture_sound`). Drop it when the binder has no audio partition;
    /// the page's sound is then dropped by the feed.
    pub sound: crate::BrowserSoundNode,
    /// The sound's counters.
    pub sound_stats: Arc<crate::BrowserSoundStats>,
}

/// A text source node (ADR-0129). It shows nothing until the worker sends a picture.
#[non_exhaustive]
pub struct TextSource {
    /// The node.
    pub node: rbs_nodes::TextSourceNode,
    /// The node's picture channel; the text worker keeps it.
    pub frames: ParamSender<Option<rbs_nodes::TextFrame>>,
    /// Its counters.
    pub stats: Arc<rbs_nodes::TextNodeStats>,
    /// Its settings, read leniently.
    pub settings: crate::text::TextSettings,
}

/// A node instance for a bound source.
#[non_exhaustive]
pub enum SourceInstance {
    /// Adopt into the render partition.
    Still(StillSource),
    /// Adopt into the render partition.
    Video(VideoSource),
    /// The picture node into the render partition, the sound node into the audio
    /// partition (ADR-0106). Boxed: far larger than the other variants.
    Media(Box<MediaSource>),
    /// Adopt into the render partition. No audio pin.
    Browser(BrowserSource),
    /// Adopt into the render partition; hand `frames` to the text worker. Boxed: it
    /// carries its settings.
    Text(Box<TextSource>),
    /// Adopt into the render partition; attach `frames` to the slide show hub
    /// (ADR-0171). Boxed like text.
    Slideshow(Box<crate::slideshow::SlideshowSource>),
    /// Adopt into the audio partition. Boxed: larger than the picture variants.
    Audio(Box<AudioSource>),
    /// The picture node into the render partition, the sound node into the audio
    /// partition, and the control handle to the host, which starts the worker once a
    /// pairing connects (spec `guest-source.md`). Boxed like media.
    Guest(Box<crate::guest::GuestSource>),
}

impl fmt::Debug for SourceInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceInstance::Still(s) => f
                .debug_struct("Still")
                .field("size", &s.node.size())
                .finish_non_exhaustive(),
            SourceInstance::Video(v) => f
                .debug_struct("Video")
                .field("adapter", &v.adapter)
                .field("target", &v.target)
                .finish_non_exhaustive(),
            SourceInstance::Media(m) => f
                .debug_struct("Media")
                .field("adapter", &m.adapter)
                .finish_non_exhaustive(),
            SourceInstance::Guest(g) => f
                .debug_struct("Guest")
                .field("running", &g.control.running())
                .finish_non_exhaustive(),
            SourceInstance::Browser(b) => f
                .debug_struct("Browser")
                .field("running", &b.node.helper_running())
                .finish_non_exhaustive(),
            SourceInstance::Text(t) => f
                .debug_struct("Text")
                .field("kind", &t.settings.kind)
                .field("box", &t.settings.block.box_size)
                .finish_non_exhaustive(),
            SourceInstance::Slideshow(s) => f
                .debug_struct("Slideshow")
                .field("size", &s.size)
                .finish_non_exhaustive(),
            SourceInstance::Audio(a) => f
                .debug_struct("Audio")
                .field("adapter", &a.adapter)
                .field("endpoint", &a.endpoint)
                .finish_non_exhaustive(),
        }
    }
}
