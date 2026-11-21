# Spec: Input port

- Status: Draft
- Owner crate(s): `rbs-input` (port); adapters `rbs-input-win`, `rbs-input-mac` (later PRs)
- Related ADRs: [0003](../adr/0003-ports-and-adapters.md), [0004](../adr/0004-single-master-clock.md), [0013](../adr/0013-zero-cost-when-unused.md), [0162](../adr/0162-extension-ready-registries-and-plugin-order.md), [0165](../adr/0165-show-mouse-and-keyboard-input.md)
- Related specs: [capture-port.md](capture-port.md), [video-filters.md](video-filters.md), [plugin-sdk.md](plugin-sdk.md)

## Purpose

The port through which RBS learns what the user does with the mouse (and, from ADR-0165 phase 2, the keyboard) anywhere on the desktop, so that video filters can draw click rings and a pointer halo on a captured display or window. Every OS input API is an adapter behind this one contract. The port owns the shared monitor: one OS listener per process, however many consumers subscribe, started by the first subscriber and stopped by the last (ADR-0165 decisions 1 and 2).

## Scope

In scope:

- Event types: pointer position, button down and up, wheel; keys are named in the types and refused until phase 2.
- `InputBackend`, the adapter trait, and `InputMonitor`, the shared, reference-counted monitor over one backend.
- Lock-free delivery: one event ring per subscription and one pointer history per monitor.
- Errors and capability keys.

Out of scope:

- Mapping desktop points to captured pictures: the capture port (ADR-0165 decision 5).
- Drawing: the pointer filters (ADR-0165 decision 3).
- Keys, secure input and the Input Monitoring permission: phase 2.
- Linux adapters: a later phase (ADR-0165 decision 0).

## Contract

The Rust types in `rbs_input` are the source of truth; the signatures below are kept in sync with them. `Timestamp` and `CapabilitySet` come from `rbs_core`; `EdgeClock` (OS time to master time for instants) is `rbs_core::timing::EdgeClock`.

### Events

```rust
/// A point in the OS desktop space: Windows physical pixels of the virtual screen
/// (per-monitor DPI aware v2; negative left of or above the primary monitor),
/// macOS points in the global display space (origin top-left of the main display).
pub struct DesktopPoint { pub x: f64, pub y: f64 }

#[non_exhaustive] pub enum Button { Left, Right, Middle, Back, Forward }

#[non_exhaustive] pub enum InputEvent {
    ButtonDown { button: Button, at: DesktopPoint },
    ButtonUp { button: Button, at: DesktopPoint },
    Wheel { at: DesktopPoint, dx: f32, dy: f32 },   // notches; positive dy away from the user
}

pub struct TimedEvent { pub time: Timestamp, pub event: InputEvent }   // master time
pub struct PointerSample { pub time: Timestamp, pub at: DesktopPoint }

/// Which event classes a subscription wants. KEYS is named now and refused until phase 2.
pub struct EventMask(u8);   // POINTER, BUTTONS, WHEEL, KEYS; union, contains, is_empty
```

### Adapter trait

```rust
pub trait InputBackend: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> CapabilitySet;
    /// Starts the OS listener for `mask` (never empty, never containing a class the
    /// capabilities lack). Called on the control thread; may start a thread and wait
    /// for it to be ready, at most 500 ms. Events go to `sink` from the adapter's thread.
    fn start(&self, mask: EventMask, sink: InputSink) -> Result<Box<dyn RunningInput>, InputError>;
}

pub trait RunningInput: Send {
    /// The union of the subscribers' masks changed (control thread). The adapter
    /// stops asking the OS for classes no longer wanted.
    fn set_mask(&mut self, mask: EventMask);
    /// Removes the OS listener and joins the adapter's thread (control thread).
    fn stop(self: Box<Self>);
}
```

`InputSink` is not `Clone`: the adapter uses it from its one listener thread (the event rings are single-producer):

- `pointer(sample)` writes the pointer history. Called at most once per OS pointer message; never blocks or allocates.
- `event(timed)` pushes to every subscription whose mask contains the event's class. Never blocks or allocates.

Adapters stamp events with master time through their own `EdgeClock` over the OS event clock (QPC on Windows, mach time on macOS), read when the event arrives (ADR-0165, "Time").

### Monitor and subscriptions

```rust
pub struct InputMonitor;   // InputMonitor::new(Box<dyn InputBackend>)
impl InputMonitor {
    pub fn capabilities(&self) -> CapabilitySet;
    /// Control thread. The first subscription starts the backend; later ones only widen
    /// its mask. Fails with Unsupported for a class the backend lacks (KEYS in phase 1),
    /// Busy when MAX_SUBSCRIPTIONS are open, or the backend's start error.
    pub fn subscribe(&self, mask: EventMask) -> Result<(Subscription, InputReader), InputError>;
}

/// Control-side handle. Dropping it ends the subscription; the last one stops the backend
/// (joins its thread), so it is dropped on the control thread, never on a real-time thread.
pub struct Subscription;

/// Reader-side handle, Send, for the thread that consumes events (the render thread).
pub struct InputReader;
impl InputReader {
    /// Next event in arrival order, if any. Wait-free, no allocation.
    pub fn next_event(&mut self) -> Option<TimedEvent>;
    /// Newest pointer sample at or before `time`; None when the history has none
    /// (empty, or every kept sample is newer). Wait-free, no allocation, bounded retries.
    pub fn pointer_at(&self, time: Timestamp) -> Option<PointerSample>;
    pub fn latest_pointer(&self) -> Option<PointerSample>;
    /// Events dropped for this subscription because its ring was full.
    pub fn dropped(&self) -> u64;
}
```

Constants: `MAX_SUBSCRIPTIONS` = 8, `EVENT_RING` = 256 events per subscription, `POINTER_HISTORY` = 64 samples (about 64 ms at 1000 Hz, more than capture latency).

### Invariants

1. **Zero cost when unused (P13).** A monitor with no subscription has no thread, no OS listener and no ring memory. Ring and history memory is allocated by the first `subscribe` and freed when the backend stops.
2. **One listener.** At most one running backend per monitor; its mask is the union of the open subscriptions' masks, updated with `set_mask` on every subscribe and drop.
3. **Mask enforcement.** A reader never receives an event of a class its subscription did not ask for. The host decides which masks a consumer may ask for (plugin permissions, ADR-0165 decision 3).
4. **Ring full.** A full event ring drops the newest event and counts it in `dropped`; the producer never waits.
5. **Pointer history.** The newest `POINTER_HISTORY` samples, overwritten in place. Each slot carries a sequence number written before and after the sample; a reader that sees them differ (or odd) reads the slot again, at most a few times, and otherwise skips it, so it never uses a torn sample and never spins without bound. Pointer moves never go into the event ring.
6. **Times.** Event and sample times are master time and do not decrease within one backend run.
7. **Threads.** `subscribe` and `Subscription` drop run on the control thread. `InputReader` methods run on any one thread at a time and are real-time safe (no allocation, lock or wait). `InputSink` methods run on the adapter's threads and are real-time safe.
8. **No input leaves the engine.** The port never logs event contents (only counts and faults, ADR-0158) and offers no way to read input outside a subscription.

### Errors

```rust
#[non_exhaustive] pub enum InputError {
    Unsupported(&'static str),   // a class or call this backend does not offer
    PermissionDenied,            // the OS refuses (phase 2: Input Monitoring)
    Busy,                        // MAX_SUBSCRIPTIONS open
    Backend { code: i64 },       // OS error code, adapter-documented
}
```

No variant carries heap data.

### Capability keys

| Key | Type | Meaning |
| --- | --- | --- |
| `input.pointer` | Flag | Pointer position in desktop space |
| `input.buttons` | Flag | Button down and up |
| `input.wheel` | Flag | Wheel |
| `input.keys` | Flag | Keys (phase 2) |

## Tests

Unit tests in `rbs-input` with a scripted test backend: start on first subscribe and stop on last drop; mask union and `set_mask`; mask filtering per reader; ring full drops the newest and counts; pointer history lookup at, before and after the kept window; a writer thread and a reader thread racing on the history never yield a torn sample (bounded run with a hang guard, ADR-0032); `KEYS` refused; `Busy` at `MAX_SUBSCRIPTIONS`; no allocation on `next_event` and `pointer_at` (allocation counter).

## Implementation status

- Port crate `rbs-input`: types, shared monitor, rings, history, tests (ADR-0165 PR 1).
- Adapters `rbs-input-win`, `rbs-input-mac`: not built yet.
