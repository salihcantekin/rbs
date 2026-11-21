//! The shared monitor: one OS listener per process, reference-counted by subscriptions
//! (`docs/specs/input-port.md`, invariants 1-3 and 7).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rbs_core::capability::{CapabilityKey, CapabilitySet, CapabilityValue};
use rbs_core::time::Timestamp;

use crate::caps;
use crate::error::InputError;
use crate::event::{EventMask, PointerSample, TimedEvent};
use crate::ring::{EventRing, PointerHistory};

/// Subscriptions one monitor holds at once.
pub const MAX_SUBSCRIPTIONS: usize = 8;

/// An OS input API (the adapter).
pub trait InputBackend: Send + Sync + 'static {
    /// Short adapter name for diagnostics, for example `"raw-input"`.
    fn name(&self) -> &'static str;
    /// What the adapter can deliver (keys in [`caps`]).
    fn capabilities(&self) -> CapabilitySet;
    /// Starts the OS listener for `mask` (never empty, never a class the capabilities
    /// lack). Control thread; may start a thread and wait at most 500 ms for it. Events
    /// go to `sink` from the adapter's one listener thread.
    fn start(&self, mask: EventMask, sink: InputSink) -> Result<Box<dyn RunningInput>, InputError>;
}

/// A started OS listener.
pub trait RunningInput: Send {
    /// The union of the subscriptions' masks changed (control thread, never empty).
    fn set_mask(&mut self, mask: EventMask);
    /// Removes the OS listener and joins the adapter's thread (control thread).
    fn stop(self: Box<Self>);
}

struct Channel {
    ring: EventRing,
    /// The subscription's mask bits; 0 while the channel is free or ending.
    mask: AtomicU8,
    /// Live handles: the `Subscription` and the `InputReader`. Free at 0.
    handles: AtomicU8,
}

struct Shared {
    channels: Box<[Channel]>,
    history: PointerHistory,
}

impl Shared {
    fn new() -> Self {
        Self {
            channels: (0..MAX_SUBSCRIPTIONS)
                .map(|_| Channel {
                    ring: EventRing::new(),
                    mask: AtomicU8::new(0),
                    handles: AtomicU8::new(0),
                })
                .collect(),
            history: PointerHistory::new(),
        }
    }
}

/// Where an adapter delivers input. Used from the adapter's one listener thread;
/// every method is real-time safe (no allocation, lock or wait).
pub struct InputSink {
    shared: Arc<Shared>,
}

impl InputSink {
    /// Records a pointer position in the history.
    pub fn pointer(&self, sample: PointerSample) {
        self.shared.history.write(&sample);
    }

    /// Pushes an event to every subscription whose mask contains its class.
    pub fn event(&self, timed: TimedEvent) {
        let class = timed.event.class().bits();
        for channel in self.shared.channels.iter() {
            if channel.mask.load(Ordering::Acquire) & class != 0 {
                channel.ring.push(&timed);
            }
        }
    }
}

struct State {
    running: Option<Box<dyn RunningInput>>,
    shared: Option<Arc<Shared>>,
    masks: [EventMask; MAX_SUBSCRIPTIONS],
}

impl State {
    fn union(&self) -> EventMask {
        self.masks
            .iter()
            .fold(EventMask::NONE, |all, mask| all.union(*mask))
    }
}

struct Inner {
    backend: Box<dyn InputBackend>,
    state: Mutex<State>,
}

impl Inner {
    /// Control thread only, never contended by a real-time thread.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The shared monitor over one backend. Cheap to clone; clones share the listener.
#[derive(Clone)]
pub struct InputMonitor {
    inner: Arc<Inner>,
}

impl InputMonitor {
    /// A monitor over `backend`. Starts nothing (P13).
    pub fn new(backend: Box<dyn InputBackend>) -> Self {
        Self {
            inner: Arc::new(Inner {
                backend,
                state: Mutex::new(State {
                    running: None,
                    shared: None,
                    masks: [EventMask::NONE; MAX_SUBSCRIPTIONS],
                }),
            }),
        }
    }

    /// The backend's capabilities.
    pub fn capabilities(&self) -> CapabilitySet {
        self.inner.backend.capabilities()
    }

    /// The backend's name.
    pub fn backend_name(&self) -> &'static str {
        self.inner.backend.name()
    }

    /// Whether the OS listener runs now.
    pub fn is_running(&self) -> bool {
        self.inner.lock().running.is_some()
    }

    /// Opens a subscription (control thread). The first one starts the backend; later
    /// ones widen its mask when they ask for more.
    pub fn subscribe(&self, mask: EventMask) -> Result<(Subscription, InputReader), InputError> {
        if mask.is_empty() {
            return Err(InputError::Unsupported("an empty event mask"));
        }
        if mask.intersects(EventMask::KEYS) {
            return Err(InputError::Unsupported("keys (ADR-0165 phase 2)"));
        }
        let offered = offered(&self.inner.backend.capabilities());
        if !offered.contains(mask) {
            return Err(InputError::Unsupported(
                "an event class this backend does not offer",
            ));
        }
        let mut state = self.inner.lock();
        let slot = (0..MAX_SUBSCRIPTIONS)
            .find(|&i| {
                state.masks[i].is_empty()
                    && state
                        .shared
                        .as_ref()
                        .is_none_or(|s| s.channels[i].handles.load(Ordering::Acquire) == 0)
            })
            .ok_or(InputError::Busy)?;
        let before = state.union();
        let existing = state.shared.clone();
        let shared = match (existing, state.running.as_mut()) {
            (Some(shared), Some(running)) => {
                let after = before.union(mask);
                if after != before {
                    running.set_mask(after);
                }
                shared
            }
            _ => {
                let shared = Arc::new(Shared::new());
                let sink = InputSink {
                    shared: Arc::clone(&shared),
                };
                let running = self.inner.backend.start(mask, sink)?;
                state.running = Some(running);
                state.shared = Some(Arc::clone(&shared));
                shared
            }
        };
        let channel = &shared.channels[slot];
        // The channel's mask has been 0 since the last subscription on it ended, so the
        // producer pushes nothing new; at most one push that read the old mask just
        // before can land after this reset (a stale event with an older time).
        channel.ring.reset();
        channel.handles.store(2, Ordering::Release);
        channel.mask.store(mask.bits(), Ordering::Release);
        state.masks[slot] = mask;
        drop(state);
        Ok((
            Subscription {
                inner: Arc::clone(&self.inner),
                shared: Arc::clone(&shared),
                slot,
                mask,
            },
            InputReader { shared, slot, mask },
        ))
    }
}

/// The classes the capabilities offer.
fn offered(caps: &CapabilitySet) -> EventMask {
    let flag = |key: &CapabilityKey| matches!(caps.get(key), Some(CapabilityValue::Flag(true)));
    let mut mask = EventMask::NONE;
    if flag(&caps::POINTER) {
        mask = mask.union(EventMask::POINTER);
    }
    if flag(&caps::BUTTONS) {
        mask = mask.union(EventMask::BUTTONS);
    }
    if flag(&caps::WHEEL) {
        mask = mask.union(EventMask::WHEEL);
    }
    mask
}

/// The control-side handle of a subscription. Dropping it ends the subscription; the
/// last one stops the backend and joins its thread, so drop it on the control thread.
pub struct Subscription {
    inner: Arc<Inner>,
    shared: Arc<Shared>,
    slot: usize,
    mask: EventMask,
}

impl Subscription {
    /// The classes this subscription receives.
    pub fn mask(&self) -> EventMask {
        self.mask
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let channel = &self.shared.channels[self.slot];
        channel.mask.store(0, Ordering::Release);
        let mut state = self.inner.lock();
        let ours = state
            .shared
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, &self.shared));
        if ours {
            let before = state.union();
            state.masks[self.slot] = EventMask::NONE;
            let union = state.union();
            if union.is_empty() {
                state.shared = None;
                if let Some(running) = state.running.take() {
                    running.stop();
                }
            } else if union != before {
                if let Some(running) = state.running.as_mut() {
                    running.set_mask(union);
                }
            }
        }
        drop(state);
        channel.handles.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The reader-side handle of a subscription, for the thread that consumes input.
/// Every method is real-time safe (no allocation, lock or wait).
pub struct InputReader {
    shared: Arc<Shared>,
    slot: usize,
    mask: EventMask,
}

impl InputReader {
    /// The classes this reader receives.
    pub fn mask(&self) -> EventMask {
        self.mask
    }

    /// The next event in arrival order, if any.
    pub fn next_event(&mut self) -> Option<TimedEvent> {
        self.shared.channels[self.slot].ring.pop()
    }

    /// The newest pointer sample at or before `time`. `None` without the pointer class,
    /// or when every kept sample is newer.
    pub fn pointer_at(&self, time: Timestamp) -> Option<PointerSample> {
        if !self.mask.contains(EventMask::POINTER) {
            return None;
        }
        self.shared.history.at(Some(time))
    }

    /// The newest pointer sample. `None` without the pointer class or before the first.
    pub fn latest_pointer(&self) -> Option<PointerSample> {
        if !self.mask.contains(EventMask::POINTER) {
            return None;
        }
        self.shared.history.at(None)
    }

    /// Events dropped for this subscription because its ring was full.
    pub fn dropped(&self) -> u64 {
        self.shared.channels[self.slot].ring.dropped()
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        self.shared.channels[self.slot]
            .handles
            .fetch_sub(1, Ordering::AcqRel);
    }
}
