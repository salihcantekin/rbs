# ADR-0139: Remote guests: one RBS shares its program to another over peer-to-peer WebRTC, paired by two codes, with no server of ours

- Status: Accepted
- Date: 2026-11-17
- Deciders: Salih (owner)
- Extends: [ADR-0107](0107-temporal-sources-sleep-when-hidden.md) decision 7 for `org.rbs.source.guest` only (decision 12); this is the later decision that ADR-0107 decision 7 expects
- Builds on: [ADR-0006](0006-three-tier-plugin-model.md), [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0019](0019-encode-mux-transport-ports.md), [ADR-0024](0024-node-restart-with-back-off.md), [ADR-0032](0032-fast-deterministic-tests.md), [ADR-0046](0046-network-access-over-tls.md), [ADR-0047](0047-several-outputs-per-canvas.md), [ADR-0052](0052-ai-agent-control-through-mcp.md), [ADR-0055](0055-crash-resilience.md), [ADR-0087](0087-output-cost-estimate-and-hardware-limits.md), [ADR-0090](0090-recording-protection-disk-lag-and-adaptive-bitrate.md), [ADR-0106](0106-picture-and-sound-on-one-source.md), [ADR-0107](0107-temporal-sources-sleep-when-hidden.md), [ADR-0109](0109-browser-source.md), [ADR-0127](0127-multistreaming-to-several-destinations.md)

## Context

Small production teams need remote guests ([vision](../vision.md), item 11). The roadmap puts them after 1.0 as a plugin with an SFU, roles and moderation, and ADR-0006 names remote guests in the process tier. The owner wants a first version now, with three constraints (2026-11-17):

- **No server we run.** No signalling server, no TURN relay, no hosted guest page at this stage.
- **WebRTC**, because browser guests and guest screen sharing are later goals and both need it.
- **RBS on both ends first.** The guest runs RBS, builds their own scene, presses *Share* and gets a code. The host adds a source from that code, and the guest's program arrives directly.

What exists: the output chain (encoder, optional muxer, transport; [output ports](../specs/output-ports.md)) with hardware H.264 encoders on every platform; several outputs and encoder chains per canvas (ADR-0047, ADR-0127: `MAX_CHAINS_PER_CANVAS` = 4, `NoEncoderSlot`); picture-and-sound sources (ADR-0106) and the sound aligner the browser source uses (`rbs_audio::InputAligner`); the browser runner's texture hand-off rules (ADR-0109 decision 8); typed Unavailable reasons and restart back-off (ADR-0024, ADR-0055); the control API with scopes and the agent network permission (ADR-0052); TLS on rustls with the ring provider only, enforced by `deny.toml` (ADR-0046). The specs already name a remote guest as its own source type, not OS capture ([capture port](../specs/capture-port.md), [media graph](../specs/media-graph.md)).

What does not exist: a WebRTC stack; an Opus codec; a decoder that takes packets from a live stream (`rbs-media-decode` opens files; `VideoDecoder` is listed in [backend ports](../specs/backend-ports.md) but not specified; there is no `AudioDecoder`); mix-minus; a plugin loader. The output node counts `KeyframeRequested` but does not forward it, and the encoder node has no parameter channel for `EncoderChange::Bitrate` (ADR-0047 decision 5 and ADR-0090 decision 5 call adding it a hot-path change).

WebRTC needs an offer and an answer before media flows. Without a server, both must travel through something the users already have (a chat message). A single one-way link cannot carry both. Without signalling there is also no ICE restart, and NAT mappings and ICE consent expire within tens of seconds to minutes (RFC 8445, RFC 7675), so the pairing must be timed against those limits.

## Decision

### Shape

1. **Two parts, one per side.** The sending side is a new output kind, **guest share**: the program of one canvas through a WebRTC transport. The receiving side is a new source type, **`org.rbs.source.guest`**, a picture-and-sound source (ADR-0106). One pairing carries media one way. Both parts are built in and wired by the composition roots, like the browser runner (ADR-0109 decision 1). They must not become the plugin process host, and their wire format must not enter the plugin SDK. A later plugin may replace them without a scene-format change; ADR-0006's process-tier row stays true for that plugin.

2. **Ports (P1, P2, rule 2).** No crate below the composition roots names WebRTC, ICE or a crypto library.
   - **`rbs-peer`** (port crate, no network, codec or crypto dependency) defines the pairing session and its *envelope*: format version, session id, expiry, transport kind, and the check-phrase input (decision 6). The transport-specific part of a code is an **opaque payload** owned by the adapter. Sketch: `PeerFactory::create_invite(&InviteSettings) -> PendingInvite` (gives the invite code), `PeerFactory::accept_invite(&str, &AcceptSettings) -> PendingAnswer` (gives the answer code), `PendingInvite::accept_answer(&str) -> PeerSession`, `PeerSession::{poll_events, check_phrase, close}`, and on the receive side `PeerSession::take_packets` (encoded packets with the sender's capture times). All types are `#[non_exhaustive]`; errors are typed (P9).
   - **`rbs-peer-str0m`** is the first adapter, on [str0m](https://github.com/algesten/str0m) (sans-IO, safe Rust, MIT/Apache-2.0). Its WebRTC payload is a compressed SDP, so the planned WHIP output and phase 3's browser guests reuse it; the roadmap's WHIP transport must use this same adapter, not a second WebRTC stack. Its sending half implements `Transport` (input = packets). A WebRTC transport attaches to an existing paired session: the composition root passes the session as an opaque handle in the transport options; engine code keeps no registry of sessions. It declares `transport.codecs = {h264, opus}` and `transport.bandwidth_estimate`. The overview layer table and the `check-layers` allow list name the crate as an adapter of two ports.
   - **Live decode** is `VideoDecoder` and a new `AudioDecoder` in `rbs-codec` (packet in, frame out), specified in a decode section of `backend-ports.md`. `rbs-media-decode` stays the file port; the platform crates (`rbs-media-mf`, `rbs-media-avf`) may implement both ports and reuse their decoder sessions. One port for packets, one for files, because a file port owns demuxing and seeking that a live stream does not have.
   - **Opus** is `rbs-codec-opus` (encoder and `AudioDecoder`). Its build must not need CMake (ADR-0046's build goal); the PR names the crate, its build path and the licence check.
   - **Threading.** Each session has one owner thread, created only by *Share* or *Add source › Guest* (P13), that owns the socket and the str0m instance, sends keep-alives while a code is pending, and ends with the session. `Transport::send` pushes into a bounded lock-free queue to that thread and never blocks; the queue has the shape the output-ports spec gives transports (a staging queue owned by the producer side, where every drop decision is made, then a short bounded SPSC hand-off), and its drop policy drops and cuts the GOP, then requests a keyframe. The receive side hands packets from that thread to the source's worker the same way.
   - A spec draft `docs/specs/peer-port.md` lands with the first code PR, before any public type; the code format is versioned there (decision 5).

3. **Crypto backend.** This ADR keeps ADR-0046's ban list and build goal, not its wording that TLS lives in one crate only (`rbs-transport-rtmp` already uses rustls); each PR lists the crypto crates it adds. str0m's DTLS and SRTP backend must pass `cargo deny` as it is: no `openssl-sys`, no `aws-lc-sys` (ADR-0046). The adapter uses a backend built only from Rust crates (or the platform's own crypto where str0m provides it), pinned to an exact version. If no such backend works, phase 1b stops and a new ADR that supersedes ADR-0046 for this one adapter crate is written first. The risk of a younger DTLS stack is accepted for RBS-to-RBS; phase 3 adds an interop test against a browser before browser guests ship.

### Pairing

4. **Two codes, live sessions as the authority.** *Share* creates an **invite code**; the host's *Add source › Guest* takes it and creates an **answer code**; the guest pastes the answer into their share. Codes are text (`rbs-guest:1:` followed by base64url), at most 4 KiB, with at most 8 candidates.
   - A code carries only the envelope and the payload: session id, an advisory expiry, a fresh certificate fingerprint (a new certificate per share, so it is no long-term id), ICE credentials, candidates, codecs. It must not carry a stream key, an API token, a user name or a file path.
   - The issuer's live session state is the only authority: an unknown, closed or expired session id is refused. The expiry inside a code is advisory, checked with 2 minutes of tolerance for clock skew. An answer is bound to the invite's session id and to the offerer's fingerprint. Pending invites and answers are never saved; an app restart invalidates them.
   - An invite is single-use and lives 10 minutes by default (a setting, at most 30). An answer lives 3 minutes by default (a setting, at most 10).
   - All candidates are gathered before the code is shown (no trickle ICE), bounded by a gathering timeout (default 3 s, a setting).
   - While a code is pending, its socket refreshes its STUN binding every 20 s so the server-reflexive candidate stays valid, and the answering side keeps its connectivity checks running (re-armed as needed) until the answer expires. If str0m cannot do this, the adapter does it around str0m; the first adapter PR proves it with the test in decision 16.

5. **Code format is a contract.** `rbs-guest:1:` is versioned in `peer-port.md`. A receiver refuses an unknown version with a clear message ("This code is from a newer RBS"). A new version is a minor change only when old receivers can refuse it cleanly.

6. **Check phrase.** Both sides show a phrase derived as H("rbs-guest-phrase-v1" ‖ session id ‖ offerer fingerprint ‖ answerer fingerprint), from the fingerprints DTLS authenticated, never from the code text, at least 66 bits (6 words from a 2048-word list), so a party in the middle cannot grind certificates to match it. Both sides gate on it: the share sends **no media until the sharing user confirms** that the phrases match, and the guest source **publishes nothing** (it shows *Not verified* and stays out of every mix and picture) **until the host confirms**, or the host side has turned the check off. So a party who replaced the invite and confirms its own leg still reaches nothing on the host's program. The UI tells users to compare phrases by voice or another channel than the one that carried the codes. The check is on by default; a setting on each side can skip it for that side, and the UI then says the codes' channel is the only protection.

7. **No server of ours; STUN only.** Candidates are host addresses and server-reflexive addresses from STUN. The STUN list is a user setting with a default list of public servers named in `user-data.md`; an empty list means local network only. No STUN query happens before *Share* or *Add source › Guest*. A setting leaves out private host addresses for pairings that are not on one local network. The Share dialog states that the other side learns your public IP address, and the STUN setting's help says the STUN operator does too. There is no TURN in this ADR; a TURN list is reserved in the settings format but not read until a later ADR. Failures have separate typed reasons: `guest_no_public_address` (no server-reflexive candidate), `guest_unreachable` (no candidate pair succeeded), `guest_disconnected` (consent lost).

### Media

8. **Encoder feedback is a prerequisite.** Phase 1a forwards `KeyframeRequested` to the encoder as `EncoderChange::RequestKeyframe` only for an output that owns its encoder chain (every guest share does, decision 9; ADR-0047 decision 6 and ADR-0127 decision 4 still forbid it for a shared encoder), and adds the encoder node's parameter channel for `EncoderChange::Bitrate`, as ADR-0090 decision 5 describes, with `hot-path-review`, measurements and the owner's sign-off (P15). Until it lands, no share ships. On an encoder that does not list `bitrate` in `codec.reconfigure`, the share keeps its start bitrate, uses a 1 s keyframe interval, and says "This encoder cannot follow the network; the guest may see stalls on a weak connection". NACK and retransmission (RTX) are on in all cases.

9. **The share's encoding.** Video is H.264, audio Opus 48 kHz stereo. A guest share always has its own encoder chain: the output is marked not shareable, so no other output joins it under ADR-0047 decision 3, and the existing rule that adaptive outputs get their own encoder applies. Low-latency tuning, no B-frames, a keyframe on request (rate-limited to one per second), default 1280×720 at 30 fps and 2.5 Mbit/s, at most 1920×1080 at 60 fps and 8 Mbit/s (settings). Rate control is CBR on every encoder that offers it (an encoder without CBR keeps a fixed bitrate, as in decision 8). The bitrate follows the congestion estimate between a floor of 300 kbit/s and the maximum, at most one change per 500 ms, down at once and up by at most 10 % per step. This is the share's own limiter, not ADR-0090's slower one, because no other output reads this encoder. A share counts toward `MAX_OUTPUTS`, `MAX_OUTPUTS_PER_CANVAS`, `MAX_CHAINS_PER_CANVAS` and the ADR-0087 estimate. When no chain is left, *Share* is refused with `NoEncoderSlot` before any code is made, and the UI says "All encoders on this canvas are in use. Stop a recording or stream destination to share." From *Share* until the output is attached, or until *Stop*, failure or expiry of the pairing session (not merely of the single-use invite), the share **holds** one output slot (counted in `MAX_OUTPUTS_PER_CANVAS` and `MAX_OUTPUTS`), one chain slot (`MAX_CHAINS_PER_CANVAS`) and its ADR-0087 cost, as bookkeeping only (no encoder, no partition), so other starts see them as used and a confirmed share never finds a slot taken (`TooManyOutputs` or `NoEncoderSlot` are given at *Share*, never at confirmation).

10. **Share lifecycle (P13).** A pending invite holds one socket and its keep-alives, visible in the UI as *Waiting for answer*; no encoder exists. The output is attached to the studio only when DTLS is connected and the check phrase is confirmed, or the sharing side has turned the check off. Consent loss (RFC 7675, about 30 s with no reply) is final: the output is detached, the encoder stops, and new codes are needed. *Stop* or invite expiry closes everything.

11. **The receive path.**
    - **Untrusted input.** Network I/O, depacketizing, jitter buffering and decoding run on the guest source's own worker, never on the render or audio thread. Before any data reaches a platform decoder, the worker parses SPS and PPS in safe Rust and checks every SPS and PPS in the stream, not only the first, and accepts only an allowed list: H.264 Constrained Baseline, Main or High, level at most 5.2, 4:2:0, 8-bit, at most 3840×2160 and 60 fps; every decoded picture's size must match the validated SPS. A token bucket limits the incoming rate to 50 Mbit/s with a 2 s burst; a peer above it is closed with reason `guest_limits`. A decoder failure marks only that source Unavailable (P7, ADR-0055). The guest gets no control-API access through a pairing. **Exit criterion for isolation:** before phase 3 (browser guests, where any browser can be a peer), the decode moves into a helper process like the browser runner; the port boundary in decision 2 means that move changes no contract.
    - **Sync.** Both ends are RBS, so each RTP packet carries the sender's master-clock capture time in a header extension (abs-capture-time). The worker keeps one sender-to-master map (offset plus drift) for picture and sound together and releases both from the jitter buffer by it. The sound goes through `InputAligner` paced by that release; the picture is stamped from the same map, so picture and sound reach the program at the same master time within one audio block (ADR-0106 decision 4). Until the map is valid, nothing is published. The jitter buffer target is 150 ms by default (a setting, 40 to 1000 ms).
    - **Picture hand-off (P4, P6).** ADR-0109 decision 8's rules apply: a decoded surface is imported on the studio's GPU adapter; a surface goes back to the decoder only after the studio's GPU work that reads it has finished; the render thread never waits on a lock and never samples a surface it does not hold; `process` takes the newest due picture and does not allocate, block or touch the network. A decoder that gives only system memory uploads off the render thread, counted in metrics. The guest picture and sound hand-off is added to `hot-paths.md` with the first code, and needs `hot-path-review`, measurements and the owner's sign-off (P15). So do the send path of `rbs-peer-str0m` (a transport send loop outside the `rbs-transport*` pattern, named in `hot-paths.md` in phase 1b) and the `rbs-codec-opus` encoder and decoder.

12. **The guest source's lifecycle and limits.**
    - It is capture class (ADR-0107 keeps capture sources warm and sleeps only media and browser sources): while placed and not deactivated it keeps its connection and its sound pipeline running, even when hidden (a hidden placement is not heard), because a guest who drops on every scene switch cannot be produced. While no shown scene (any canvas's program, preview or bound hold scene) draws it, its **video decode stops**: packets are dropped before the decoder, and a keyframe is requested (PLI) when it is drawn again. This extends ADR-0107 decision 7 for this one type: the binder reports the shown set for `org.rbs.source.guest` too, and the host writes it to the source's control handle, while the source stays in the capture class (warm, always linked). Installed means nothing: no socket, thread, decoder or STUN query.
    - On `guest_disconnected` the source keeps its last picture, as a browser source does (ADR-0109), and its sound fades to silence over one audio block (no click). A source that cannot recover stays Unavailable with its last picture until the user removes it or pairs it again.
    - At most four guest sources run and at most two shares run, until a measurement raises the numbers. All running guests that a shown scene draws decode; when the ADR-0087 estimate or the decoder adapter's declared session capacity allows fewer,  The guest source cap is checked when the answer code is made, before pairing, so a paired guest is never refused later. the ones a program or bound hold scene draws decode first, then source id order (as ADR-0109 decision 7), and the rest show their last picture with reason `guest_decode_cap`. The ADR-0087 estimate counts decode sessions.

13. **No return feed in phase 1.** Leaving a guest's own sound out of what is sent back to them is a per-output mix, which RBS does not have (every output takes the canvas's main mix; ADR-0097's buses are not built). So in phase 1, *Share* is refused while the canvas's program or bound hold scene draws any guest source, with reason `guest_return_feed` and the UI text "Sharing a program that contains a guest needs a return mix, which comes later.", and a share stops with that reason if such a source is drawn later. This prevents both the echo and the mirrored picture. The guest hears the host on the call they already use to compare the phrase. Return feeds come with mix-minus in phase 2.

### Control and agents

14. **Control API.** Share, accept answer, stop, reading a pending code, and adding a guest source from a code are control-API methods with the same validation as the window (ADR-0052 decision 1). Share, accept answer, reading a pending code and adding a guest source are **`admin`** methods; stop is `control`, like `stream.stop`, like `record.start_to_file`: a share sends the program to a peer the caller chooses, and the others return or consume codes. The control-api spec's rule that a `control` token cannot send the program elsewhere stands, and remote use needs "Allow remote admin". Effects (ADR-0052 decision 4): share, accept answer and stop are `live`; adding a guest source is `edit`; reading a pending code is `read`. Share, accept answer and adding a guest source open connections, so a token without "Allow actions that cannot be undone" gets `not_permitted` with reason `network` for them; stop still needs that permission because it is `live`, as ADR-0052 says. Codes are never returned to `read` or `control` tokens and never appear in events or logs. While live, each code, its ICE username fragment and its ICE password are registered with `rbs_crash::add_secret` and removed when the session ends; `rbs_crash` gains secret removal and keeps at least 32 secrets, so codes from two shares and four guests cannot push out a stream key (ADR-0055). Method names, the protocol minor and the scene-format bump are set by the implementation PR (whoever merges takes main + 1).

### Plan and checks

15. **Phases.** Each step is its own PR series with its own exit criteria. A later step that changes a contract here needs its own ADR.
    - **1a:** encoder feedback (decision 8), measured and signed off.
    - **1b (P15 for the send path and Opus):** `peer-port.md`, `rbs-peer`, `rbs-peer-str0m`, codes and check phrase, `rbs-codec-opus`, the guest share output, behind `admin` API methods and no UI.
    - **1c:** `VideoDecoder` and `AudioDecoder` with the macOS and Windows adapters, the guest source, sync, measurements (P15), a glass-to-glass latency measurement against a budget of 400 ms on one local network.
    - **1d:** the UI (Share dialog, code fields, *Add source › Guest*, status), following rule 16.
    - **2 (needs its own ADR):** mix-minus (a return feed without that guest's own sound), which changes the mixer and output model (likely on ADR-0097's buses) and lifts decision 13's refusal; return feeds to several guests (one video chain at a fixed bitrate shared by the return feeds, one audio encoder per guest, so the chain limit holds); a Linux decoder.
    - **3 (needs its own ADR):** browser guests (a page must be served from somewhere, so this reopens the "no server" constraint), guest screen sharing as a second video track, TURN, the decoder in a helper process (decision 11).
    - **Optional:** an SRT transport for RBS-to-RBS as a second adapter of `rbs-peer` (its payload is its own).

16. **Checks.** Peer tests drive two str0m sessions through an in-memory network on `TestClock`, with loss, reordering and NAT mapping expiry injected (ADR-0032); a real-socket loopback test is `slow_*` with a hang guard only. The tests: codes round-trip; expired, reused, unknown-version, oversized and wrong-session codes are refused, and so is an answer after the share restarted; clock skew within tolerance is accepted; a code never contains the stream key or a token; a DTLS fingerprint mismatch is refused; the check phrase is equal on both sides and differs under a substituted invite; no media flows before the guest confirms, and a guest source publishes nothing before the host confirms, so a substituted invite gives no picture or sound on the host's program; an answer pasted 2 minutes 50 seconds after it was made still connects, through a simulated NAT with a 30 s UDP mapping timeout and address-restricted filtering (so only the keep-alive keeps it open); nothing recovers after consent expiry and the share's output is detached; no encoder exists while an invite is pending, and its chain slot is held from *Share* to attach; a PLI gives a keyframe on the next encoded frame; the bitrate stays within its bounds under the estimate; lip sync within one audio block; no drift with a 200 ppm sender clock over one simulated hour, simulated at packet level on `TestClock` so it runs in milliseconds (otherwise named `slow_*`); nothing is created while Installed; a hidden guest keeps its session and decodes no video; a peer above the limits or with a refused SPS is closed with `guest_limits`; *Share* is refused with `guest_return_feed` while the program draws a guest, and a running share stops when one is drawn; the cap is checked at answer time and prefers program-drawn guests; `admin` methods are refused for a `control` token; an agent without the network permission gets `not_permitted`. Fuzz targets: the code parser, the H.264 RTP depacketizer, the Opus RTP depacketizer and the SPS/PPS validator.

## Consequences

- Two RBS users can put one on the other's program with no account, no server and no fee, encrypted end to end (DTLS-SRTP), with the guest's own scene, filters and hardware encoder, locked to the host's master clock. OBS ships no guest input; browser call tools give the guest no studio.
- Some pairings across the internet will fail without TURN (both sides behind symmetric NAT, some mobile and company networks). Those users see a typed reason and cannot fix it in this version. Phase 1 measures how often.
- Phase 1 has no return feed: a host whose program shows a guest cannot share back until mix-minus (phase 2).
- Pairing takes two pastes and a phrase check; a connection lost for longer than consent expiry needs new codes. Each side learns the other's public IP address.
- New dependencies: str0m and its crypto backend, an Opus crate. Each passes `cargo deny` and the PR's dependency justification. Binary size grows by a few MB.
- Hot-path work in 1a, 1b (the peer send path, Opus) and 1c needs measurements and the owner's sign-off.
- Spec follow-ups, in the PR of each step: `peer-port.md` (new); a decode section in `backend-ports.md`; `output-ports.md` (non-shareable outputs, the WebRTC transport, the Feedback paragraph corrected so `KeyframeRequested` is never forwarded to an encoder that feeds several outputs (ADR-0047 decision 6, ADR-0127 decision 4), and closing its open WHIP item on negotiating codecs before the encoder exists: codecs are fixed in the codes before any encoder is created); `recording.md` (guest share output kind, chain limits); `scene-sources.md` (the guest type, capture class, settings); `scene-model.md` (the binder's shown set for the guest type, extending ADR-0107 decision 7); `control-api.md` (the `admin` methods); `user-data.md` (STUN list, reserved TURN list, defaults); `hot-paths.md`; `performance-budgets.md` (idle check: no socket while Installed); `overview.md` layer table; glossary terms *Share*, *invite code*, *answer code*, *check phrase*, *guest*.
- `docs/vision.md` item 11 and `docs/roadmap.md` are updated with this ADR's acceptance: a first, built-in version comes before 1.0.

## Reference tools (CLAUDE.md rule 17)

- **VDO.Ninja** gives browser guests over WebRTC through its own public signalling server and a hosted page; in OBS the guest enters as a browser source, decoded in Chromium and copied again. **OBS Studio** has no guest input; its WHIP output only sends. **vMix Call** and **Riverside** route through their own servers and need accounts.
- **Where this falls short:** no browser guests, no relay for hard networks, no roles or moderation, two pastes instead of one link.
- **What RBS does by this decision:** no third-party server or account in the media path, a guest with a full studio and hardware encoding, picture and sound locked to the host's clock by the sender's capture time, a hidden guest that stays connected but decodes no video, and typed reasons when a pairing fails.

## Alternatives considered

- **A hosted guest page and signalling server (VDO.Ninja style).** One link and browser guests on day one, but it needs a server, a domain and HTTPS hosting, which the owner ruled out for now. It is phase 3.
- **A single link with an open port (UPnP or NAT-PMP).** One paste, but it fails on many routers and opens a port on the user's router. Codes work wherever ICE works.
- **A public free broker (MQTT, a public relay) for signalling.** One link, but the session depends on a third party we do not control.
- **SRT instead of WebRTC.** Already a dependency on the send side and simpler, but browser guests and screen sharing need WebRTC anyway, and SRT does not adapt the bitrate to the network. Kept as an optional second adapter.
- **Both SRT and WebRTC in phase 1.** Roughly one to two more weeks and every feature tested twice; deferred.
- **libwebrtc (Google).** Complete, but a very large C++ build with its own toolchain, against P11 and build-speed goals.
- **webrtc-rs.** A port of Pion with its own async runtime and threads; str0m's sans-IO design fits RBS's own threading and test clock.
- **A plugin first (ADR-0006 process tier).** There is no plugin loader; waiting for it would delay guests past 1.0. The port boundary keeps the plugin path open.
- **Decoding in a helper process from phase 1.** Safer, but it adds a second process design before the first guest works. Phase 1 peers are other RBS users holding a code, the SPS validator and fuzzing guard the decoder, and the move is required before browser guests.

## Review

Independent review per ADR-0014.

### Round 1

Reviewer: independent AI session
Date: 2026-11-17
Verdict: changes requested
Concerns and resolutions (author, 2026-11-17):
1. Encoder feedback (PLI, bitrate) does not exist; ADR-0047/0090 make it a hot-path change -> decision 8: phase 1a prerequisite with P15; fallback for encoders without bitrate change.
2. Share and accept send the program elsewhere, which the control-api spec reserves for `admin` -> decision 14: `admin` methods, codes never returned to other scopes, never logged, registered as secrets.
3. Return feeds in phase 1 make an audio and picture feedback loop -> decision 13: the answering peer's sound is left out of a share; headphones prompt; test.
4. A short phrase from one fingerprint does not stop a party in the middle -> decision 6: H(session ‖ both fingerprints), at least 66 bits, compared by another channel, no media before confirmation, fresh certificate per share.
5. NAT mapping expiry, check retransmission limits and consent expiry make the timing infeasible -> decision 4: STUN keep-alive every 20 s, answer expiry 3 min, checks kept running; decision 10: consent loss is final; tests in decision 16.
6. Expiry enforced by the wrong party, replay rules incomplete -> decision 4: live session state is the authority, advisory expiry with skew tolerance, answer bound to session and fingerprint, nothing persisted, size and candidate bounds.
7. In-process decode of network input against ADR-0006 -> decision 11: SPS/PPS allow list in safe Rust, token bucket, fuzz targets, helper process required before phase 3.
8. str0m's OpenSSL and aws-lc backends fail `deny.toml` (ADR-0046) -> decision 3: backend must pass `cargo deny` as is, or a superseding ADR first; Opus build without CMake.
9. Second decode port duplicates `rbs-media-decode`; no `AudioDecoder` -> decision 2: live decode in `rbs-codec` (`VideoDecoder`, `AudioDecoder`), file port unchanged, adapters may implement both; trait sketch and `peer-port.md` first; spec in `backend-ports.md`.
10. GPU hand-off unspecified -> decision 11: ADR-0109 decision 8 rules, upload off the render thread, `hot-paths.md` entry.
11. A/V sync underspecified -> decision 11: abs-capture-time, one sender-to-master map for both, nothing published before it is valid; drift test.
12. Encoder sharing and chain limits -> decision 9: non-shareable output, `NoEncoderSlot` before any code with UI text; decision 12: estimate counts decode sessions; phase 2 return feeds share one video chain and needs its own ADR.
13. Hidden guests still decode; cap order -> decision 12: video decode stops while not drawn, PLI on show; cap checked at answer time, program-drawn first.
14. Share lifecycle not exact -> decision 10.
15. Transport creation for a session-bound transport -> decision 2: opaque session handle from the composition root, layer table and allow list, declared capabilities.
16. Port bakes in ICE and DTLS -> decision 2: envelope in the port, opaque adapter payload (compressed SDP), WHIP reuses the adapter.
17. IP leakage understated -> decision 7.
18. Unsourced failure figure, no latency target, one failure reason -> figure removed (phase 1 measures it), 400 ms local budget in 1c, three typed reasons.
19. Disconnect behaviour differs from the browser source -> decision 12: last picture kept, sound fades over one block.
20. Real-socket test may flake; tests missing -> decision 16.
21. Phase 1 too large; code format is a contract -> decision 15 splits 1a to 1d; decision 5.

### Round 2

Reviewer: independent AI session
Date: 2026-11-17
Verdict: changes requested
Round 1 resolutions: 18 of 21 verified; #3, #4 and #12 not resolved; #1, #2, #13 and #15 left gaps.
Concerns and resolutions (author, 2026-11-17):
1. The return-feed match used a fingerprint that decision 4 makes new per share -> 2.
2. Leaving one source out of one output is a per-output mix that does not exist -> decision 13: phase 1 has no return feed; *Share* is refused (`guest_return_feed`) while the program or hold scene draws a guest; mix-minus is phase 2.
3. The phrase gated only the guest's side -> decision 6: the guest source publishes nothing until the host confirms; phrase from DTLS-authenticated fingerprints with a domain tag; per-side skip; test.
4. The chain slot was not held between *Share* and attach -> decision 9: bookkeeping hold of the slot and its estimate cost; test.
5. Keyframe forwarding to shared encoders; bitrate policy -> decision 8: only for an output owning its chain; decision 9: CBR, 300 kbit/s floor, own limiter, encoders without CBR fixed.
6. Scopes and effects of the code methods; secrets -> decision 14: all but stop `admin`, effects declared, permission named, ICE fragments registered, secret removal and at least 32 secrets.
7. Shown set for a capture-class type extends ADR-0107 -> decision 12 says so and defines shown scenes; cap order uses program or hold.
8. Session threading and ownership -> decision 2, Threading.
9. Hot-path coverage of the peer send path and Opus -> decisions 11 and 15, Consequences.
10. ADR-0046's "only stack" wording -> decision 3.
11. Skip setting wording -> decision 6.
12. Every SPS and PPS; size match; depacketizer fuzz targets -> decisions 11 and 16.
13. Test runtime and NAT model -> decision 16.
14. Open WHIP item in `output-ports.md` -> Consequences.

### Round 3

Reviewer: independent AI session
Date: 2026-11-17
Verdict: approve with changes
Round 2 resolutions: all 14 verified in the Decision text, no conflict with accepted ADRs.
Concerns and resolutions (author, 2026-11-17), all applied:
1. The hold covered no output slot -> decision 9 holds an output slot too (`MAX_OUTPUTS_PER_CANVAS`, `MAX_OUTPUTS`).
2. Hold end tied to the single-use invite -> decision 9: until attach, *Stop*, failure or expiry of the pairing session.
3. Attach and publish when the check is skipped -> decisions 6 and 10.
4. Network refusal and scopes of stop and code reading -> decision 14.
5. Wrong output-ports citation -> decision 2, Threading: staging queue plus SPSC hand-off, producer-side drops.
6. ADR-0107 extension not recorded -> header and Consequences (`scene-model.md`).
7. output-ports Feedback paragraph conflicts with ADR-0047 d6 -> Consequences.
8. Decode limit not named -> decision 12 (`guest_decode_cap`).

### Owner approval

Salih, 2026-11-17, in the project thread: "Evet bunun ADR'ını yaz ve kabul et." ("Yes, write this ADR and accept it."), given before the review rounds, on the plan of RBS-to-RBS WebRTC with two codes and no server. Consequence to note for the owner: phase 1 has no return feed (decision 13).
