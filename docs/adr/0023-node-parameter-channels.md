# ADR-0023: Latest-value parameter channels for running nodes

- Status: Accepted
- Date: 2026-11-02
- Deciders: Salih (owner)

## Context

ADR-0020 moves node instances between the control thread and the executor inside whole schedules. That covers structure: nodes joining and leaving. It does not cover changing what a node that keeps running does. Examples:

- The compositor's layer list (transform, crop, opacity per layer) changes when the user drags a source, fades it or edits a scene. The scene binder (`rbs_scene::binder`) resolves layers on the control thread; the result has to reach the running `org.rbs.compositor` node.
- An audio mixer's gains, a transition's settings and a filter's settings change in the same way.

Sending a whole new schedule for each of these would prepare nothing, but it would still lay out every slot, cost about 56 µs per edit for a 32-source canvas, and block other edits for a tick (`Busy`). A drag sends tens of updates per second.

Forces:

- Real time (P6, hot paths): reading a parameter in `process` must not allocate, free, block or wait on a lock. Freeing matters as much as allocating: dropping the old `Vec` of layers on the render thread is a deallocation.
- Only the newest value matters. A compositor that is a tick behind should jump to the latest layout, not replay the intermediate ones.
- Parameters and schedules travel separately, so a parameter value can be computed for a schedule that is not running yet, or that has already been replaced.
- Layers (P1): `rbs-graph` must not know what a layer list is; `rbs-gpu` defines it.
- `unsafe` is allowed only in adapter crates and marked low-level modules (CLAUDE.md rule 7). `rbs-graph` is a core crate.

## Decision

1. **Typed channel per node and parameter.** `rbs_graph::runtime::param::channel(initial: T) -> (ParamSender<T>, ParamReceiver<T>)`, with `T: Send`. Neither end is `Clone`: one writer, one reader. `send` and `recycle` take `&mut self`, so values come from one thread at a time; control code on several threads wraps the sender itself. The composition root creates the pair when it builds a node, passes the receiver to the node's constructor, and keeps the sender (keyed by `NodeId`). The graph runtime does not route parameters, so a node can have several channels of different types.
2. **Latest-value semantics.** `ParamSender::send(value) -> Option<T>` makes `value` the newest one and returns an older value that the receiver had not taken yet, for reuse. `ParamReceiver::update()` takes the newest value if there is one and returns `true` when the current value changed; `get()` returns it. Values arrive in send order; intermediate values may be skipped.
3. **Nothing is dropped on the receiving side.** `update` moves values with `std::mem::swap`. The value it replaces goes back to the sender through the exchange. `ParamSender::recycle()` hands it out so the control thread can clear and refill it without allocating; a value not recycled before the next `send` is dropped by `send`, on the control thread. The exchange keeps the invariant that a fresh value and a returned value are never both waiting, so `update` always has room to hand one back (checked by a `debug_assert!` and a unit test).
4. **Exchange.** An `AtomicBool` "fresh value ready" flag plus a `std::sync::Mutex` around two `Option<T>` slots (`fresh`, `used`).
   - The receiver loads the flag first; with nothing new, `update` is one atomic load and returns. The flag is set and cleared under the lock; the mutex orders the values, and a stale `false` only delays a value by one call.
   - With something new, the receiver calls `try_lock`, never `lock`. If the sender holds the exchange at that moment, `update` returns `false` and keeps the current value.
   - Both ends hold the lock only to move `Option`s, never while running user code or dropping a value.
   - A poisoned lock is used as is: the exchange holds only `Option`s, so it cannot be left inconsistent.
   - The mutex is locked once when the channel is created, so a platform mutex that initialises lazily never does so on the real-time thread.
5. **Real-time argument.** P6 says real-time code takes no contended locks and that communication uses lock-free queues. This design reads P6 as "the real-time thread never waits", and states the exception explicitly:
   - `try_lock` is one compare-and-swap (or `pthread_mutex_trylock`) and never waits. There is no priority inversion, because the real-time thread never waits for the control thread.
   - On futex-based platforms, the receiver's unlock issues a `FUTEX_WAKE` system call when a sender spun out and parked on the lock. That is a non-blocking call, and only happens when the two sides met on the lock.
   - Delivery latency depends on the control thread's scheduling. A value is taken on the first `update` after the sender released the exchange, normally the next tick. If the control thread is preempted inside `send`, or sends continuously, the receiver can miss several ticks in a row. A value is delayed in that case, never lost. This is the trade-off against a lock-free design (see Alternatives).
   - `docs/architecture/hot-paths.md` now lists `ParamReceiver::update` and states the rule: a never-waiting `try_lock` with a fallback is allowed on a hot path only with an ADR that argues it; `lock()` never is.
6. **Pairing with schedules: refer to producers by node id, not by position.** A parameter value and the schedule it was computed for take effect independently: the value can arrive before the swap or after it. Positions of links on a many-input pin shift when a producer leaves the plan, so a layer list that addressed inputs by link index could draw the wrong source with another layer's transform for a tick or two. Therefore:
   - `Inputs` exposes the producer of each link: `Inputs::source(pin, link) -> Option<NodeId>` and `Inputs::find(pin, node) -> Option<usize>`.
   - Parameters that refer to inputs carry the producer's `NodeId` (the binder's `BoundLayer::producer`). A link index, as returned by `rbs_scene::binder::resolve`, is only a hint: the node checks `source(pin, hint) == Some(producer)` and otherwise looks the producer up with `find`.
   - A layer whose producer is not scheduled is skipped for that tick. A producer that is scheduled but not in the parameter value is not drawn. So a mismatch shows at most a layer missing for a tick, never the wrong source.
7. **Animated values are evaluated by the node.** Fades, transitions and gain changes are sent as targets with timing (start time, duration, curve) that the node evaluates against `Tick` time, not as one value per frame from the control thread. Smoothness then does not depend on control-thread jitter or on point 5's latency. Audio nodes also ramp gain jumps to avoid clicks.
8. **No `unsafe`.** The channel is written in safe Rust.
9. **Lifetime and drop location.** The receiver lives in the node. While the node is idle on the control side, the channel stays open and `pending()` stays true; values sent meanwhile are taken on its first `update` after it joins again. `ParamSender::is_closed()` turns true once the node is dropped. The receiver's current value, and anything left in the exchange when the sender is gone, are dropped wherever the node is dropped. Under ADR-0020 that is the control thread, except for the last-resort `Executor::drop` at shutdown, which drops nodes on the executor thread.

## Consequences

- Live parameter edits do not touch the schedule and do not make `apply` return `Busy`.
- Measured on the reference VM (Xeon 2.1 GHz), `cargo bench -p rbs-graph --bench executor -- param_update`:
  - `update` with nothing new: about 2 to 3 ns.
  - `update` that takes a 64-element `Vec`: about 50 ns measured with the timer, of which the timer itself (bench `timer_only`) is about 31 ns, so about 20 ns of work. The cost is only paid on ticks where a value changed.
  - The bench measures the uncontended case. Under contention `update` returns early and costs less.
- Exposing link producers adds one `Vec<NodeId>` per schedule, built in `apply`, and one slice in `Inputs`. The executor tick bench stays within run-to-run noise: 0 to 7 % across repeated runs on the reference VM (a tick of 68 near-empty nodes about 1.58 to 1.60 µs, against 1.50 µs before).
- Tests in `crates/rbs-graph/tests/param.rs` and `param.rs` unit tests:
  - the allocation guard checks that `update` neither allocates nor frees, on one thread and inside the reader of a two-thread test;
  - the two-thread test checks send order and that no value is dropped on the receiving thread;
  - a held exchange makes `update` return `false` without allocating, and the next call takes the value;
  - the exchange invariant holds over an arbitrary call sequence;
  - a value sent while a node is idle is taken after it rejoins;
  - the last value is taken after the sender is dropped;
  - `Inputs::find` follows a producer when link positions shift.
- A node that wants derived state (for example GPU uniform buffers built from the layer list) rebuilds it when `update` returns `true`, within real-time rules, or reserves it in `warm`.
- Keep `T` small or heap-backed: values are moved under the lock and swapped inline, so a large inline `T` lengthens both.
- One more `Arc` and mutex per channel. That is negligible next to the node.

## Alternatives considered

- **Send a new schedule for every parameter change.** Correct and atomic with structure, but costs a full `apply` per drag step and blocks structural edits for a tick.
- **Stamp schedules with a generation and parameters with the generation they were resolved for.** Would let a node hold back a value until its schedule is running. But it needs a second slot in the receiver, and it still cannot fix a value that arrives late. Addressing producers by id (Decision 6) covers both directions without that.
- **A lock-free triple buffer with `UnsafeCell`, own code or the `triple_buffer` crate.** Delivery latency would not depend on the control thread's scheduling. But it needs `unsafe` in a core crate, or a new dependency in one, and `triple_buffer` keeps all three values alive, so the control thread cannot recycle them as simply. The safe version already costs about 2 ns per unchanged tick.
- **A pair of pre-allocated lock-free SPSC rings** (values forward, used values back, for example with `rtrb`). This is literally what P6 prescribes. It needs `unsafe` or a new dependency in a core crate, and it delivers every value instead of the newest, so the reader would have to drain the ring each tick. The API is small enough to be re-implemented this way later without breaking callers.
- **`ArcSwap` / `Arc<T>` replacement.** The receiver would drop the last reference to an old `Arc` and so free memory on the real-time thread, unless a deferred-free scheme is added.
- **A bounded `std::sync::mpsc` channel of values.** Delivers every value instead of the newest, so a slow reader replays stale layouts, and old values are dropped on the receiver.
- **A generic `set_param(&mut self, Box<dyn Any>)` method on `Node`.** Needs dynamic typing and allocation per update, and routing by the runtime.

## Review

Independent AI session per ADR-0014, 2026-11-02. It got only the proposal, specs and code. Verdict: changes requested. The exchange was found correct: no lost values, send order kept, no drop, allocation or wait in `update`. All concerns were addressed in the same change:

1. Blocking: a layer list addressed inputs by link position. Positions shift when a producer leaves the plan, so a value applied against the wrong schedule could draw the wrong source. Fixed by Decision 6: `Inputs::source` and `Inputs::find`, with parameters naming producers by `NodeId`. Test `inputs_name_the_producer_of_each_link_so_layers_survive_index_shifts`.
2. Blocking: the `Mutex` on the render path had to be argued against P6, and P15 had to be followed. Fixed by Decision 5 and the new rule and row in `hot-paths.md`. The `hot-path-review` checklist is in the PR. Owner sign-off: see below.
3. The latency claim ("one tick later") was too strong. Reworded in Decision 5.
4. The receiver's `spare` path could never run, and the ADR described it wrongly. Removed; the invariant is now stated, `debug_assert!`ed and tested.
5. The recycling API was incomplete. `send` now returns the unread value it displaces, and the docs say to call `recycle` before `send`.
6. The numbers were inexact. Timer overhead is now measured (`timer_only`), and the comparison with the per-node tick cost is corrected.
7. The size of `T` was not discussed. Guidance added.
8. The two-thread test could hang if the reader panicked. It now stops when the channel closes.
9. Missing tests: held exchange, idle rejoin, sender dropped first, displaced value, and allocations in the concurrent reader. All added.
10. API polish: `Debug` for both ends, `#[must_use]` on `channel`, and the single-writer rule documented.
11. Animated parameters: Decision 7.
12. Drop location: Decision 9 and module docs.
13. The SPSC-ring alternative was missing. Added.
14. The spec was out of sync. Updated.

Owner approval: Salih, 2026-11-02 (early-phase merge policy: merge once CI is green; large reviews later). This also stands in for the P15 human sign-off on the hot-path change until the owner's later review.
