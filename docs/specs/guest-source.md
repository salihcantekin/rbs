# Guest source

- Status: Draft (2026-11-28), revised after its first independent review (below). It is not a contract until a second review approves the revision and the owner accepts it.
- Decisions: [ADR-0139](../adr/0139-remote-guests-rbs-to-rbs-over-webrtc.md) decisions 1, 2, 6, 11, 12 and 14 (phase 1c).
- Owner crates:
  - `rbs-peer` (port): the receiving half, `PeerReceiver`.
  - `rbs-sources` (`guest/`): the worker logic, which covers the rate limit, the time checks, the clock map, the jitter buffer and the gate.
  - `rbs-studio-host`: the guest pairing and the `guest.source.*` methods.
  - `rbs-peer-str0m` (adapter).
- Related specs:
  - [peer-port.md](peer-port.md): pairing and the media path.
  - [backend-ports.md](backend-ports.md): `VideoDecoder` and `AudioDecoder`.
  - [scene-sources.md](scene-sources.md) and [control-api.md](control-api.md).

## What it is

`org.rbs.source.guest` is a picture-and-sound source (ADR-0106). Its picture and sound come from another RBS's guest share, over one pairing. It is capture class (ADR-0107): it stays warm while placed and not deactivated, and its sound runs even while it is hidden. A hidden placement is not heard.

**Lifecycle (P13).**

| State | Holds |
| --- | --- |
| Installed | Nothing: no socket, thread, decoder or STUN query. |
| Standby | Unpaired. The source is in the collection but has no pairing: after a load, after undo of a removal, or after its pairing ended. It shows *Unavailable* ("Pair again") with its last picture if it had one, as a copy the source owns. No thread, socket, decoder or aligner. |
| Pairing | The answering session (a socket and its keep-alives), from `guest.source.add` or `guest.source.pair` until connected. No worker yet. |
| Active | Connected: the worker, a decoder while it is shown, the Opus decoder, the aligner. |

A pairing is never saved. Undo never brings back an old pairing, only a Standby source.

**Registration (rule 19).** The type is registered through the existing closed lists: `SourceClass::Capture` and `rbs_api::SourceKind::Guest`, with no new `match` on type ids in the engine or the host. Its descriptor sets a new flag, `carries_peer_program`. The share's return-feed check reads that flag instead of the type id (control-api.md 1.116, open point). Saved settings, as an `rbs-params` schema:

- `name`
- `jitter_ms`: 40 to 1000, default 150
- `check_phrase`: default on
- `local_only`: default off

`scene-sources.md` gains the entry in the same change.

## The receiving half in the port

The guest source reaches the pairing only through the port, so a plugin adapter can serve it.

```rust
#[non_exhaustive] pub enum FrameTrack { Video, Audio }   // H.264 access unit (Annex B) | one Opus packet

#[non_exhaustive] pub struct ReceivedFrame {
    pub track: FrameTrack,
    pub data: Arc<[u8]>,
    pub rtp_time: u64,              // the track's clock, extended past wrap
    pub capture_nanos: Option<u64>, // the sender's master time (abs-capture-time)
    pub keyframe: bool,
    pub contiguous: bool,           // false after a loss
}
impl ReceivedFrame { pub fn new(track, data, rtp_time, capture_nanos, keyframe, contiguous) -> Self }

pub trait PeerReceiver: Send {
    fn poll(&mut self) -> Option<ReceivedFrame>; // never blocks
    fn request_keyframe(&self);                  // a PLI
    fn confirmed(&self) -> bool;                 // both gates open on this side
    fn ended(&self) -> bool;
    fn overflow(&self) -> u64;                   // frames dropped before poll
}

// On PeerSession, a provided method (additive, a minor change to the port):
fn take_receiver(&mut self) -> Option<Box<dyn PeerReceiver>> { None }
```

- **One receiver per session.** `take_receiver` gives `Some` once on an answering session. It gives `None` after that, and always on an offering session. The host takes the receiver and hands it to the worker, so nothing downcasts on the receive path. A conformance case checks this.
- **No arrival time in the frame.** The worker stamps each frame's arrival with its own injected clock (`rbs_core` `Clock`, `TestClock` in tests) when it polls the frame. The receive ring adds at most one worker wait to the stamp, and the clock map's minimum filter absorbs that.
- **The receiver never ends the pairing.** The worker sends a reason (`guest_limits`) to the host on the source's control handle. The host calls `PeerSession::close` and shows the reason in `guest.status`.
- **Depacketizing** (str0m's RTP reassembly and decryption) runs on the adapter's owner thread. The worker handles everything after it: every check of the bytes, the times and the parameter sets, and the decoders.
- **Adapter.** `rbs-peer-str0m`'s `GuestReceiver` implements the trait (`confirmed` is its link state `Live`). The adapter counts received bytes before its ring, and that count reaches the worker with `overflow`.

## The worker

**When it runs.** Each Active guest source has one worker thread, `rbs-guest-<n>`. It starts when the session connects, never while the answer is pending, and it ends at disconnect, at the end of the pairing or at removal. When it ends, its decoders and its aligner are released.

**How it waits.** It waits until the next due release or decode deadline, or 10 ms, whichever comes first. A wake hook on the port, so a new frame wakes it at once, is a later, measured change.

**Each frame, in order:**

1. **Gate.** While `confirmed()` is false, every frame is dropped and counted. This is a second gate behind the adapter's. Until this side's user confirms the phrase (or turned the check off), the source shows *Not verified* and stays out of every mix and picture.
2. **Rate limit.** A token bucket allows 50 Mbit/s with a 2 s burst (12.5 MB), counted on frame bytes, plus 1500 bytes for each frame `overflow` grew by since the last turn. That way a flood the adapter's ring drops still reaches the limit. A frame over the bucket ends the pairing with `guest_limits`.
3. **Frame kinds.** Only `FrameTrack::Video` (H.264) and `FrameTrack::Audio` (Opus) are taken. Any other track a newer adapter adds is dropped and counted.
4. **Times.** Each track checks its frames against their RTP times (`guest/timing.rs`). The sender derives RTP time from the capture time, so the two must agree within 20 ms and move forward. A frame without a capture time gets one from its RTP time and the last good frame.
   - An outlier is dropped and counted. For video, it also counts as a loss (step 7).
   - 50 outliers in a row end the pairing with `guest_limits`.
   - A mapped time more than 1 s from the frame's arrival is an outlier too, once the map is valid.
5. **Parameter sets.** Every SPS and PPS of every access unit is parsed in safe Rust (`rbs_codec::h264`) before any decoder sees it. A refused set ends the pairing with `guest_limits`. The allowed list:
   - Constrained Baseline, Main or High;
   - level at most 5.2;
   - 4:2:0, 8 bit luma and chroma;
   - at most 3840×2160.

   The 60 fps limit is enforced on the measured rate of capture times: more than 61 video frames in any second of sender time is a refusal. The VUI frame rate may be missing, so it is not used.

   This needs, in `rbs_codec::h264` and `backend-ports.md`, in the node's change: chroma bit depth in `Sps`; a PPS check (at most 256 bytes, ids in range, a PPS that names a known SPS); and its fuzz target.

   A decoded picture whose size differs from the last valid SPS's cropped size is dropped and counted, and a keyframe is requested.
6. **Clock map** (`guest/clock_map.rs`). It maps the sender's capture time to this side's master clock, one map for picture and sound together. The map is `gap = offset + drift × (sender − at)`, refitted once per second of sender time:
   - **Drift** is a least-squares line through the per-second minimum gaps of the last 60 completed seconds. Minimums more than 10 ms above a first fit are left out. It is bounded to ±500 ppm.
   - **Offset** is the median of the last 10 seconds' minimums, carried to now along the drift, so one far-off second does not move it.
   - **Slewing.** The map in use moves toward a new fit by at most 1 ms per second. A change above 50 ms is a resync: it is applied at once, counted and logged. So a route change does not jump the release times, and `InputAligner` keeps its own map.
   - **One correction only.** The aligner sees frames already on the master clock at a steady rate, so it does not correct the drift a second time. Its drift steering only follows its own block cadence.
   - **Validity.** The map is valid after one whole second of sender time. Until then nothing is published.
   - The map follows offset plus the least network delay. The jitter target sits on top of that.
7. **Jitter buffer** (`guest/jitter.rs`, one per track). A frame waits until `mapped time + target` (the `jitter_ms` setting).
   - **Sound** leaves in capture order.
   - **Video** leaves in arrival order, which is decode order. A video frame whose capture time does not move forward is refused, since RBS sends no B-frames.
   - **Late frames.** A frame more than one target late is dropped.
   - **Video losses.** Any lost video frame makes the track drop frames until the next keyframe and request one, at most once per second. That covers a gap, a late frame, a frame dropped for room and an outlier, so the decoder never misses a reference.
   - **Bounds.** At most 2 s of sender time, 600 frames and 12.5 MB are held. Above that, the oldest frames are dropped (for video, up to the next keyframe held).
8. **Decode.** A video frame goes to the decoder at its release time minus a decode lead: 1.5 times the decode time measured over the last 30 frames, at least 5 ms and at most the target. The decoder is `VideoDecoder`, with the studio's adapter and native memory kinds (`DecodeOutput`).
   - **Stamps.** Each decoded picture is stamped with its mapped master time plus the target.
   - **Sound.** A sound frame goes to the Opus `AudioDecoder` at its release.
   - **Concealing.** A missing stretch of sound, measured in RTP time with each packet's length read from its TOC byte, is concealed in 120-sample multiples up to 100 ms. A longer stretch resets the decoder, inserts silence and restarts the aligner's map, and is counted.
9. **Hand-off.**
   - **Pictures.** Pictures go into a three-slot queue that the source's node reads on the render thread. `process` takes the newest picture whose stamp is due, as ADR-0139 says, under ADR-0109 decision 8's rules. A surface goes back to the decoder only after the GPU work that reads it has finished.
   - **Sound** goes through a sound ring to the source's sound node, paced by `InputAligner` at the mapped time.
   - So picture and sound meet at the same master time within one audio block.
   - This hand-off is a hot path (`hot-paths.md`). It needs `hot-path-review`, measurements and the owner's sign-off before merge (P15). Those measurements include each decoder adapter's decode latency.

**Counters.** All drops and refusals are counted per cause. The counts are summarised once a minute in a log line, while any is non-zero. They are never logged per frame.

## Shown or not

The binder reports the shown set for `org.rbs.source.guest` too: every canvas's program, preview and bound hold scene. This is ADR-0139 decision 12, an extension of ADR-0107 decision 7. The host writes the set to the source's control handle.

- **Not shown.** Video frames are dropped before the decoder. The decoder is released after 2 s not shown. That hysteresis is added to `backend-ports.md` invariant 10, in the node's change.
- **Shown again.** A keyframe is requested and a new decoder is made. The node shows the last picture until the first new one.
- **Sound** always runs.

## Limits and the decode cap

- **Cap of 4.** At most 4 guest sources pair or run at once: Pairing and Active sources count, Standby ones do not. The cap is checked when the answer code is made, so a paired guest is never refused later (`conflict`, `data.reason` `guest_cap`).
- **Decode cap.**
  - The ADR-0087 estimate counts actual decode sessions (shown guests), not guest sources.
  - When the estimate or the decoder's declared session capacity allows fewer decoders than the shown guests need, the guests a program or bound hold scene draws decode first, then in source id order. The rest show their last picture with reason `guest_decode_cap`.
  - The cap is evaluated again when the shown set changes and when an output starts or stops.
  - A guest keeps its decoder until a guest of higher rank needs it, so two equal guests never swap.

## Failure and end

- **Disconnected** (`guest_disconnected`, or `ended()`).
  - The source keeps its last picture, and its sound fades to silence over one audio block.
  - It goes to Standby: the worker, the decoders and the aligner are released.
  - It stays *Unavailable* until it is paired again or removed.
- **Decoder fault.** It marks only this source *Unavailable* (P7, ADR-0055). The worker makes a new decoder under ADR-0024's restart back-off, and asks for a keyframe after each new one.
- **Remove.** Removing the source closes the pairing and ends the worker.

## Secrets

- **Codes.** While a pairing is live, the answer code, its ICE username fragment and its ICE password are registered with `rbs_crash::add_secret`, and removed when the pairing ends. The adapter does this, as for shares.
- **The `invite` parameter** of `guest.source.add` and `guest.source.pair`:
  - It is never kept in the undo history, the settings or a log line.
  - It never appears in the API's request log or in MCP transcripts that RBS writes. The method table marks the field secret, so the request log redacts it.
- **Labels and phrases** are not secrets, but they are never logged.

## Control API (the protocol minor after 1.116; the merger sets it)

- **`guest.source.add`** (`admin`, `edit`, network): `{invite, scene?, name?, check_phrase?, local_only?, jitter_ms?}` → `{source, item, answer_expires_at}`.
  - It checks the invite and the cap, makes the answer, and adds the source to `scene` (default: the current scene), all in one undo step.
  - A bad invite is `invalid_params` with the code reason ids of 1.116. The cap is `conflict` with `guest_cap`.
- **`guest.source.pair`** (`admin`, `edit`, network): `{source, invite}` → `{answer_expires_at}`. It pairs a Standby source again.
- **`guest.source.answer`** (`admin`, `read`): `{source}` → `{answer, expires_at}`. The answer is a secret: shown, copied, never logged.
- **`guest.source.confirm`** (`admin`, `live`): `{source}` → `{state}`.
- **`guest.status`** gains `sources: [{source, state, phrase?, reason?, expires_at?, width?, height?, decoding}]`.
  - `state` is one of `unpaired`, `gathering`, `waiting_for_guest`, `connecting`, `check_phrase`, `live`, `failed` or `closed`.
  - `guest.changed` carries both lists.
- **Removing** a guest source is `source.remove`, as for any source.

## Logging

One `remote` line per state change, with the source id, the state and the reason id. Counters are summarised as above. A line never holds a code, a phrase, an address or a label.

## Tests

All tests use fakes of `PeerReceiver`, `VideoDecoder` and `AudioDecoder`, and the injected clock. No socket, no GPU.

- **Rate limit.** The burst passes, then 50 Mbit/s; a sustained rate above it is refused. `overflow` growth counts toward the limit.
- **Times.**
  - A far-future, backward or unanchored capture time is an outlier, and a missing one is derived.
  - 50 outliers in a row refuse the peer.
  - More than 61 video frames in a second refuse it.
- **Parameter sets.** An SPS or PPS outside the allowed list is refused before the decoder. A size change without a new SPS drops the picture.
- **Clock map.**
  - Within 2.5 ms of offset plus the least delay after 10 s, under 40 ms of random jitter.
  - It follows a 200 ppm drift over one simulated hour within 1 ms, under 2 ms of jitter.
  - A 20 ms route change slews at 1 ms per second, and a 2 s step is one resync.
  - One far-off second does not move it.
  - Nothing is published until it is valid.
- **Jitter buffer.**
  - Sound leaves in capture order at the target; video leaves in arrival order, and a backward video frame is refused.
  - Late frames are dropped.
  - Any video loss waits for a keyframe and asks for one at most once per second.
  - Time, frame count and bytes are bounded.
- **Sound.** Concealment stops at 100 ms on an `rtp_time` jump.
- **Gate.** Nothing is published while `confirmed()` is false, even when the fake receiver hands frames over.
- **Worker wakes.** No worker runs while the answer is pending: a count of wakes on the injected clock is zero. The worker and decoder are released after a disconnect.
- **Shown or not.** A source that is not shown decodes nothing, and asks for a keyframe when shown again.
- **Decode cap.** It prefers sources a program draws, and does not swap equal guests.
- **Track kinds.** A frame of another track kind is dropped.
- **Fuzz.** The SPS and PPS parsers, and a worker turn fed random frames and times.
- **Port.** A conformance case: `take_receiver` gives `Some` once on an answering session and `None` on an offering one.

## Implementation status

2026-11-28, branch only, not merged:
- This spec.
- The port's `PeerReceiver`, `ReceivedFrame` and `PeerSession::take_receiver`, and the str0m adapter's implementation.
- Pure, tested worker logic in `rbs-sources` `guest/`: `TokenBucket`, `TrackTiming`, `ClockMap` and `JitterBuffer`.

The decoders are another thread's work (`backend-ports.md`). The node, the hand-off, the worker loop and the host side come after this spec is accepted and the decoders exist.

## Review

First independent review, 2026-11-28, by an AI session given the draft, ADR-0139, the specs and the code. Verdict: changes requested. Changes made:

1. The receiver has `confirmed()`, so the worker's second gate is possible.
2. The worker reports `guest_limits` to the host, which closes the session.
3. The receiver is taken from the session (`take_receiver`), not from the factory.
4. The port types are `#[non_exhaustive]` and `ReceivedFrame` has a constructor.
5. Arrival is stamped with the worker's injected clock, not an `Instant`.
6. The clock map is one fit, with a median offset and slewing. Resyncs, the test bound and the relation to `InputAligner` are stated.
7. Untrusted times are checked against RTP time, outliers are counted and refused, missing times are derived, and the jitter buffer is bounded by frames and bytes.
8. Video keeps decode order, and any video drop waits for a keyframe.
9. Concealment is capped at 100 ms, and packet lengths are read from the TOC byte.
10. The parameter-set check names what `h264.rs` must add (chroma depth, PPS check). The 60 fps limit is measured from the capture rate.
11. The picture hand-off has a decode lead, stamped pictures and a three-slot queue, and the node picks the newest due picture.
12. The worker starts at connect and ends at disconnect, it waits until deadlines, and depacketizing is named as the adapter's.
13. Ring overflow counts toward the rate limit.
14. Standby is defined, `guest.source.pair` added, the cap's counting stated, and the states `unpaired` and `waiting_for_guest` added.
15. The secrets section is added.
16. Registration, the settings schema, the `scene-sources.md` follow-up and the `carries_peer_program` flag are added.
17. Decoder release hysteresis, decode-cap counting, re-evaluation and stability, ADR-0024 back-off and what is shown while waiting for a keyframe are stated.
18. The tests are extended.

Not part of this spec: the reviewer noted a garbled sentence in ADR-0139 decision 12. It is left for an in-place fix with a `## Review` note (rule 9).
