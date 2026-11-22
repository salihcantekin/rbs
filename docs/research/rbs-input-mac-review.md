# rbs-input-mac: risky-review and hot-path notes (author's own pass)

- Date: 2026-11-22
- Change: the macOS input adapter `rbs-input-mac` (ADR-0165 phase 1, mouse), its registration in `rbs-platform`, and one master clock shared by the engine and the input monitor in `rbs-app` (`studio_start.rs`) and `rbs-cli record`.
- Contract: `docs/specs/input-port.md`, "macOS adapter".
- This is the author's own checklist. It is not the independent review: the callback is on the input hand-off hot path (`docs/architecture/hot-paths.md`, "adapters in `crates/rbs-input-*`"), so a human maintainer verifies it before merge (P15).

## risky-review

### unsafe and FFI

- `// SAFETY:` on every block: OK. `undocumented_unsafe_blocks` is denied by the workspace lints and clippy passes.
- Pointer lifetimes: OK. The callback's `user_info` is a `Box<TapState>` turned into a raw pointer (`OwnedState`). Every `Tap` is declared after the owner, so it drops first: its `Drop` disables the tap, removes the run-loop source and invalidates the port before the state is freed. A rebuild installs the new tap, then drops the old one, while the run loop is not running, so no callback can run in between. `TapState::tap` (for re-enabling) is written only while the loop is not running and points to a port its `Tap` retains.
- Ownership across FFI: OK. `CFRetained` owns the port, source and run loop; the block given to `CFRunLoopPerformBlock` is copied by Core Foundation.
- No unwinding into Core Graphics: OK. `on_event` wraps its body in `catch_unwind` and returns the event unchanged (a listen-only tap ignores the return value).
- Thread affinity: OK. The tap, its source and `CFRunLoopRunInMode` live on the tap thread. Other threads use only `CFRunLoopPerformBlock`, `CFRunLoopWakeUp` and the retain count through `LoopHandle`, which Core Foundation documents as thread-safe; `LoopHandle`'s `Send`/`Sync` impls say so.
- `mach_timebase_info` is declared locally (`#[repr(C)]` two `u32`), so no deprecated `libc` item is used.

### Real-time threads

- Callback: no allocation (`slow_handling_an_event_does_not_allocate`, allocation counter over 400 handled events: 0), no lock (atomics and the port's lock-free rings), no I/O, no log. The only log lines are on the tap thread after its run loop has stopped.
- Queues: the port's rings; a full ring drops the newest and counts it (port invariant 4).

### Concurrency

- `wanted` (mask bits) and `stop`: `Release` stores on the control thread, `Acquire` loads after each run-loop return; the callback's `Relaxed` load may see a mask one event late, which only matters for a narrower mask and the tap is rebuilt anyway. Counters are `Relaxed`.
- No lock is held across a callback. `stop` is called under the monitor's lock and joins the thread; the thread never calls back into the monitor, so there is no deadlock.
- `start` waits at most 500 ms. On a timeout it returns an error without joining; the thread finds the channel closed when it reports and removes its tap.

### Contracts

- Port traits unchanged. New public items: `rbs_input_mac::{MacInput, TapStats, codes, NAME}`, `rbs_platform::Options::{master_clock, with_master_clock}` (the struct is `#[non_exhaustive]`, so a new field is minor; `Debug` is now written by hand because `dyn Clock` has none), `rbs_app::platform::adapters_on`.
- The spec's "macOS adapter" section records the behaviour; `docs/status.md` updated.

### Tests

- Miri: N/A (the unsafe code is FFI to Core Foundation and Core Graphics).
- Pure helpers: unit tests on every OS. Device tests on this Mac (M4 Pro, macOS 26.7.1): start and stop through the monitor and a mask change by a second subscription; posted move, left and middle click and wheel reach the reader with times between the post and now (swallowed by an active tap after ours, so no app sees them; ignored by default because the first `CGEventPost` of a process returns after about 0.7 s).

## hot-path-review (callback `TapState::handle`, on the tap thread)

- No heap allocation per call: OK (allocation test above).
- No contended lock, blocking call, I/O or log per call: OK.
- Unchanged-picture and copy items: N/A (no pictures).
- No new branch testing whether an inactive feature is on: OK. The tap exists only while a subscription is open (P13); the per-event mask check is how a narrower mask applies before the rebuild, not a feature switch.
- Bounded work per call: OK (a fixed number of field reads, one edge-clock update, at most one sample and one event).
- Clock math: OK. `event_host_nanos` widens to `u128` for the tick scaling and saturates; the edge clock clamps to now and keeps stamps increasing.
- Errors are cheap: OK (no error values on the path).
- Numbers (`examples/tap_cost.rs`, release, 10 s, Apple M4 Pro): idle 0 events, tap thread CPU 1.3 ms; 1,000 posted moves a second: 999 events/s, 607 ms CPU (6.1 % of one core). The same run with the callback returning at once used 6.4 %, so the callback's own work is within noise and the cost is the OS delivering events to the tap thread. There is no before: the adapter is new, and it runs only while a pointer filter is Active.

## Left for the human reviewer

1. **Input Monitoring.** On macOS 26.7.1 a listen-only mouse tap without the grant is created and receives nothing. The adapter refuses to start then (`PermissionDenied`, checked with `CGPreflightListenEventAccess`, which never prompts). RBS does not ask for the permission anywhere yet; ADR-0165 decision 1 then moves the Permissions row into phase 1, which is the owner's call. A bundled RBS.app will see no pointer effects until that exists; a build started from a terminal uses the terminal's grant.
2. **Visible change.** With an input monitor registered, `set_served` now reports the pointer on macOS, so Add filter offers Click rings and Pointer halo there. That is a visible change to the filter list and needs the owner's approval before merge (rule 16).
3. **Wheel sign with natural scrolling** on a physical wheel or trackpad is not checked; posted events keep their sign.
4. **Real-event timestamps.** The unit was observed on posted events (nanoseconds); the pure chooser also handles ticks, and the manual test (`manual_real_click`) prints a real click's time against the clock.
5. **1000 Hz from a physical mouse** was not measured, only posted moves at 1000 a second.
