# ADR-0088: Replay buffer: keep the last minutes in memory, save a clip, or turn the buffer into a recording

- Status: Accepted
- Date: 2026-11-09
- Deciders: Salih (owner)
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0019](0019-encode-mux-transport-ports.md), [ADR-0020](0020-node-contract-and-schedule-swap.md), [ADR-0026](0026-partitions-and-queues.md), [ADR-0041](0041-studio-recording-on-a-running-engine.md), [ADR-0045](0045-recording-tail-on-stop.md), [ADR-0047](0047-several-outputs-per-canvas.md), [ADR-0050](0050-command-registry-menus-and-toolbar.md), [ADR-0060](0060-shortcuts-in-app-and-global.md), [ADR-0087](0087-output-cost-estimate-and-hardware-limits.md)
- Spec: [recording](../specs/recording.md), [io-nodes](../specs/io-nodes.md), [output-ports](../specs/output-ports.md), [control-api](../specs/control-api.md), [shortcuts](../specs/shortcuts.md)

## Context

A very common loss: something great happens and the user was not recording, or forgot to start. The owner asked for a way to keep the recent past and, when it matters, say "save the last 5 minutes". OBS has a replay buffer (a rolling save of the last seconds, mainly for clips); it does not turn the buffer into a recording that continues, it keeps its buffer in memory only, and its length is limited by RAM without any warning of the cost. The aim here is the same safety net with better behaviour, at the lowest possible cost.

Facts that shape the design:

- Raw frames cannot be buffered. 1080p60 NV12 is about 186 MB/s; five minutes would be roughly 56 GB. The buffer must hold **encoded packets**: at 20 Mbit/s five minutes is about 750 MB, at 60 Mbit/s about 2.2 GB, plus audio. Memory is the main cost, with a running encoder session as the second.
- Packets carry master-clock timestamps and keyframe flags (media types), a muxer can write packets without re-encoding, and a packet queue that starts begins at a keyframe (ADR-0047), so clips can be cut from packets exactly at keyframes.
- An output chain can already share the video and audio encoder with another output when their settings match (ADR-0047); `MAX_OUTPUTS = 2` is the limit of file/network outputs.
- Zero cost when unused (P13): a disarmed buffer must add no thread, memory or encoder session.

## Decision

1. **Armed/disarmed.** The replay buffer is off by default. *Arm* is a user action (a registry toggle, `replay.toggle`, a Live controls button, optionally *Arm when RBS starts*, a shortcut); *Disarm* releases everything. Disarmed = Installed: no node, no memory, no session. Armed = Active.
2. **What it is.** A new output kind of the `Studio`: a **packet ring** sink fed by a video encoder and (if the setting says so) an audio encoder, in the output partitions, never the render or audio partitions. It is a distinct kind, not one of the two `MAX_OUTPUTS` slots (that limit means record and stream); at most one replay buffer per canvas. Its encoder is **shared** with a recording or stream whose settings match (ADR-0047), so while recording with matching settings the buffer costs only the memory; otherwise it opens its own session, which ADR-0087 counts. An unshared buffer needs an encoder partition; with two outputs running none may be free (ADR-0047), so arming is then refused with that reason, or the buffer shares.
3. **Settings (all user-visible, saved in the output profile; none hard-coded).** `length` (10 seconds to 30 minutes, default 2 minutes), `memory_limit` (default 1 GiB, bounded by the estimate), quality mode (*Match the recording*, default, or its own `VideoSettings`/`AudioSettings`), `keyframe_interval` (in *Match the recording* mode it is the recording's, because an encoder is shared only when every setting including the keyframe interval is equal (ADR-0047), so a clip is cut within the recording's interval; in its own mode the default is 1 s), `audio` on/off, container for saved clips (ADR-0085), file name template, folder, *Arm when RBS starts* (off), *Keep armed while live* (on). If the memory limit is reached before `length`, the buffer holds less, and the UI says exactly so ("Holds the last 3:12, limit: memory").
4. **Save a clip.** Command `replay.save` (registry, shortcut, global shortcut allowed by ADR-0060, Live controls button, control API `replay.save {seconds?}`): the clip covers the last `seconds` (default: the full length). It starts at the latest keyframe at or before that point (so a clip can be up to one keyframe interval longer than asked, never shorter; the exact bounds are in the file name or report) and ends at the moment of the command. The packets are written by the muxer as they are, **without re-encoding**, by a short-lived writer: a throwaway output graph (muxer plus file transport) on a worker thread, off every real-time path, which rebases timestamps so the clip's earliest sample is 0 and starts at a keyframe; saving takes a moment and does not interrupt capture, the buffer or any output. Audio and video are cut at the same master-clock instant. Result: a fragmented file in the chosen container that is valid even if the app dies during the write, a toast with the file name and *Show in folder*, and the event `replay.saved`.
5. **Retroactive recording: "record from the buffer".** Command `record.start {include_buffer: seconds}` / the Record button's menu item *Record and include the last N minutes*: the new recording's file begins with the buffer's packets (from the keyframe at or before the requested point) and then continues live from the same encoder with no gap, one continuous file. It needs the buffer's encoder settings to equal the recording's (*Match the recording* mode); if they differ, the command is disabled with the reason. This is what answers "I forgot to press Record".
6. **Real-time safety.** The ring node's `process` only moves packets into a preallocated ring (capacity reserved at arm time from `length` and the rates, with headroom; if the capacity is exceeded the oldest packets are dropped first, never the newest, and no growth allocation happens in `process`). Evicted packets are handed to a small reaper thread that frees them, so no payload is freed in `process`. Under memory pressure (ADR-0090) the ring only evicts earlier; its capacity never grows or is reallocated. A save request reaches the node as an atomic value set by the control thread; the node cuts the clip into a vector reserved at arm time (shared packet references: reference-count increments, no copy of data) and returns it through a bounded channel with `try_send`; the control thread supplies the next vector through a second one. The node takes no lock (neither the `try_lock` exception of ADR-0023 is needed): `try_send` and `try_recv` on bounded channels never wait or allocate. This code is a new node type and is on the hot-path list: before/after measurements with 0, 1 and 2 other outputs, the `hot-path-review` checklist and Salih's sign-off (P15). The measured quantities are the output partition's tick time, the encoder's packet latency and the memory per second of buffered video.
7. **Cost shown first.** The estimate (ADR-0087) includes the buffer: its memory (rate times length) and its encoder session (none when shared). Settings › Recording › Replay buffer shows "About 750 MB of memory for 5 minutes at this quality" and the level before it can be armed; arming a buffer that is `over` is refused with the reasons.
8. **Limits, stated plainly.** The buffer lives in RAM: if the app or the computer crashes, the buffered past is lost (the live recording is not affected). A disk-backed ring (rotating segment files, which would survive a crash and allow longer buffers but write continuously to the SSD) is an open follow-up, with its own wear and space warning, not part of this decision. Playing a clip back on air as a source (slow motion, "instant replay" in the broadcast sense) is a separate feature.
9. **Control API and events** (next free minor version): `replay.status` (armed, held duration, bytes, limit reason), `replay.arm`, `replay.disarm`, `replay.save`, `record.start` parameter `include_buffer`, events `replay.state` and `replay.saved`. Class `control`; saving writes only inside the configured folder (a path argument is `admin`, like `record.start_to_file`).
10. **UI** (visual: proposal first, then screenshots in three themes, rules 15 and 16): a Replay card in Settings › Recording, an arm toggle and a Save button in Live controls (also registry commands usable on the toolbar, ADR-0050), an armed indicator that follows the live-indicator rules (U4, U12) without being mistaken for On air, and the Record button menu entry of decision 5.

## Consequences

- A user who forgot to record can recover the last minutes as a clip or as the start of a recording, at the price of RAM and, in the non-shared case, one encoder session.
- The memory cost is visible before arming and its limit is explicit; nothing is hidden or unbounded.
- Cutting at keyframes means a clip can be slightly longer than asked; the 1 s default keyframe interval keeps that small.
- A new sink node and a new `Studio` output kind are added; `recording.md`, `io-nodes.md`, `control-api.md`, `shortcuts.md` and the glossary change in the implementing PRs.
- Crash safety of the past is not provided in v1 (decision 8).

## Alternatives considered

- **Buffer raw frames.** Tens of gigabytes; not possible.
- **Always record to disk and trim later.** Survives crashes but writes constantly to the SSD and fills the disk; offered by the open follow-up as an explicit mode, not as the default.
- **Treat the buffer as a third output slot.** Raising `MAX_OUTPUTS` mixes meanings and costs a second session where sharing makes it free.
- **Re-encode the clip on save.** Slower and lossy; cutting at keyframes needs no encoder.
- **Cut exactly on the requested frame.** Needs re-encoding of the first group of pictures; not worth it for the default.

## Review

- 2026-11-09, independent AI session (round 1, reviewing only the ADRs, specs and code): changes requested. Concerns covered contract details, claims about existing code, hot-path and zero-cost rules, and number allocation across these ADRs; each was resolved in the text above (the numbers of protocol minors and scene formats are now assigned at merge time, not claimed by ADRs).
- Round 2 (verification by a different fresh reviewer) was still open when the owner accepted the ADR.
- 2026-11-10, implementation of the ring node (PR 2): decision 6 changed from the parameter channel to an atomic request plus bounded channels, so no lock of any kind runs in `process` (see the text above).
- 2026-11-10: Owner (Salih) accepts this ADR. The host and control API already on main may be ahead of the remaining decision text.
