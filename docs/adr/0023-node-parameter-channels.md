# ADR-0023: Latest-value parameter channels for running nodes

- Status: Proposed
- Date: 2026-11-02
- Deciders: Salih (owner)

## Context

ADR-0020 moves node instances between the control thread and the executor inside whole schedules. That covers structure: nodes joining and leaving. It does not cover changing what a node that keeps running does. Examples:

- The compositor's layer list (transform, crop, opacity per layer) changes when the user drags a source, fades it or edits a scene. The scene binder (`rbs_scene::binder`) resolves layers to link indices on the control thread; the result has to reach the running `org.rbs.compositor` node.
- An audio mixer's gains, a transition's progress and a filter's settings change in the same way.

Sending a whole new schedule for each of these would prepare nothing, but it would still lay out every slot, cost about 56 µs per edit for a 32-source canvas, and block other edits for a tick (`Busy`). A drag sends tens of updates per second.

Forces:

- Real time (P6, hot paths): reading a parameter in `process` must not allocate, free, block or take a contended lock. Freeing matters as much as allocating: dropping the old `Vec` of layers on the render thread is a deallocation.
- Only the newest value matters. A compositor that is a tick behind should jump to the latest layout, not replay the intermediate ones.
- Layers (P1): `rbs-graph` must not know what a layer list is; `rbs-gpu` defines it.
- `unsafe` is allowed only in adapter crates and marked low-level modules (CLAUDE.md rule 7). `rbs-graph` is a core crate.

## Decision

1. **Typed channel per node and parameter.** `rbs_graph::runtime::param::channel(initial: T) -> (ParamSender<T>, ParamReceiver<T>)`, with `T: Send`. Neither end is `Clone`: one writer, one reader. The composition root creates the pair when it builds a node, passes the receiver to the node's constructor, and keeps the sender (keyed by `NodeId`). The graph runtime does not route parameters, so a node can have several channels of different types.
2. **Latest-value semantics.** `ParamSender::send(value)` makes `value` the newest one; an older value that was not taken yet is dropped on the sender's thread. `ParamReceiver::update()` takes the newest value if there is one and returns `true` when the current value changed; `get()` returns it. Values sent from one thread are seen in send order; intermediate values may be skipped.
3. **Nothing is dropped on the receiving side.** `update` moves values with `std::mem::swap`. The value it replaces goes back to the sender through the exchange and is dropped there, or handed out by `ParamSender::recycle()` so the control thread can clear and refill it without allocating. If the sender has not taken back the previous replaced value, the receiver keeps the new one waiting rather than drop anything. (By construction `send` always takes the replaced value, so this case does not arise with one sender; the check is defensive.)
4. **Exchange.** An `AtomicBool` "fresh value ready" flag plus a `std::sync::Mutex` around two `Option<T>` slots (`fresh`, `used`).
   - The receiver loads the flag first; with nothing new, `update` is one atomic load and returns.
   - With something new, the receiver calls `try_lock`, never `lock`. If the sender holds the exchange at that moment, `update` returns `false` and the value is taken on the next call, one tick later.
   - The sender holds the lock only to move `Option`s, never while running user code or dropping a value.
   - A poisoned lock is used as is: the exchange holds only `Option`s, so it cannot be left inconsistent.
   - The mutex is locked once when the channel is created, so a platform mutex that initialises lazily never does so on the real-time thread.
5. **No `unsafe`.** The channel is written in safe Rust. The `try_lock` costs nothing measurable in the common case because the flag check comes first.
6. **Lifetime.** The receiver lives in the node. While the node is idle on the control side, the channel stays open; `ParamSender::is_closed()` turns true once the node is dropped. Values sent while the node is idle are taken on its first `update` after it joins again.

## Consequences

- Live parameter edits do not touch the schedule and do not make `apply` return `Busy`. A value sent between ticks is visible on the next tick, or one tick later when `update` meets the sender holding the lock.
- Measured on the reference VM (Xeon 2.1 GHz), `cargo bench -p rbs-graph --bench executor -- param_update`:
  - `update` with nothing new: about 2.9 ns;
  - `update` that takes a 64-element `Vec`: under 50 ns, including about 20 ns of timer overhead.
- The allocation guard in `crates/rbs-graph/tests/param.rs` checks that `update` neither allocates nor frees, and a two-thread test checks that no value is ever dropped on the receiving thread.
- A node that wants derived state (for example GPU uniform buffers built from the layer list) rebuilds it when `update` returns `true`, within real-time rules, or reserves it in `warm`.
- Parameters that must be applied together with a structural change (a new layer and the link that feeds it) cannot use this channel alone; they arrive either before or after the swap. The compositor tolerates this: a layer whose link index is out of range is skipped for that tick. A "parameters ride with the schedule" variant can be added if a node needs exact pairing.
- One more `Arc` and mutex per channel. That is negligible next to the node.

## Alternatives considered

- **Send a new schedule for every parameter change.** Correct but costs a full `apply` per drag step and blocks structural edits for a tick.
- **A lock-free triple buffer with `UnsafeCell`, own code or the `triple_buffer` crate.** Slightly faster when a value changes. But it needs `unsafe` in a core crate, or a new dependency in one, and `triple_buffer` keeps all three values alive, so the control thread cannot recycle them as simply. The measured cost of the safe version is already well under the per-node tick overhead (about 25 ns).
- **`ArcSwap` / `Arc<T>` replacement.** The receiver would drop the last reference to an old `Arc` and so free memory on the real-time thread, unless a deferred-free scheme is added.
- **A bounded `std::sync::mpsc` channel of values.** Delivers every value instead of the newest, so a slow reader replays stale layouts, and old values are dropped on the receiver.
- **A generic `set_param(&mut self, Box<dyn Any>)` method on `Node`.** Needs dynamic typing and allocation per update, and routing by the runtime.

## Review

Pending: independent review per ADR-0014.
