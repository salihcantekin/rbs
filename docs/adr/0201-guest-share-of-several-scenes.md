# ADR-0201: A guest share of several scenes

- Status: Proposed
- Date: 2026-11-28
- Deciders: Salih (owner)
- Extends: [ADR-0139](0139-remote-guests-rbs-to-rbs-over-webrtc.md) decisions 4, 5, 9, 11, 12, 13 and 14. It changes no decision of ADR-0139 for a share of one scene, which stays phase 1 as built.

## Context

ADR-0139 phase 1 sends one picture per pairing: the sharing canvas's program. The owner asked (2026-11-28) that a guest can pick several of their scenes, for example a camera scene and a screen scene, and send them together, so the host sees each as its own guest source and can place them freely in the host's scenes.

Forces:

- One pairing per scene would repeat the codes, the check phrase and the NAT work per scene. Users would paste six codes for three scenes.
- A scene that is not on the sharing canvas's program has no compositor of its own. The studio composes the program, the studio-mode preview scene and the projector pictures (ADR-0125). A further scene needs its own compositor while it is shared.
- Each picture costs one composition, one readback (ADR-0038) and one encoder session. The canvas has at most 4 chains (`MAX_CHAINS_PER_CANVAS`), and hardware encoders limit sessions.
- RBS has one mix per canvas. A scene that is not on the program has no sound of its own (ADR-0097's per-output mixes are not built).
- The host's guest source cap (4) and decode sessions (ADR-0087) count pictures.
- Codes are a contract (`peer-port.md`, version 1). Old receivers must refuse a newer code cleanly (ADR-0139 decision 5).

## Decision

1. **One pairing, several pictures.** A share carries 1 to 3 pictures in one pairing: one connection, one DTLS session, one check phrase, one congestion estimate. Each picture is its own H.264 video track; there is one Opus sound track. A share of one picture is exactly phase 1.

2. **What a picture is.** Each picture is either *the program* (it follows the sharing canvas's scene switches and transitions, as phase 1) or *a fixed scene* of that canvas (it shows that scene whatever is on air). The user picks them at *Share*, in order. The same scene may not be picked twice, and at most one picture is the program.

3. **A compositor per fixed scene.** A fixed scene that is not on the program is composed by its own scene compositor while the share is live, like the studio-mode preview compositor. It must not exist before the share is live (P13: a waiting share holds a socket only, ADR-0139 decision 10). When the fixed scene is also on the program, the program compositor's output must be reused, not composed twice.

4. **Encoding and slots.** Each picture has its own encoder chain under ADR-0139 decision 9 (own chain, low latency, CBR, keyframe on request). *Share* reserves one output slot and one chain slot per picture, and refuses with `no_encoder_slot` before any code when the canvas cannot hold them all. The ADR-0087 estimate counts each picture. A picture's encoder follows keyframe requests for its own track only.

5. **Bitrate.** The share's one congestion estimate is split between the pictures in proportion to their highest bitrates, and each picture's limiter (ADR-0139 decision 9: floor 300 kbit/s, one change per 500 ms, up by at most 10 %) follows its part. The share's total highest bitrate must not pass 8 Mbit/s.

6. **Sound.** The one sound track is the sharing canvas's main mix, as in phase 1. On the host, it plays with the first picture's guest source; the other sources of the pairing are pictures only and say so in Properties ("Sound comes with *name*"). A per-picture mix waits for per-output mixes (ADR-0097).

7. **Code version 2.** An invite with more than one picture is code version 2 (`rbs-guest:2:`); its payload adds the picture count and, per picture, a label of at most 32 characters (the scene name the user sees, which the sharing user may edit before the code is made). A share of one picture still makes a version 1 code, so phase 1 receivers keep working. A version 1 receiver refuses a version 2 code with "This code is from a newer RBS" (ADR-0139 decision 5). The answer names the tracks it takes; it must take all of them.

8. **The host side.** Adding a version 2 invite as a guest makes one guest source per picture, in order, named after the labels, in one undo step. They share one pairing, one jitter buffer clock and one sender-to-master map (ADR-0139 decision 11), so the pictures are in step with each other and with the sound. Each picture counts as one guest source toward the cap of 4 and as one decode session; the cap is checked when the answer code is made, for all pictures at once. A source whose picture no shown scene draws stops decoding that track (ADR-0139 decision 12) while the others go on. The pairing ends when the last of its sources is removed; removing one source drops its track before the decoder. The sender keeps encoding a dropped track until a later signal for it exists (a known cost, see Consequences).

9. **Return feed.** *Share* is refused, and a live share stops, with `guest_return_feed` when the program or any fixed scene of the share draws a guest source (ADR-0139 decision 13 applied to every picture).

10. **Control API.** `guest.share` gains `pictures?: [{scene?: id, label?}]` (no `scene`: the program); absent, one picture of the program. `guest.status` share rows gain `pictures: [{scene?, label, width, height, bitrate_kbps}]`. `guest.source.add` returns `sources: [id]` and `items: [id]`. The protocol minor is the one main has + 1 when this lands. Codes stay `admin` only (ADR-0139 decision 14).

11. **Limits.** At most 3 pictures per share and 2 shares, so at most 6 sending chains, still capped by `MAX_CHAINS_PER_CANVAS` per canvas. The limits rise only with a measurement.

## Consequences

- Positive: one code exchange and one phrase for several pictures; the host gets each scene as its own source to place and crop; the pictures stay in sync, since they share a pairing and a clock.
- Negative: each fixed scene costs a composition, a readback and an encode on the sharing machine. A 3-picture share uses 3 of the canvas's 4 chains, so a recording that needs its own chain may then be refused; the Share dialog must say so before the code is made. A dropped track keeps being encoded until a stop signal exists.
- Sound is per pairing, not per picture.
- Follow-up work, each its own PR with its own exit criteria:
  1. Code version 2 in `peer-port.md` and the `rbs-peer` port; the adapter's offer and answer with several video tracks; its media path with one send ring entry per track (`hot-paths.md`, hot-path review and owner sign-off, P15).
  2. The scene compositor for a fixed scene (`gpu-compositor.md`, `recording.md`), with measurements.
  3. The studio host: several pictures per share, slots and estimate, the split limiter.
  4. The guest source: several sources per pairing.
  5. The control API (`control-api.md`), with an independent review (P14).
  6. The UI: scene picker in the Share dialog, the label field, the Properties note (UI rules, approval with dark and light screenshots).
- Test: a share of two fixed scenes and the program over the loopback test pairs, checking track order, labels, the slot reservation, the split estimate and that a version 1 receiver refuses version 2 cleanly.

## Alternatives considered

- **One pairing per scene.** Nothing new in the protocol, but the user repeats the codes and the phrase per scene, and the pictures drift apart, since each pairing has its own clock map. Rejected.
- **One picture with the scenes side by side, cut apart on the host.** One encoder, but a larger picture, a lower quality per scene at the same bitrate, and the host's crop must match the sender's layout. Rejected.
- **Simulcast layers for the scenes.** WebRTC simulcast sends one source at several qualities, not different sources; it does not fit. Rejected.
- **A sound track per picture now.** Needs per-output mixes, which RBS does not have (ADR-0097). Deferred.

## Review

Independent review per ADR-0014: pending.
