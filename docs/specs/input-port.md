# Spec: Input port

- Status: Draft
- Owner crate(s): `rbs-input` (port); adapters `rbs-input-mac` (macOS), `rbs-input-win` (later PR)
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
- Keys and secure input: phase 2. Input Monitoring, which macOS 26 requires even for mouse events ("macOS adapter"), is read and asked through the port (`permission`, `request_permission`) and shown by the host as a fifth permission kind (ADR-0141, ADR-0165 decision 7, moved into phase 1 by the owner on 2026-11-22).
- Linux adapters: a later phase (ADR-0165 decision 0).

## Contract

The Rust types in `rbs_input` are the source of truth; the signatures below are kept in sync with them. `Timestamp` and `CapabilitySet` come from `rbs_core`; `EdgeClock` (OS time to master time for instants) is `rbs_core::timing::EdgeClock`.

### Events

```rust
/// A point in the OS desktop space: Windows physical pixels of the virtual screen
/// (per-monitor DPI aware v2; negative left of or above the primary monitor),
/// macOS points in the global display space (origin top-left of the main display).
pub struct DesktopPoint { pub x: f64, pub y: f64 }   // DesktopPoint::new(x, y)

#[non_exhaustive] pub enum Button { Left, Right, Middle, Back, Forward }

#[non_exhaustive] pub enum InputEvent {
    ButtonDown { button: Button, at: DesktopPoint },
    ButtonUp { button: Button, at: DesktopPoint },
    Wheel { at: DesktopPoint, dx: f32, dy: f32 },   // notches; positive dy away from the user
}

pub struct TimedEvent { pub time: Timestamp, pub event: InputEvent }   // master time
pub struct PointerSample { pub time: Timestamp, pub at: DesktopPoint }

/// Which event classes a subscription wants. KEYS is named now and refused until phase 2.
pub struct EventMask(u8);   // NONE, POINTER, BUTTONS, WHEEL, KEYS, MOUSE (the three mouse classes);
                            // union (also `|`), without, contains, intersects, is_empty
// Button::ALL lists the buttons in a fixed order; InputEvent::class gives an event's class.
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
    /// Whether the OS lets this process read input. Fast, never prompts, any thread.
    /// Default `Granted` (no OS permission).
    fn permission(&self) -> Permission { Permission::Granted }
    /// Shows the OS prompt when the user was not asked yet; returns the answer known
    /// after it. Blocks while the prompt shows (a worker thread, never the UI or a
    /// real-time thread). Default: `permission()`.
    fn request_permission(&self) -> Permission { self.permission() }
}

#[non_exhaustive] pub enum Permission { Granted, Denied, NotDetermined, Restricted, Unknown }
// The same meanings as the capture port's `Permission` (ADR-0141).

pub trait RunningInput: Send {
    /// The union of the subscribers' masks changed (control thread). The adapter
    /// stops asking the OS for classes no longer wanted.
    fn set_mask(&mut self, mask: EventMask);
    /// Removes the OS listener and joins the adapter's thread (control thread). Called
    /// while the monitor's lock is held: neither `stop` nor the listener thread may call
    /// back into the InputMonitor (deadlock).
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
impl InputMonitor {   // Clone: clones share the listener
    pub fn capabilities(&self) -> CapabilitySet;
    pub fn backend_name(&self) -> &'static str;
    pub fn permission(&self) -> Permission;          // the backend's, never takes the lock
    pub fn request_permission(&self) -> Permission;  // blocks while a prompt shows
    pub fn is_running(&self) -> bool;
    /// Control thread. The first subscription starts the backend; later ones only widen
    /// its mask. Fails with Unsupported for a class the backend lacks (KEYS in phase 1),
    /// Busy when MAX_SUBSCRIPTIONS are open, or the backend's start error.
    pub fn subscribe(&self, mask: EventMask) -> Result<(Subscription, InputReader), InputError>;
}

/// Control-side handle. Dropping it ends the subscription; the last one stops the backend
/// (joins its thread), so it is dropped on the control thread, never on a real-time thread.
pub struct Subscription;   // mask()

/// Reader-side handle, Send, for the thread that consumes events (the render thread).
pub struct InputReader;
impl InputReader {
    pub fn mask(&self) -> EventMask;
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
3. **Mask enforcement.** A reader never receives an event of a class its subscription did not ask for, nor an event pushed for an earlier subscription on the same channel: each subscription has a generation, the producer reads mask and generation as one value and stamps each event with it, and the reader skips any other generation or class. The host decides which masks a consumer may ask for (plugin permissions, ADR-0165 decision 3).
4. **Ring full.** A full event ring drops the newest event and counts it in `dropped`; the producer never waits.
5. **Pointer history.** The newest `POINTER_HISTORY` samples, overwritten in place. Each slot carries a sequence number written before and after the sample; a reader that sees them differ (or odd) reads the slot again, at most a few times, and otherwise skips it, so it never uses a torn sample and never spins without bound. Pointer moves never go into the event ring. While no subscription asks for the pointer the backend writes no samples, so the history may hold old ones when the pointer class comes back; consumers check `time`.
6. **Times.** Event and sample times are master time and do not decrease within one backend run.
7. **Threads.** `subscribe` and `Subscription` drop run on the control thread. `InputReader` methods run on any one thread at a time and are real-time safe (no allocation, lock or wait). `InputSink` is `Send` but not `Sync` and its methods run on the adapter's one listener thread; they are real-time safe.
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
| `input.permission` | Flag | The OS has a user permission for input (macOS Input Monitoring) |
| `input.permission.prompt` | Flag | `request_permission` can show the OS prompt in this process |
| `input.permission.restart` | Flag | A grant takes effect only after the app restarts |

The keys are the constants `rbs_input::caps::{POINTER, BUTTONS, WHEEL, KEYS, PERMISSION, PERMISSION_PROMPT, PERMISSION_RESTART}`. The permission keys mirror the capture port's `capture.permission.<kind>` keys, so `rbs_sources::permissions` folds the monitor's answer like any adapter's.

## Tests

Unit tests in `rbs-input` with a scripted test backend: start on first subscribe and stop on last drop; mask union and `set_mask`; mask filtering per reader; ring full drops the newest and counts; pointer history lookup at, before and after the kept window; a writer thread and a reader thread racing on the history never yield a torn sample, and a producer and a consumer racing on an event ring keep order and account for every event (progress-bounded runs with a hang guard, ADR-0032); events of an earlier generation or another class are skipped; `KEYS` refused; `Busy` at `MAX_SUBSCRIPTIONS`; no allocation on `next_event` and `pointer_at` (allocation counter).

## macOS adapter

`rbs-input-mac`, `MacInput::new(clock)`, name `cg-event-tap`, capabilities `input.pointer`, `input.buttons`, `input.wheel`. `rbs-platform` sets it as the sources' input monitor when the root passes the engine's master clock (`Options::with_master_clock`; `rbs-app` `platform::adapters_on`, `rbs-cli record`), so pointer times and capture frame times (stamped on the engine's clock through `InstanceContext`) share one clock.

- **Tap.** `CGEventTapCreate` at `kCGSessionEventTap`, head insert, `kCGEventTapOptionListenOnly`, asking only for the types the mask needs: `POINTER` mouse moved and left, right and other dragged; `BUTTONS` left, right and other down and up; `WHEEL` scroll wheel. With `POINTER` in the mask, button and wheel events also write a pointer sample (at most one per OS event). A NULL tap is `PermissionDenied`.
- **Permission (observed on macOS 26.7.1, Apple M4 Pro, 2026-11-22).** A listen-only tap for mouse events only needs **Input Monitoring**. In a process without the grant (spawned with its responsibility disclaimed, `CGPreflightListenEventAccess` false) the tap was created but received no event in 10 s, while a granted process saw 10,477 (posted and real). So `start` first calls `CGPreflightListenEventAccess`, which never shows a prompt, and returns `PermissionDenied` when it is false instead of running a silent tap. `permission` reads the preflight: granted is `Granted`; otherwise `NotDetermined` until a request ran in this process and `Denied` after it, because the preflight cannot tell "not asked" from "refused" and the system prompts only once (the ScreenCaptureKit rule of ADR-0141 decision 5). `request_permission` calls `CGRequestListenEventAccess`, which shows the system's alert and returns at once, so the answer right after it is still `Denied` until the user allows RBS and restarts it. The adapter declares `input.permission`, `.restart` (a grant applies after RBS restarts) and `.prompt` only when RBS is its own responsible app (`rbs_system_probe::app_identity::is_own_app`, ADR-0141 decision 10): Input Monitoring has no `Info.plist` usage key, so the prompt cannot end the process, but from a terminal it would ask for, and grant, the terminal's access; without `.prompt` `request_permission` returns the current status. To check on the Mac: whether the preflight turns true before a restart once the user allows RBS (the row would then read Allowed while the running tap still hears nothing). When and where RBS asks is the host's job (ADR-0141; ADR-0165 decision 1 moved the Permissions row into phase 1, owner's decision 2026-11-22). Accessibility is not needed. A terminal-started build inherits the terminal's (or the Claude app's) grant.
- **Thread.** `start` spawns `rbs-input-mac`, which creates the tap, adds its run-loop source to its own run loop in the common modes, enables the tap and reports ready or the error; `start` waits at most 500 ms (`Backend { code: 2 }` on a timeout, the thread then removes its tap without running it: it checks the stop flag `start` set right after it reports, so a report that lands just after the timeout does not leave a tap running). `stop` sets the flag, wakes the loop and joins the thread; it returns as soon as a mask rebuild in progress (`CGEventTapCreate`, a window-server call) returns, which is not bounded by RBS. The thread runs `CFRunLoopRunInMode(default, 2 s)` in a loop.
- **Stop and mask changes.** `set_mask` and `stop` store an atomic (wanted mask, stop flag) and post a block that calls `CFRunLoopStop` on the loop it runs on (`CFRunLoopPerformBlock` + `CFRunLoopWakeUp`); a block posted before the loop runs is run by its first run, so neither is lost. After each return the thread checks the stop flag (remove the source, invalidate the tap, exit) and the mask: a changed mask builds a new tap with the new types and then removes the old one; if the new tap is refused the old one stays. The callback also filters by the wanted mask, so a narrower mask applies at once. `stop` joins the thread. A run that returns `Finished` (the system invalidated the tap) ends the thread with a Warn line.
- **Callback.** On `kCGEventTapDisabledByTimeout` or `ByUserInput` it enables the tap again and counts it. Location from `CGEventGetLocation` (global display points, top-left of the main display, the `DesktopPoint` space). Other buttons by `kCGMouseEventButtonNumber`: 2 `Middle`, 3 `Back`, 4 `Forward`, others ignored. Wheel `dy` from `kCGScrollWheelEventFixedPtDeltaAxis1`, `dx` from axis 2, in lines. No allocation, lock or log; a panic is caught and never unwinds into Core Graphics.
- **Wheel signs.** A posted `CGEventCreateScrollWheelEvent2(Line, 2, +3, -2)` reads `dy` +3, `dx` -2: the field keeps the sign of the event (positive axis 1 is up, away from the user, for a wheel without natural scrolling). Whether a physical wheel's sign follows the user's natural-scrolling setting is not checked yet.
- **Time.** `CGEventGetTimestamp` is documented in nanoseconds but is mach ticks on some hardware. A pure function picks the reading nearer the host time at arrival: raw as nanoseconds, or `raw * numer / denom` (`mach_timebase_info`); when both are more than 1 s away it uses the arrival time. On the M4 Pro (timebase 125/3) posted events carry **nanoseconds** of `CLOCK_UPTIME_RAW`. Each event then observes the adapter's `EdgeClock` with (`CLOCK_UPTIME_RAW` now, master now) and is stamped with `to_master`, so its time is at most `clock.now()` (one nanosecond past it only when the previous stamp is already `now`, the `EdgeClock` floor). A button event with the pointer class writes its sample and its event with one stamp.
- **Counts.** `MacInput::stats()` gives events handled, re-enables and the ended tap threads' CPU time; each stop logs one `capture` line at Info (events, re-enables, mask changes, CPU), never event contents.
- **Cost (decision 8, M4 Pro, release, 10 s, `examples/tap_cost.rs`).** Idle: 0 events, tap thread CPU 1.3 ms (thread start included). 1,000 posted moves a second (swallowed after the tap): 999 events/s, tap thread CPU 607 ms, 6.1 % of one core; with an empty callback 6.4 %, so the cost is the OS delivery to the tap thread, not the callback. Delivery: the callback runs about 0.2-0.3 ms after the post.

## Windows adapter

`rbs-input-win`, `WinInput::new(clock)`, name `raw-input`, capabilities `input.pointer`, `input.buttons`, `input.wheel`, no permission keys (Raw Input needs none, so `permission` is the default `Granted`). `rbs-platform` sets it as the sources' input monitor on Windows the same way as the macOS adapter (`Options::with_master_clock`).

- **Thread.** `start` spawns `rbs-input-win`, which makes itself per-monitor DPI aware v2 (the WGC mapping thread's context, capture-port.md picture mapping rule 5), creates a message-only window (class `RbsRawInputSink`, parent `HWND_MESSAGE`), registers it for the mouse (usage page 1, usage 2) with `RIDEV_INPUTSINK` so reports arrive while another app has the focus, reports ready or the error and runs `GetMessageW`. `start` waits at most 500 ms (`Backend { code: 2 }` on a timeout: `start` sets the stop flag and posts the stop message if the window exists, and the thread checks the flag right after it reports, both sides sequentially consistent, so a late report never leaves the thread running). Raw Input always delivers the whole mouse, so `set_mask` only stores an atomic the handler reads. `stop` sets the flag, posts a private message to the window and joins the thread, which removes the registration (`RIDEV_REMOVE`, only when its own registration succeeded) and destroys the window. The window class stays registered for the process's life and is reused by the next start. A Raw Input registration is per process and usage: one running adapter per process (the `InputMonitor` shares it, ADR-0165), and any other code in RBS that registers the mouse would take the reports from it.
- **Handler.** `WM_INPUT` is read in the loop with `GetRawInputData` into a stack `RAWINPUT` (no allocation), then still dispatched so `DefWindowProcW` frees the system's copy. The position is `GetCursorPos` (physical pixels of the virtual screen, the `DesktopPoint` space; a report that cannot read it is skipped). `usButtonFlags` give one event per transition flag, in a fixed order: left, right, middle (`RI_MOUSE_MIDDLE_*`), button 4 `Back`, button 5 `Forward`. `RI_MOUSE_WHEEL` and `RI_MOUSE_HWHEEL` give `Wheel` with `usButtonData` as a signed count of 120ths of a notch (positive away from the user, and to the right), the port's signs. With `POINTER` in the mask every report writes one pointer sample. No allocation, lock or log.
- **Time.** QPC read when the report is handled (`GetMessageTime` has only millisecond resolution), observed with the master clock now by the adapter's `EdgeClock` and stamped with `to_master`. WGC frame times are QPC too, so both meet on the engine's clock.
- **Limits.** Raw Input sees neither the secure desktop (UAC, sign-in) nor, from a process that is not elevated, input to an elevated window (UIPI); the effects then show nothing there. No low-level hook is used (ADR-0165, alternatives).
- **Counts.** `WinInput::stats()` gives mouse reports handled and the ended threads' CPU time; each stop logs one `capture` line at Info, never event contents.
- **Cost.** Not measured on hardware yet (decision 8): to be measured on SC-PC at 1,000 Hz before the hot-path approval.

## Implementation status

- Port crate `rbs-input`: types, shared monitor, rings, history, tests (ADR-0165 PR 1).
- `rbs-input-mac`: built and registered on macOS (section above); unit tests for the pure helpers (`src/model.rs`, every OS), device tests on the Mac (`tests/device.rs`: start and stop through the monitor, mask change by a second subscription; `slow_` (ignored by default, the first post takes 0.7 s): posted move, left and middle click and wheel reaching the reader, swallowed so no app sees them; a manual real-click test).
- Permission (`permission`, `request_permission`, the `input.permission*` keys): port, macOS adapter, `rbs_sources::permissions::PermissionKind::InputMonitoring` and the host's ask when the window adds the first pointer filter (ADR-0141 decision 4); the macOS prompt and the restart are to be checked on the Mac with a bundled RBS.app.
- `rbs-input-win`: built and registered on Windows (section above); unit tests for the pure helpers (`src/model.rs`, every OS); Windows tests `starts_and_stops_twice` and `stamping_does_not_allocate` (`src/win.rs`). Not yet run on Windows hardware.
