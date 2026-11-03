# ADR-0041: A studio keeps a canvas running for preview and meters; recordings start and stop on the running engine

- Status: Proposed
- Date: 2026-11-03
- Deciders: Salih (owner)
- Supersedes: [ADR-0040](0040-recording-composition.md) decision 6 (who owns the graph when `rbs-app` records)

## Context

`rbs-cli record` (ADR-0040) builds a graph, runs it and shuts the engine down: one recording per engine. The desktop app needs something else. Its Program panel shows the live scene before, during and after a recording; its mixer meters move while nothing records; the user adds and removes sources while it runs; and a recording starts and stops without the preview blinking or devices reopening. The owner's Windows-first plan (2026-11-03) asks for exactly that next: the app wired to the engine with a real preview, an add-source device picker, a record button and output settings.

ADR-0040 decision 6 sketched an `attach`/`detach` entry point that adds a recording's nodes to "a graph, binder and engine the caller owns". Writing it showed that what the caller would own is most of the work: the binder's state, the logical graph, which source nodes have instances in which partition, the compositor's layer sender and the mixer's parameter sender with a level meter per strip, and the bookkeeping to take retired instances back. `rbs-app` and `rbs-cli` would both need all of it.

Three engine facts shape the design:

- **Scheduling needs a sink.** `GraphDescription::plan` drops nodes with no path to an `Output` (`SkipReason::NoSink`). A compositor with nothing consuming it, and so its sources, is not scheduled; nor is a mixer whose meters the UI wants while nothing records.
- **Plain `apply` swaps between ticks.** Removing an encoder and an output with `Engine::apply` releases them without a final tick: the frame in the encoder's queue and the packets in the output's queue are lost, and only `Engine::shutdown` runs the final ticks of ADR-0029.
- **A partition has a worker only while its schedule has nodes** (ADR-0027), so partitions can exist for the app's lifetime at no cost.

Forces: P1 (engine code names no adapter), P4 (the preview never reads frames back), P6/P15 (the preview node's `process` is a hot path), P13 (nothing runs that nobody uses), ADR-0008/ADR-0022 decision 2 (the UI is a client of the engine; one process is an optional mode), ADR-0026 (encoders and outputs on their own partitions).

## Decision

1. **`rbs_record::Studio` owns a running canvas.** `Studio::start(engine, gpu, collection, sources, settings)` binds the collection, builds the program compositor and, with `settings.audio`, the canvas's mixer, adds every partition (render, audio, video encoder, audio encoder, output; ids as in ADR-0040) and applies. It keeps the binder, the logical graph, a copy of the last collection, the source instances and the parameter senders. Its control-thread API: `update(collection, sources)` for scene edits, `start_recording(adapters, out)`, `stop_recording()`, `service()`, `progress()`, `sources()`, `preview()`, `meter(source)`, `shutdown()`. It never names an adapter (P1).
2. **Sinks keep the scene running.** Two new `Output` nodes give the scheduler a path while nothing records:
   - `rbs_nodes::PreviewNode` (`org.rbs.preview`, render partition, input `canvas`): owns an `rbs_gpu::share::PreviewPublisher` from `prepare` to `release`, publishes each new canvas frame and ticks the publisher every `process`; its endpoint is readable from `PreviewNodeStats::endpoint()` once prepared. It converts nothing while no UI is subscribed (the publisher skips). macOS and Windows; on other platforms `prepare` fails permanently, so a studio asked for a preview there fails to start.
   - `rbs_audio::MeterSinkNode` (`org.rbs.audio.meter-sink`, audio partition, input `in`): discards the mixer's block, so the mixer and its inputs run for the meters. No queue, no copy; `process` does nothing.
   Without a preview the canvas renders only while a recording runs: video sources are not opened (P13). Without audio no audio source is opened.
3. **Recordings attach to and detach from the running graph.** `start_recording` chooses every adapter first (ADR-0040 decision 2; `NoAdapter` before anything is added), then adds the video export (render), the video encoder, the audio encoder (with `out.audio`) and the output to the graph, opens the sources the new plan schedules for the first time, and applies once. If the export, an encoder or the output does not start, the recording's nodes are removed again and the call fails with `Start`; the studio runs on. `stop_recording` removes the recording's nodes and applies with `Engine::apply_draining` (decision 4), then takes their instances back. One recording at a time (`AlreadyRecording`).
4. **`Engine::apply_draining`** is `apply` with ADR-0029's final ticks for the partitions it empties: a partition that has a worker and whose new schedule is empty first runs its final tick(s) on the old schedule (one tick on a cadence; on a doorbell, until no reader has packets left), then gets the empty schedule. Partitions are sent in pipeline order over the queues before and after the edit, so the encoders finish before the output, which then writes the trailer in `release`. Plain `apply` is unchanged. A drain that times out is `DrainTimeout`, handled as at shutdown.
5. **Scene edits while running.** `update` re-runs the binder on the new collection, opens sources the plan schedules that have no instance yet, applies a replaced source break-before-make (first without the new node, so an exclusive camera is released), sends the compositor the layers of the new plan and the mixer one strip (gain 1, with a level meter) per input, applies, and takes retired instances back. The canvas's size and rate cannot change in a running studio. Binder problems and sources that fail to open are reported, not fatal. Compositor and mixer hold up to `MAX_LAYERS` (64) layers and strips without allocating.
6. **`Recording` is a studio without a preview plus one recording.** `Recording::start` checks the canvas, chooses the adapters, starts a studio in a strict mode (a binder problem that leaves out a source of the program scene, or a source that cannot be instantiated, fails the start before any device opens, as ADR-0040 specified) and starts the recording; `finish` stops the recording, then the engine. Its contract and behaviour are unchanged; `rbs-cli` keeps working as is.
7. **`rbs-app` hosts the studio in its own process for now** (ADR-0022 decision 2's one-process mode), on a dedicated control thread that owns the `Studio` and the adapter registries. The UI thread only sends commands over a channel and receives snapshots (sources, levels, recording state) through `slint::invoke_from_event_loop`; it never calls the studio. The preview reaches the UI through the studio's `PreviewEndpoint`, exactly as from another process (ADR-0028, ADR-0036), so moving the studio into an engine process when the control API exists changes only how commands and snapshots travel.

## Consequences

- One assembly of preview, meters, scene edits and recording serves the desktop app, the CLI and tests; ADR-0040's partition layout, selection rules, stream order, sync rules and failure rules stay.
- The preview node and the meter sink are new node types on hot paths: the preview node's `process` (publish and tick) is the publisher's existing hot path called from a node; the sink does nothing. Both need the P15 measurement and the owner's sign-off.
- A recording stopped on a running studio loses less than a shut-down one: the encoder encodes the last frame its queue holds and the output delivers every packet before the trailer. Frames still in the export's readback ring and the encoder's look-ahead remain lost (ADR-0040 consequences).
- The render worker stops and restarts with each recording when there is no preview; tick indices continue across the restart (ADR-0027), so a drained worker's final tick takes the next index ahead of time and the restarted worker skips it.
- Layer updates go through the compositor's parameter channel and schedule swaps through the engine; for one tick after an edit the compositor may draw the old layer list on the new schedule. Layers match producers by node id, so a missing producer is skipped, never drawn in the wrong place.
- Tests: `rbs-engine` checks that a branch removed with `apply_draining` drains in pipeline order while render keeps running and that the drained partitions take a branch again; `rbs-record`'s studio tests check meters without a recording, two recordings in a row on one engine with trailers and finished transports, a recording that cannot open its destination leaving the studio running, and sources added and removed while recording.

## Alternatives considered

- **ADR-0040 decision 6 as written** (the caller owns graph, binder and engine; `rbs-record` adds and removes a recording's nodes): every composition root would repeat the instance and parameter bookkeeping, which is where the bugs are. Rejected.
- **A recording per engine, the preview on another engine:** two engines would capture the same devices twice (an exclusive camera cannot) and the preview would not show what is recorded. Rejected.
- **Keep the canvas scheduled by marking the compositor an output:** changes the node contract for one use and does not cover the mixer. A sink node is explicit and costs nothing. Rejected.
- **Make plain `apply` drain the partitions it empties:** changes the timing of every edit for every caller, including tests that rely on swaps between ticks. Opt-in is narrower. Rejected.
- **Move the studio into an engine process now:** needs the control API (roadmap step 7); the one-process mode is already allowed by ADR-0022 and the preview path is the cross-process one, so nothing is lost by waiting. Deferred.

## Review

Pending: independent review per ADR-0014.
