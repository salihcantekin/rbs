# ADR-0201: A guest share of several scenes

- Status: Proposed
- Date: 2026-11-28
- Deciders: Salih (owner)
- Extends: [ADR-0139](0139-remote-guests-rbs-to-rbs-over-webrtc.md) decisions 4, 5, 9, 11, 12, 13 and 14. It replaces two parts of ADR-0139: decision 1's "the program of one canvas" (a share may now carry fixed scenes too), and decision 15's phase-3 item "guest screen sharing as a second video track", for which this is the ADR that item asked for. It extends ADR-0107 decision 6 and ADR-0152 (which scenes count as shown). A share of one picture of the program is still ADR-0139 phase 1 as built.

## Context

ADR-0139 phase 1 sends one picture per pairing: the sharing canvas's program. The owner asked (2026-11-28) that a guest can pick several of their scenes, for example a camera scene and a screen scene, and send them together. The host then sees each as its own guest source and can place them freely in the host's scenes.

Forces:

- **Pairings.** One pairing per scene would repeat the codes, the check phrase and the NAT work for each scene. Users would paste six codes for three scenes.
- **Compositors.** A scene that is not on the sharing canvas's program has no compositor of its own. The studio composes the program, the studio-mode preview scene, the hold scene and the projector pictures (ADR-0125). The binder counts only program, preview and hold scenes as shown, so a further scene's media and browser sources would sleep (ADR-0107, ADR-0152).
- **Cost per picture.** Each picture costs one composition, one readback through the existing per-chain export (ADR-0038) and one encoder session. A canvas has at most 4 chains (`MAX_CHAINS_PER_CANVAS`) and the studio at most 8 (`MAX_CHAINS`). Hardware encoders limit sessions.
- **Outputs.** An output today has one video stream, then audio (`recording.md`).
- **Sound.** RBS has one mix per canvas. A scene that is not on the program has no sound of its own (ADR-0097's per-output mixes are not built).
- **Host limits.** The host's guest source cap (4) and its decode sessions (ADR-0087) count pictures.
- **Codes.** Codes are a contract (`peer-port.md`, version 1). Old receivers must refuse a newer code cleanly (ADR-0139 decision 5). Codes must carry no user or device names.

## Decision

1. **One pairing, several pictures.** A share carries 1 to 3 pictures in one pairing: one connection, one DTLS session, one check phrase and one congestion estimate. Each picture is its own H.264 video track, and there is one Opus sound track. A share of one picture is exactly phase 1.

2. **What a picture is.** Each picture is either *the program* or *a fixed scene* of the sharing canvas.
   - The program follows the canvas's scene switches and transitions, as in phase 1.
   - A fixed scene shows that scene whatever is on air.
   - The user picks the pictures at *Share*, in order. The same scene may not be picked twice, and at most one picture is the program.
   - Every picture uses the share's one size and bitrate setting, scaled from its canvas. Per-picture size and bitrate wait for a measured need.

3. **A compositor per fixed scene.** Each fixed scene is bound like the hold scene: a compositor for `(canvas, Role::Share(n))` that exists only while the share is live, never while it waits (P13; ADR-0139 decision 10).
   - Its visible sources count as shown at the `OnAir` level, since a remote audience sees them (extends ADR-0107 decision 6 and ADR-0152).
   - It keeps its own compositor for the life of the share, even while the same scene is also on the program. That avoids graph changes at every scene switch. Reusing the program or preview compositor is a later optimisation, and must come with a measurement.

4. **Output and slots.** A share is **one output with N video chains**: one transport, one session, one audio chain.
   - The share counts once toward `MAX_OUTPUTS` and `MAX_OUTPUTS_PER_CANVAS`, and N times toward `MAX_CHAINS_PER_CANVAS` and `MAX_CHAINS`.
   - Each video chain follows ADR-0139 decision 9: its own chain, low latency, CBR and a keyframe on request. A picture's encoder follows keyframe requests for its own track only.
   - *Share* reserves the output slot and N chain slots at once. When the canvas cannot hold them all, it refuses with `no_encoder_slot` before any code is made.
   - The ADR-0087 estimate counts each picture.
   - `recording.md` gains this output shape: video streams 0 to N-1, then audio.

5. **Bitrate.** The share's one congestion estimate is split between the pictures in proportion to their highest bitrates. Each picture's limiter follows its part (ADR-0139 decision 9: floor 300 kbit/s, one change per 500 ms, up by at most 10 % per step).
   - When the estimate is below N × 300 kbit/s, the first picture keeps its floor and the others share what is left, down to their floors. Below the sum of the floors, the later pictures stay at their floor and the network drops what it must. Pausing a picture waits for a stop signal (decision 8).
   - A track the host stopped decoding still gets its part until that signal exists.
   - The share's total highest bitrate must not pass 8 Mbit/s.

6. **Sound.** The one sound track is the sharing canvas's main mix, as in phase 1.
   - On the host, the sound plays with one named source of the pairing, the first by default. The host can move it to another source of the pairing in Properties. The other sources say so in Properties ("Sound comes with *name*").
   - When the source with the sound is removed, the sound moves to the next source in order.
   - A hidden placement is not heard (ADR-0139 decision 12), so the Properties note says that hiding that source silences the guest.
   - A per-picture mix waits for per-output mixes (ADR-0097).

7. **Code version 2.** An invite with more than one picture is code version 2 (`rbs-guest:2:`). A share of one picture still makes a version 1 code, so phase 1 receivers keep working. A version 1 receiver refuses a version 2 code with "This code is from a newer RBS" (ADR-0139 decision 5).
   - The picture count and the labels are in the `rbs-peer` envelope, not the adapter payload, so that the port checks them. `peer-port.md` gains them.
   - A version 2 answer takes all the tracks or refuses. Taking only some is left to a later version.
   - A size test shows that 3 video sections and 8 IPv6 candidates stay under the 4 KiB code limit.

8. **Labels.** Each picture has a label of at most 32 Unicode scalar values and 96 bytes.
   - The default is the neutral "Picture *n*". The scene name is used only when the sharing user picks it in the dialog, since scene names often hold a person's name and codes must not carry one without the user choosing it.
   - The host treats a label as untrusted text: control and bidirectional characters are removed, and an empty label becomes "Picture *n*".
   - Labels never go in a log line.
   - Renaming the scene later does not change the label.

9. **The host side.** Adding a version 2 invite as a guest makes one guest source per picture, in order, named after the labels, in one undo step.
   - The sources share one pairing, one jitter buffer clock and one sender-to-master map (ADR-0139 decision 11), so the pictures stay in step with each other and with the sound.
   - Each picture counts as one guest source toward the cap of 4 and as one decode session. The cap is checked for all pictures at once when the answer code is made.
   - A source whose picture no shown scene draws stops decoding that track (ADR-0139 decision 12), while the others go on.
   - Removing one source drops its track before the decoder. The pairing ends when the last of its sources is removed.
   - The sender keeps encoding a dropped track until a later stop signal exists (a known cost, see Consequences).

10. **One picture fails.**
    - **Sender.** An encoder or compositor fault on one picture restarts that chain with back-off (ADR-0024) while the other pictures go on. The share ends only when the transport or session fails.
    - **Host.** A decoder fault or a refused SPS on one track makes only that source *Unavailable* with its last picture (P7).
    - **Limits.** The ADR-0139 token bucket (`guest_limits`) applies to the whole pairing, so a peer above it closes every picture.

11. **The sending user edits scenes.**
    - **Deleted scene.** A fixed scene deleted while shared keeps its last frame for one second, then sends black. The share shows a warning, and never switches silently to another scene.
    - **Moved scene.** A scene cannot be moved to another canvas while it is shared; that is refused.

12. **Return feed.** *Share* is refused, and a live share stops, with `guest_return_feed` when the program, the bound hold scene or any fixed scene of the share draws a guest source, nested scene sources included. This is ADR-0139 decision 13 applied to every picture.

13. **Control API.**
    - `guest.share` gains `pictures?: [{scene?: id, label?}]`. A picture without `scene` is the program; without `pictures`, the share is one picture of the program.
    - `guest.status` share rows gain `pictures: [{scene?, label, width, height, bitrate_kbps}]`.
    - `guest.source.add` keeps `source` and `item` (the first picture) and adds `sources: [id]` and `items: [id]`. Guest source rows gain `pairing` and `picture` (their index).
    - The protocol minor is main's + 1 when this lands. Codes stay `admin` only (ADR-0139 decision 14).

14. **Limits.**
    - At most 3 pictures per share and 2 shares.
    - The chain pools are the real limit: 4 chains per canvas and 8 in all. One canvas can hold one 3-picture share, or a 3-picture share and a 1-picture share, and two 3-picture shares need two canvases.
    - The limits rise only with a measurement.

## Consequences

- Positive:
  - One code exchange and one phrase for several pictures.
  - The host gets each scene as its own source to place and crop.
  - The pictures stay in step, since they share a pairing and a clock.
- Negative:
  - Each fixed scene costs a composition, a readback and an encode on the sharing machine.
  - A 3-picture share uses 3 of the canvas's 4 chains, so a recording that needs its own chain may then be refused. The Share dialog must say so before the code is made.
  - A dropped track keeps being encoded until a stop signal exists.
  - Sound is per pairing, not per picture.
- Follow-up work, each its own PR with its own exit criteria:
  1. **Codes and the adapter.** Code version 2 in `peer-port.md` and the `rbs-peer` port. The adapter's offer and answer with several video tracks. Its media path with one send ring entry per track (`hot-paths.md`, hot-path review and owner sign-off, P15).
  2. **Compositors and the output.** The share role in the binder (`scene-model.md`). The compositor per fixed scene and the N-chain output (`gpu-compositor.md`, `recording.md`), with measurements: GPU and CPU time on the sending machine for a 3-picture share at 1280×720 and 30 fps, against a 1-picture share.
  3. **The studio host.** Several pictures per share, the slots and estimate, the split limiter, and the per-picture restart.
  4. **The guest source.** Several sources per pairing, and the sound moving between them.
  5. **The control API** (`control-api.md`), with an independent review (P14).
  6. **The UI.** The scene picker in the Share dialog, the label field and the Properties notes (UI rules, approval with dark and light screenshots).
- Tests, over the loopback test pairs:
  - A share of two fixed scenes and the program: track order, labels, the slot reservation and the split estimate.
  - A version 1 receiver refuses version 2 cleanly.
  - The code size bound.
  - Two pictures cut on the same frame by a scene change both pictures show stay within one frame of each other after the jitter buffer.
  - Removing the source with the sound moves it to the next one.
  - One picture's decoder fault leaves the others running.
  - Deleting a shared fixed scene gives black and a warning.

## Alternatives considered

- **One pairing per scene.** Nothing new in the protocol, but the user repeats the codes and the phrase for each scene, and the pictures drift apart, since each pairing has its own clock map. Rejected.
- **One picture with the scenes side by side, cut apart on the host.** One encoder, but a larger picture, lower quality per scene at the same bitrate, and the host's crop must match the sender's layout. Rejected.
- **Simulcast layers for the scenes.** WebRTC simulcast sends one source at several qualities, not different sources, so it does not fit. Rejected.
- **N outputs that share one session handle.** This fits today's one-video output, but it counts N outputs toward `MAX_OUTPUTS_PER_CANVAS` and needs a shared transport between outputs. Rejected for decision 4.
- **A sound track per picture now.** It needs per-output mixes, which RBS does not have (ADR-0097). Deferred.

## Review

Independent review per ADR-0014, 2026-11-28, by an AI session that got only the proposal, ADR-0139, the specs and the code. Verdict: approve with changes. Changes made:

1. The header names the ADR-0139 decisions 1 and 15 this replaces, and ADR-0107 and ADR-0152.
2. Decision 4 now picks one output with N video chains (1 output slot, N chain slots) and adds the output shape to `recording.md`.
3. Decision 14's limits are stated against the chain pools.
4. Decision 3 binds a fixed scene like the hold scene, at the `OnAir` level.
5. Decision 3 keeps one compositor per fixed scene for the share's life. Reuse is a later, measured optimisation.
6. Decision 12 also covers the hold scene and nested scenes.
7. Decision 6 gives the sound a named, movable source, and says where it goes when that source is removed.
8. Decision 7 has no contradiction any more (all tracks or refuse), puts count and labels in the envelope, and adds a size test.
9. Decision 8: neutral default labels, bounds in scalars and bytes, untrusted text on the host, kept out of logs.
10. Decision 10 covers the failure of a single picture.
11. Decision 11 covers scene edits; decision 2 says every picture uses one size.
12. Decision 5 covers the bitrate below the floors and stopped tracks.
13. Readback through the existing per-chain export, and a measurement target, are in Context and follow-up 2.
14. `guest.source.add` keeps `source` and `item`.
15. Adds a track sync test.

Owner approval: pending.
