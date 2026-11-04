//! The control-thread side (ADR-0044 decisions 2, 5 and 6): the [`ControlTarget`] the
//! composition root implements, the [`ControlEndpoint`] it drains, and the
//! [`EventPublisher`] that counts `seq` per state key and fills the event slots.
//!
//! The control thread never blocks on API output. Replies go into a channel sized by
//! the requests that can be in flight (one per connection under the connection cap),
//! so a send never fails while the API runs and is dropped once it stopped. Events go
//! into per-state-key latest-value slots that a newer event overwrites; an `events
//! pending` flag wakes the API thread once per batch of writes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use crate::codec::RequestId;
use crate::error::{ApiError, ErrorKind};
use crate::event::{Event, EventPayload, StateKey, Topic, TopicSet};
use crate::request::{Command, Response, Subscribed};
use crate::transport::ConnectionId;
use crate::types::SeqMap;
use crate::wake::Wake;

/// A command the session forwards to the control thread.
#[derive(Debug, Clone, PartialEq)]
pub struct Forwarded {
    /// The session's connection.
    pub conn: ConnectionId,
    /// The request's id.
    pub id: RequestId,
    /// The command.
    pub command: Command,
}

/// The control thread's answer to a [`Forwarded`] command.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    /// The session's connection.
    pub conn: ConnectionId,
    /// The request's id.
    pub id: RequestId,
    /// The answer.
    pub result: Result<Response, ApiError>,
}

/// Carries out commands on the control thread, which owns the studio, the scene
/// collection, recording profiles and canvas settings. Implemented by the composition
/// root (`rbs-app`, `rbs-cli`); tests use a fake.
pub trait ControlTarget {
    /// Carries out one command. State queries return the `seq` of the state keys they
    /// include (`events.seq_map`); every state change calls `events.changed`.
    /// `events.subscribe` never reaches the target.
    fn handle(
        &mut self,
        command: Command,
        events: &mut EventPublisher,
    ) -> Result<Response, ApiError>;
}

/// Shared between the event publisher (control thread) and the API thread.
#[derive(Debug)]
pub(crate) struct EventShared {
    mask: AtomicU32,
    slots: Mutex<HashMap<StateKey, Event>>,
    pending: AtomicBool,
    wake: Arc<Wake>,
}

impl EventShared {
    pub(crate) fn new(wake: Arc<Wake>) -> Self {
        Self {
            mask: AtomicU32::new(0),
            slots: Mutex::new(HashMap::new()),
            pending: AtomicBool::new(false),
            wake,
        }
    }

    /// Sets the topics API sessions subscribe to. Called before the subscribe that
    /// needs it is forwarded, so the `seq` read for its answer comes after.
    pub(crate) fn set_mask(&self, topics: TopicSet) {
        self.mask.store(topics.bits(), Ordering::Release);
    }

    /// The events written since the last call. Clears the pending flag before it reads
    /// the slots, so an event written meanwhile sets the flag again and is not missed.
    pub(crate) fn drain(&self) -> Vec<Event> {
        if !self.pending.swap(false, Ordering::AcqRel) {
            return Vec::new();
        }
        let mut events: Vec<Event> = self
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, e)| e)
            .collect();
        events.sort_by_key(Event::key);
        events
    }
}

/// The publisher's link to a running API server.
#[derive(Debug, Clone)]
pub struct EventLink(pub(crate) Arc<EventShared>);

type LocalSink = Box<dyn FnMut(&Event) + Send>;

/// Counts `seq` per state key and publishes events, on the control thread. Exists
/// whether or not the API runs: without a link and without a local subscriber a change
/// costs one counter increment and builds no payload (ADR-0013).
#[derive(Default)]
pub struct EventPublisher {
    seqs: HashMap<StateKey, u64>,
    link: Option<EventLink>,
    local: Option<(TopicSet, LocalSink)>,
}

impl EventPublisher {
    /// A publisher with no subscriber.
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes to a running API server from now on.
    pub fn attach(&mut self, link: EventLink) {
        self.link = Some(link);
    }

    /// Stops publishing to the API server.
    pub fn detach(&mut self) {
        self.link = None;
    }

    /// Registers the in-process UI as a permanent subscriber to `topics`; `sink` runs on
    /// the control thread and must not block.
    pub fn set_local(&mut self, topics: TopicSet, sink: impl FnMut(&Event) + Send + 'static) {
        self.local = Some((topics, Box::new(sink)));
    }

    /// The current `seq` of a state key (0 before its first change).
    pub fn seq(&self, key: StateKey) -> u64 {
        self.seqs.get(&key).copied().unwrap_or(0)
    }

    /// The current `seq` of every state key of the state topics in `topics`, for state
    /// query answers and `events.subscribe`.
    pub fn seq_map(&self, topics: impl IntoIterator<Item = Topic>) -> SeqMap {
        let mut map = SeqMap::new();
        for topic in topics {
            if topic.is_sampled() {
                continue;
            }
            map.insert(topic.name().to_owned(), self.seq(StateKey::topic(topic)));
            for (key, seq) in &self.seqs {
                if key.topic == topic && key.object.is_some() {
                    map.insert(key.to_string(), *seq);
                }
            }
        }
        map
    }

    /// Forgets a state key whose object is gone (a deleted source), so per-object keys
    /// do not accumulate. Its `seq` restarts at 0 if the key comes back.
    pub fn forget(&mut self, key: StateKey) {
        self.seqs.remove(&key);
    }

    /// The state of `key` changed: increments its `seq` and, when anyone subscribes to
    /// its topic, builds the payload once and publishes it. Returns the new `seq`.
    pub fn changed(&mut self, key: StateKey, payload: impl FnOnce() -> EventPayload) -> u64 {
        let seq = self.seqs.entry(key).or_insert(0);
        *seq += 1;
        let seq = *seq;
        let api = self
            .link
            .as_ref()
            .filter(|l| TopicSet::from_bits(l.0.mask.load(Ordering::Acquire)).contains(key.topic));
        let local = self
            .local
            .as_mut()
            .filter(|(topics, _)| topics.contains(key.topic));
        if api.is_none() && local.is_none() {
            return seq;
        }
        let payload = payload();
        debug_assert_eq!(payload.topic(), key.topic, "payload of another topic");
        let event = Event {
            object: key.object,
            seq,
            payload,
        };
        if let Some((_, sink)) = local {
            sink(&event);
        }
        if let Some(link) = api {
            link.0
                .slots
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key, event);
            if !link.0.pending.swap(true, Ordering::AcqRel) {
                link.0.wake.signal();
            }
        }
        seq
    }
}

/// The control thread's end of the API: forwarded commands in, replies out. Drain it
/// after the local UI's queue, which always goes first.
pub struct ControlEndpoint {
    pub(crate) requests: Receiver<Forwarded>,
    pub(crate) replies: Sender<Reply>,
    pub(crate) wake: Arc<Wake>,
    pub(crate) events: Arc<EventShared>,
}

impl ControlEndpoint {
    /// The link to attach to the control thread's [`EventPublisher`].
    pub fn event_link(&self) -> EventLink {
        EventLink(self.events.clone())
    }

    /// The next forwarded command, without waiting.
    pub fn try_next(&self) -> Option<Forwarded> {
        self.requests.try_recv().ok()
    }

    /// Carries out one forwarded command and sends the reply.
    pub fn serve(
        &self,
        forwarded: Forwarded,
        target: &mut dyn ControlTarget,
        events: &mut EventPublisher,
    ) {
        let Forwarded { conn, id, command } = forwarded;
        let result = match command {
            Command::EventsSubscribe(s) => Ok(Response::Subscribed(Subscribed {
                seq: events.seq_map(s.topics.iter().copied()),
            })),
            other => target.handle(other, events),
        };
        // The API thread may have stopped; then nobody waits for the reply.
        if self.replies.send(Reply { conn, id, result }).is_ok() {
            self.wake.signal();
        }
    }

    /// Serves the next forwarded command, if one waits. Returns true when it served one.
    pub fn serve_one(&self, target: &mut dyn ControlTarget, events: &mut EventPublisher) -> bool {
        match self.try_next() {
            Some(f) => {
                self.serve(f, target, events);
                true
            }
            None => false,
        }
    }
}

/// An error for a command a target does not support.
pub fn unsupported(command: &Command) -> ApiError {
    ApiError::new(
        ErrorKind::Failed,
        format!("{} is not supported by this server", command.method()),
    )
}
