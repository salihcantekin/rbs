//! Events (ADR-0044 decisions 6 and 13). A state event carries the whole current state
//! of its state key, never a delta, so a newer event replaces an older one without
//! loss; every state key has a change counter `seq`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::types::{
    Canvas, CanvasRecordState, CanvasStreamState, RecordState, ReplayStatus, SceneList, Settings,
    SourceInfo, SourceLevels, StreamState, VcamStatus,
};

/// An event topic. Subscriptions and the subscriber mask are per topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Topic {
    /// The program scene's source list (state).
    #[serde(rename = "source.changed")]
    SourceChanged,
    /// The recording state (state).
    #[serde(rename = "record.state")]
    RecordState,
    /// The canvas (state).
    #[serde(rename = "canvas.changed")]
    CanvasChanged,
    /// The settings (state; `admin` is needed to subscribe).
    #[serde(rename = "settings.changed")]
    SettingsChanged,
    /// Audio levels (latest value, sampled at the client's rate).
    #[serde(rename = "audio.levels")]
    AudioLevels,
    /// The stream state (state): started, connected, reconnecting, ended.
    #[serde(rename = "stream.state")]
    StreamState,
    /// The scenes, the one on air and the collection's sources (state, protocol 1.6).
    #[serde(rename = "scene.changed")]
    SceneChanged,
    /// The virtual camera, without its counters (state, protocol 1.12).
    #[serde(rename = "vcam.changed")]
    VcamChanged,
    /// The replay buffer: armed, what it holds, the last clip (state, protocol 1.27).
    #[serde(rename = "replay.changed")]
    ReplayChanged,
    /// The recording state of a canvas other than the main one, one key per canvas
    /// (state, protocol 1.29).
    #[serde(rename = "record.canvas_state")]
    RecordCanvasState,
    /// The stream state of a canvas other than the main one, one key per canvas
    /// (state, protocol 1.29).
    #[serde(rename = "stream.canvas_state")]
    StreamCanvasState,
}

impl Topic {
    /// Every topic.
    pub const ALL: [Topic; 11] = [
        Topic::SourceChanged,
        Topic::RecordState,
        Topic::CanvasChanged,
        Topic::SettingsChanged,
        Topic::AudioLevels,
        Topic::StreamState,
        Topic::SceneChanged,
        Topic::VcamChanged,
        Topic::ReplayChanged,
        Topic::RecordCanvasState,
        Topic::StreamCanvasState,
    ];

    /// Its name on the wire.
    pub const fn name(self) -> &'static str {
        match self {
            Topic::SourceChanged => "source.changed",
            Topic::RecordState => "record.state",
            Topic::CanvasChanged => "canvas.changed",
            Topic::SettingsChanged => "settings.changed",
            Topic::AudioLevels => "audio.levels",
            Topic::StreamState => "stream.state",
            Topic::SceneChanged => "scene.changed",
            Topic::VcamChanged => "vcam.changed",
            Topic::ReplayChanged => "replay.changed",
            Topic::RecordCanvasState => "record.canvas_state",
            Topic::StreamCanvasState => "stream.canvas_state",
        }
    }

    /// True for latest-value topics, which the API thread samples at the client's rate
    /// instead of the control thread sending them on change.
    pub const fn is_sampled(self) -> bool {
        matches!(self, Topic::AudioLevels)
    }

    /// The highest sampling rate a client may ask for, in Hz (sampled topics only).
    pub const fn max_rate_hz(self) -> u32 {
        match self {
            Topic::AudioLevels => 30,
            _ => 0,
        }
    }

    const fn bit(self) -> u32 {
        1 << self as u32
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A set of topics: a session's subscriptions, or the subscriber mask the control
/// thread reads to decide which payloads to build.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TopicSet(u32);

impl TopicSet {
    /// No topic.
    pub const EMPTY: TopicSet = TopicSet(0);

    /// The set of `topics`.
    pub fn of(topics: impl IntoIterator<Item = Topic>) -> Self {
        topics.into_iter().fold(Self::EMPTY, |s, t| s.with(t))
    }

    /// This set plus `topic`.
    pub const fn with(self, topic: Topic) -> Self {
        Self(self.0 | topic.bit())
    }

    /// This set without `topic`.
    pub const fn without(self, topic: Topic) -> Self {
        Self(self.0 & !topic.bit())
    }

    /// True when `topic` is in the set.
    pub const fn contains(self, topic: Topic) -> bool {
        self.0 & topic.bit() != 0
    }

    /// Both sets.
    pub const fn union(self, other: TopicSet) -> Self {
        Self(self.0 | other.0)
    }

    /// True when the set is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The raw bits (for an atomic mask).
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// A set from raw bits; unknown bits are dropped.
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits & ((1 << Topic::ALL.len()) - 1))
    }

    /// The topics in the set.
    pub fn iter(self) -> impl Iterator<Item = Topic> {
        Topic::ALL.into_iter().filter(move |t| self.contains(*t))
    }
}

impl fmt::Debug for TopicSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// A state key: a topic plus an optional object id (for example one key per source).
/// `seq`, event slots and per-session coalescing are per state key. The first slice's
/// topics have no object id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StateKey {
    /// The topic.
    pub topic: Topic,
    /// The object, when the topic has one key per object.
    pub object: Option<u32>,
}

impl StateKey {
    /// The key of a topic without object ids.
    pub const fn topic(topic: Topic) -> Self {
        Self {
            topic,
            object: None,
        }
    }
}

impl fmt::Display for StateKey {
    /// `record.state`, or `source.changed:7` with an object id: the key in `seq` maps.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.object {
            None => f.write_str(self.topic.name()),
            Some(id) => write!(f, "{}:{id}", self.topic.name()),
        }
    }
}

/// An event's topic and data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "topic", content = "data")]
pub enum EventPayload {
    /// The whole program-scene source list.
    #[serde(rename = "source.changed")]
    SourceChanged(Vec<SourceInfo>),
    /// The recording state.
    #[serde(rename = "record.state")]
    RecordState(RecordState),
    /// The canvas.
    #[serde(rename = "canvas.changed")]
    CanvasChanged(Canvas),
    /// The settings.
    #[serde(rename = "settings.changed")]
    SettingsChanged(Settings),
    /// Audio levels of every audio source.
    #[serde(rename = "audio.levels")]
    AudioLevels(Vec<SourceLevels>),
    /// The stream state.
    #[serde(rename = "stream.state")]
    StreamState(StreamState),
    /// The scenes (protocol 1.6).
    #[serde(rename = "scene.changed")]
    SceneChanged(SceneList),
    /// The virtual camera, `frames` always `null` (protocol 1.12).
    #[serde(rename = "vcam.changed")]
    VcamChanged(VcamStatus),
    /// The replay buffer (protocol 1.27).
    #[serde(rename = "replay.changed")]
    ReplayChanged(ReplayStatus),
    /// A recording state of a canvas other than the main one (protocol 1.29).
    #[serde(rename = "record.canvas_state")]
    RecordCanvasState(CanvasRecordState),
    /// A stream state of a canvas other than the main one (protocol 1.29).
    #[serde(rename = "stream.canvas_state")]
    StreamCanvasState(CanvasStreamState),
}

impl EventPayload {
    /// Its topic.
    pub const fn topic(&self) -> Topic {
        match self {
            EventPayload::SourceChanged(_) => Topic::SourceChanged,
            EventPayload::RecordState(_) => Topic::RecordState,
            EventPayload::CanvasChanged(_) => Topic::CanvasChanged,
            EventPayload::SettingsChanged(_) => Topic::SettingsChanged,
            EventPayload::AudioLevels(_) => Topic::AudioLevels,
            EventPayload::StreamState(_) => Topic::StreamState,
            EventPayload::SceneChanged(_) => Topic::SceneChanged,
            EventPayload::VcamChanged(_) => Topic::VcamChanged,
            EventPayload::ReplayChanged(_) => Topic::ReplayChanged,
            EventPayload::RecordCanvasState(_) => Topic::RecordCanvasState,
            EventPayload::StreamCanvasState(_) => Topic::StreamCanvasState,
        }
    }
}

/// An event: `{"topic": "record.state", "seq": 42, "data": {...}}` as the params of a
/// JSON-RPC `event` notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The object id when the topic has one key per object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<u32>,
    /// The state key's change counter (sampled topics: a sample counter).
    pub seq: u64,
    /// Topic and data.
    #[serde(flatten)]
    pub payload: EventPayload,
}

impl Event {
    /// Its state key.
    pub fn key(&self) -> StateKey {
        StateKey {
            topic: self.payload.topic(),
            object: self.object,
        }
    }
}
