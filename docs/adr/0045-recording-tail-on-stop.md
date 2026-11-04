# ADR-0045: A stopped recording keeps its tail: the export ends first, the audio encoder flushes when its input ends

- Status: Proposed
- Date: 2026-11-04
- Deciders: Salih (owner)
- Amends: [ADR-0043](0043-end-of-stream-on-final-ticks.md) (closes its Decision 5 open question), [ADR-0026](0026-partitions-and-queues.md) (audio reader rings on final ticks)

## Context

After ADR-0043 the video encoder flushes on its final tick, so frames inside NVENC or AMF reach the file. Two parts of a recording's tail are still lost at stop (`io-nodes.md` and `recording.md` open questions; SC-PC, PR #100):

1. **Video readbacks.** The video export reads canvases back without waiting (`YuvDownload`, a ring of `DOWNLOAD_RING` = 4 staging buffers); a frame submitted on tick `i` appears on tick `i + 1` or later. When a recording stops, the export leaves the render partition: in the desktop app without any final tick (the render partition keeps running for the preview, `Studio::stop_recording`), in `rbs-cli` after one final render tick. Every readback still in flight is dropped. On SC-PC 3 to 9 frames near the stop never reached the encoder. In addition, `take_ready` returns only the newest completed readback and drops older ones as `stale`, so frames that complete together near the end are lost too, and the final render tick, which may run up to one period ahead of its time (engine spec 8a), submits a canvas after the moment the user pressed stop.
2. **Audio encoder tail.** The audio encoder node runs on a doorbell partition behind the audio queue. On drain it gets one final tick, then one per ring while its reader still has blocks, so no tick tells it that its input has ended (ADR-0043 Decision 5). It never flushes; the AAC session's partial frame and its codec delay (up to one frame plus the priming delay, about 21 to 45 ms) are dropped in `release`.

Forces: `Node::process` and the queue endpoints are hot paths (P6, P15). No per-iteration check for an inactive feature in hot loops (P13). Stopping must not wait without a bound, and a frozen clock in tests must not hang it. Tests use `TestClock` and tick indices (ADR-0032). The node contract (ADR-0020) and `Tick` stay as they are.

## Decision

1. **The export can be ended at a master-clock time.** `rbs_nodes::ExportEnd` (from `VideoExportNode::end()`, shared with the control thread) holds the end time in one `AtomicU64` (`u64::MAX` = not ending) and an `ended` flag. `end_at(t)` sets it (an earlier time wins). Each `process` loads it once (relaxed). Not ending, the node does what it did (`take_ready`, then submit a new canvas). Ending, it takes the **oldest** completed readback (`YuvDownload::take_oldest`, below), submits only canvases with a timestamp at or before the end, and sets `ended` on a tick whose time is past the end once no readback is in flight (`YuvDownload::in_flight() == 0`), so a later tick at or before the end can still bring its canvas. `prepare` clears `ended` and keeps the end time. Both the export and the video encoder node store the timestamp of the last frame they passed on or accepted (`ExportNodeStats::last_frame`, `EncoderNodeStats::last_frame`, one relaxed store per frame). `release` counts readbacks still in flight in `lost`, so a stop without the end (or past the limit) shows its loss.
2. **`YuvDownload::take_oldest`** returns the oldest completed frame and leaves newer completed frames for later calls, so none is dropped as stale. `take_ready` keeps its newest-wins behaviour for the run path. Both share one implementation, chosen at compile time (const generic), so the run path has no new branch.
3. **The studio stops in two steps.** `Studio::end_recording()` calls `end_at(engine clock now)`. `Studio::recording_tail_done()` is true once the export has `ended` and the video encoder has accepted its last frame (`EncoderNodeStats::last_frame >= ExportNodeStats::last_frame`: end to end, since the queue between them is a newest-wins mailbox and its writer may hold a frame for a tick), or once `END_TAIL_LIMIT` (500 ms) has passed on the master clock since the end (a stalled readback must not hold the stop up), and true when nothing records. The caller keeps servicing the studio until then and calls `stop_recording()` as before; its drain then gives the encoder its final tick, which takes the last frame from the video mailbox. `stop_recording()` without `end_recording()` behaves as before (the tail is lost). `Recording::end()` / `Recording::tail_done()` wrap them for `rbs-cli`. The desktop app's Stop and closing the app while recording use the two steps; the UI already shows Stopping until the host reports `Stopped`.
4. **Audio readers ring on final ticks.** `AudioReader::process` on a final tick (`Tick::draining`) rings its own doorbell after every block it takes, also the last one and also when it drops a block as too large, besides ringing while blocks are queued (unchanged). A draining doorbell partition therefore always ends with a final tick on which the reader passes nothing. Unmarked ticks are unchanged. This holds for audio queue readers only; packet and video readers give no such guarantee (`Tick::draining` doc).
5. **The audio encoder node flushes when its input ends.** The first final tick without a block on its input is the end of its input: the node calls `AudioEncoder::flush`, passes the packets on, counts it (`EncoderNode::flushes()`) and takes no block until it is prepared again. A final tick with a block submits and drains as usual. Blocking and allocating in this flush are allowed on that one final tick, under ADR-0043 Decision 4 (bounded by the drain timeout). That is the last tick of its partition, so packets the output's queue refuses on it (more than `PER_TICK_EVENTS`, or a full queue; an AAC flush returns one to three) and blocks that arrive after the flush (an upstream partition drained late, a cycle) are lost and counted (`EncoderNode::tail_lost()`, `Progress::audio_tail_lost`). This applies to every draining apply, not only a recording's stop.
6. **Video ends at the stop time; audio runs on until the drain.** Between `end_recording` and the drain, audio keeps flowing into the encoder; the file's audio may end up to that long after the last picture: normally the readbacks in flight plus one control-loop round (a few frames), at most `END_TAIL_LIMIT` plus the caller's loop latency. Players handle this; trimming audio at the end time is an open question in `recording.md`.

## Consequences

- At stop, every canvas the export took up to the moment of the stop reaches the encoder, and the AAC tail reaches the file. Frames can still be lost on the ending path as on the run path (a full staging ring `busy`, no free frame buffer, a failed readback, a mailbox replacement if the encoder misses a tick), all counted; and when the tail limit passes first, the readbacks still in flight (counted in `lost` at `release`). Stop takes a few frames longer (the readbacks in flight, about 1 to 4 frames; at most `END_TAIL_LIMIT`).
- Hot-path cost: the export's `process` gains one relaxed atomic load and one compare per tick and one relaxed store per delivered frame; the video encoder node one relaxed store per accepted frame; `take_ready` is unchanged (monomorphised); the audio reader reads the tick's flag once per passed block; the audio encoder node reads it once per tick. The export's and the video encoder's additions are accepted by argument: no benchmark resolves a few nanoseconds next to a 1080p readback (the lavapipe numbers below vary by about 0.5 ms per run). Measured (cloud container, Intel Xeon 2.8 GHz, 4 vCPU, Linux, main vs branch, alternating runs): new bench `audio_encoder_block/queue_reader_and_encoder_node` 353 / 358 ns vs 340 / 356 ns; `download_1080p/submit_take` (lavapipe) 12.80 / 12.75 ms vs 12.65 / 13.15 ms, `take_copy` 346 / 370 us vs 368 / 387 us. No change beyond the run-to-run noise.
- Tests: `take_oldest_passes_every_completed_frame_in_order` (rbs-gpu), `an_ended_export_passes_on_every_canvas_up_to_the_end_and_none_after` (rbs-nodes), `final_ticks_pass_every_queued_block_and_end_with_one_that_passes_none` (rbs-graph), `the_encoder_flushes_once_its_queued_input_ended_on_final_ticks` (rbs-audio), `ending_first_keeps_every_frame_up_to_the_end_and_the_audio_tail` (rbs-record), the studio test's second round (ends first, the last frame in the file is the last render tick at or before the end), and the app host test (Stop answers once the tail limit passes on a still clock).
- Software adapters complete readbacks within one tick in the tests, so the node test cannot force several readbacks to complete together; `take_oldest` is checked on its own for that.
- `Progress::start_drops`, `audio_flushes` and `audio_tail_lost` are now reported.
- Closing the desktop app while recording waits for the tail on the control thread, sleeping 5 ms per round, until `recording_tail_done` (bounded by `END_TAIL_LIMIT` on the master clock) or a 1 s wall-clock hang guard; with a still test clock and nothing rendering that is the guard, so host tests stop a recording before dropping the host.
- The host test covers the limit path only (nothing renders on its still clock); the export ending by itself is covered by the studio and recording tests.

## Compatibility

- New public items (additive, semver-minor before 1.0): `rbs_nodes::ExportEnd`, `VideoExportNode::end`, `ExportNodeStats::last_frame`, `EncoderNodeStats::last_frame`, `YuvDownload::take_oldest`, `rbs_record::END_TAIL_LIMIT`, `Studio::end_recording`, `Studio::recording_tail_done`, `Recording::end`, `Recording::tail_done`, `rbs_audio::EncoderNode::flushes` and `tail_lost`, `Progress::start_drops`, `audio_flushes`, `audio_tail_lost` (`Progress` is `#[non_exhaustive]`).
- Behaviour changes: every audio queue reader on a draining partition adds one final tick; every audio encoder node now flushes on a drained apply; an export released with readbacks in flight counts them in `lost`; `Studio::stop_recording` and `Recording::finish` are unchanged and still lose the in-flight frames when called without the end first (now counted).

## Alternatives considered

- **More final ticks on the render partition while the export has readbacks in flight:** helps only `rbs-cli`; in the desktop app the render partition keeps running and the export leaves without a final tick. One video pin also passes one frame per tick, so several readbacks need several ticks anyway.
- **Wait for the readbacks in the export's `release`:** the encoder has already drained by then (pipeline order), and `release` has no outputs.
- **Block in `stop_recording` until the tail is in:** tests drive a `TestClock` on the calling thread, so a blocking wait would hang them; callers already run a service loop.
- **An `AtomicBool` "ending" instead of an end time:** the stop time would depend on when the render thread sees the flag, and the final render tick (up to a period ahead) would still add a frame after the stop.
- **Flush the audio encoder on the first final tick:** pads a partial frame with silence in the middle of the tail while blocks are still queued (ADR-0043 Decision 5).
- **Have the reader publish an "end" flag the encoder reads:** a new channel between two nodes for what an empty final tick already says.

## Review

Reviewer: independent AI session (given only the ADR, the diff, principles, specs and code)
Date: 2026-11-04
Verdict: changes requested; resolved as below.
Concerns:
1. `ended` was measured at the export's output; the writer may hold the frame and the mailbox keeps only the newest -> the tail check is end to end (`last_frame` of export and encoder); Consequences list the drops that remain on the ending path.
2. `ended` ignored the tick's time -> set only on a tick past the end (test still passes; io-nodes invariant 3 says so).
3. The audio test did not prove a flush -> `Progress::audio_flushes` == 1 and `audio_tail_lost` == 0 asserted.
4. Flush packets beyond one tick and blocks after the flush were lost silently; the reader's error path did not ring -> counted in `tail_lost`; the reader rings on its error path on final ticks too; Decision 5 states the bound.
5. Audio after the video end was understated -> Decision 6 states the real bound; trimming is an open question in `recording.md`.
6. Semver and contract impact missing; `Tick::draining` doc stale -> Compatibility section; the doc says the rule holds for audio readers only (also `media-graph.md`).
7. A stop without the end lost frames invisibly -> readbacks in flight at `release` are counted in the export's `lost` (`Progress::export_drops`).
8. The app path's ended branch is untested; close blocks on wall time -> stated in Consequences; the close path stops at the master-clock limit or the 1 s guard.
9. The export's hot-path cost was not resolvable by the benchmark -> stated as accepted by argument; status stays Proposed until the owner's P15 sign-off and the SC-PC run.

The owner's P15 sign-off and the SC-PC run are recorded before the status changes.
