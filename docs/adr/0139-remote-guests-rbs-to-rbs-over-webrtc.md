# ADR-0139: Remote guests: one RBS shares its program to another over peer-to-peer WebRTC, paired by two codes, with no server of ours

- Status: Proposed
- Date: 2026-11-17
- Deciders: Salih (owner)
- Builds on: [ADR-0006](0006-three-tier-plugin-model.md), [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0019](0019-encode-mux-transport-ports.md), [ADR-0047](0047-several-outputs-per-canvas.md), [ADR-0052](0052-ai-agent-control-through-mcp.md), [ADR-0055](0055-crash-resilience.md), [ADR-0106](0106-picture-and-sound-on-one-source.md), [ADR-0107](0107-temporal-sources-sleep-when-hidden.md), [ADR-0109](0109-browser-source.md)

## Context

Small production teams need remote guests ([vision](../vision.md), item 11; target users table). The roadmap puts them after 1.0 as a plugin with an SFU, roles and moderation, and ADR-0006 names remote guests in the process tier. The owner wants a first version now, with three constraints (2026-11-17):

- **No server we run.** No signaling server, no TURN relay, no hosted guest page at this stage.
- **WebRTC**, because browser guests and guest screen sharing are later goals and both need it.
- **RBS on both ends first.** The guest runs RBS, builds their own scene, presses *Share* and gets a code. The host adds a source from that code, and the guest's program arrives directly.

What exists: the output chain (encoder, optional muxer, transport; [output ports](../specs/output-ports.md)) with hardware H.264 encoders on every platform; several outputs per canvas (ADR-0047); picture-and-sound sources (ADR-0106) and the sound aligner the browser source uses (`rbs_audio::InputAligner`, ADR-0109 decision 6); typed Unavailable reasons and restart back-off (ADR-0024, ADR-0055); the control API with the agent network permission (ADR-0052). The specs already name a remote guest as its own source type, not OS capture ([capture port](../specs/capture-port.md), [media graph](../specs/media-graph.md)).

What does not exist: a WebRTC stack, an Opus codec, a live (non-file) video decoder (`rbs-media-decode` opens files only; `VideoDecoder` is listed in [backend ports](../specs/backend-ports.md) but not specified), a mix-minus feed, and a plugin loader (`rbs-plugin-host` loads nothing).

WebRTC needs an offer and an answer before media flows. Without a server, those must travel through something the users already have (a chat message). A single one-way link cannot carry both.

## Decision

1. **Two parts, one per side.** The sending side is a new output kind, **guest share**: an output of one canvas's program through a WebRTC transport. The receiving side is a new source type, **`org.rbs.source.guest`**, a picture-and-sound source (ADR-0106). One pairing carries media one way. A return feed is a second pairing in the other direction (the host shares, the guest adds a source). Both are built in and wired by the composition roots, like the browser runner (ADR-0109 decision 1). They must not become the plugin process host, and their wire format must not enter the plugin SDK. A later plugin may replace them without a scene-format change.

2. **Pairing by two codes.** *Share* creates an **invite code**; the host's *Add source › Guest* takes it and creates an **answer code**; the guest pastes the answer into their share, and media starts. Each code is text the users copy through any channel (`rbs-guest:1:` followed by base64url). It must carry only: the format version, a random session id, an expiry, the DTLS certificate fingerprint, ICE credentials, the gathered ICE candidates, and the offered or accepted codecs. It must not carry a stream key, an API token, a user name or a file path. All candidates are gathered before the code is shown (no trickle ICE), bounded by a gathering timeout (default 3 s, a setting). An invite code is single-use and expires (default 10 minutes, a setting); a second answer to one invite is refused. The code channel is the trust channel: whoever holds the invite can answer it, so the share shows the answering peer's certificate fingerprint as a short check phrase on both sides.

3. **No server of ours; STUN only.** Candidates are host addresses and server-reflexive addresses from STUN. The STUN server list is a user setting with a default of public servers; an empty list means local network only. STUN reveals the user's public address to that operator, and the setting's help text says so. There is no TURN in this ADR. When ICE fails (both peers behind symmetric NAT, a blocking firewall), both sides report **Unavailable** with reason `guest_unreachable` and the guest is not shown. A TURN server list is reserved in the settings format but not read until a later ADR.

4. **Media.** Video is H.264, the only codec the first version offers (every RBS platform encodes it in hardware, and browsers decode it). Audio is Opus, 48 kHz, stereo. The guest share always has **its own encoding**, never one shared with a recording or stream (ADR-0047), because WebRTC changes its bitrate at run time: low-latency tuning, no B-frames, a keyframe on request (PLI/FIR), default 1280×720 at 30 fps and 2.5 Mbit/s, at most 1920×1080 at 60 fps and 8 Mbit/s (settings, per the feature parameters rule). The bitrate follows the stack's congestion estimate (REMB or TWCC) within those bounds; an encoder that cannot change bitrate while running keeps its start bitrate and the share says so. A guest share counts toward `MAX_OUTPUTS` and the ADR-0087 estimate like any output.

5. **Ports and adapters (P1, P2).** WebRTC is an adapter. A new port crate **`rbs-peer`** (no network or codec dependency) defines the pairing session (create invite, accept invite, accept answer, events, close) and the receive side (encoded packets with sender timestamps). The first adapter is **`rbs-peer-str0m`** on [str0m](https://github.com/algesten/str0m) (sans-IO, safe Rust, MIT/Apache-2.0); its sending half also implements the existing `Transport` port (input = packets) so the output builder treats it like RTMP. Opus is **`rbs-codec-opus`** (libopus, BSD-3-Clause). Live video decode is the `VideoDecoder` port in `rbs-codec`, specified by this ADR's spec follow-up, with platform adapters first (Media Foundation, VideoToolbox; Linux follows with VA-API or an optional software adapter). No vendor or peer kind is named in engine code; the source asks for capabilities (`video.decode.h264`, `peer.transport`).

6. **The receive path.** Network I/O, depacketizing, jitter buffering and decoding run on the guest source's own worker, never on the render or audio thread. The jitter buffer target is 150 ms by default (a setting, 40 to 1000 ms). The worker maps the sender's RTP timestamps to one sender clock from RTCP sender reports, then to master time at the arrival edge with the same aligner the browser sound uses, so picture and sound reach the program at the same master time within one audio block (ADR-0106 decision 4). Decoded pictures go to the studio as GPU textures when the decoder produces them (P4), otherwise through the existing upload path; `process` takes the newest due picture and must not allocate, block or touch the network (P6). The consume path needs `hot-path-review` and measurements when it lands (P15).

7. **Lifecycle (P13).** Installed means nothing: no socket, thread, decoder or STUN query. A guest source is a capture-class source (ADR-0107 keeps capture sources warm and sleeps only media and browser sources): while it is placed and not deactivated it keeps its connection, even when hidden, because a guest who drops on every scene switch cannot be produced. Removing or deactivating it closes the connection. A guest share exists only from *Share* until *Stop* or invite expiry with no answer. ICE consent checks keep a live connection; when the path breaks, the source is Unavailable with reason `guest_disconnected`, keeps its last picture for 2 seconds, then shows nothing, and recovers by itself if ICE recovers. A connection that cannot recover needs new codes; there is no server to renegotiate through.

8. **Untrusted input.** Everything from the network is untrusted. The adapter must refuse a stream above 3840×2160 or 60 fps, or above 50 Mbit/s for 5 seconds, and close it with reason `guest_limits`. Decoding uses the platform decoders, and a decoder failure marks only that source Unavailable (P7, ADR-0055). The guest gets no control-API access of any kind through a pairing; the pairing carries media and the WebRTC control messages, nothing else. If a later measurement or audit finds in-process decoding of peer streams too risky, the receive half moves to a helper process like the browser runner; the port boundary in decision 5 is drawn so that move changes no contract.

9. **At most four guest sources Active, at most two guest shares running.** Further sources report Unavailable with reason `guest_cap` until a measurement raises the numbers (the ADR-0109 decision 7 pattern; source id order decides).

10. **Control API and agents.** Share, accept answer, stop, and adding or changing a guest source are control-API methods with the same validation as the window (API parity, ADR-0052 decision 1). A guest source type opens network connections, so an agent without the network permission gets `not_permitted` (reason `network`) for adding one or for creating a share. Method names, the protocol minor and the scene-format bump are set by the implementation PR (whoever merges takes main + 1).

11. **Phases.** Each phase is its own PR series; a later phase that changes a contract here needs its own ADR.
    - **Phase 1:** `rbs-peer` port, str0m adapter, Opus adapter, `VideoDecoder` port with the macOS and Windows adapters, guest share output, guest source, codes, control API. One guest, one direction at a time (a return feed by a second pairing works but carries the full program).
    - **Phase 2:** mix-minus (a guest's return feed without that guest's own sound), up to the decision 9 caps, a Linux decoder, measurements.
    - **Phase 3 (needs its own ADR):** browser guests (a guest page has to be served from somewhere, so this phase reopens the "no server" constraint), guest screen sharing as a second video track, and TURN.
    - **Optional:** an SRT transport for RBS-to-RBS (SRT rendezvous mode also needs no server), as a second adapter of the same port.

12. **Checks.** With a test double of `rbs-peer` and no network: invite and answer codes round-trip and reject expired, reused, wrong-version and oversized codes; a code never contains the stream key or a token; lip sync of a stamped picture and its sound within one audio block; nothing is created while Installed; a hidden guest source keeps its session; a peer exceeding the decision 8 limits is closed with `guest_limits`; a disconnect keeps the last picture for exactly 2 s on the injected clock (ADR-0032); the caps prefer lower source ids; an agent without the network permission gets `not_permitted`. A loopback test of the str0m adapter (two sessions on 127.0.0.1) runs as a normal test, with no STUN.

## Consequences

- Two RBS users can put one on the other's program with no account, no server and no fee, end to end encrypted (DTLS-SRTP), with the guest's own scene, filters and hardware encoder. That is beyond what OBS ships (no built-in guest input) and what a browser call gives (the guest has a full studio).
- About 10 to 20 percent of pairings across the internet will fail without TURN (symmetric NAT on both sides, some mobile and company networks). Those users see `guest_unreachable` and cannot fix it in this version.
- Pairing takes two copy-paste steps, and a broken connection needs new codes.
- New dependencies: str0m (and its DTLS and crypto backend), libopus. Each lands with its own `cargo deny` check and the PR's dependency justification. Binary size grows by a few MB.
- The receive path adds a decode per guest and an upload when the decoder does not give GPU textures; measured in phase 1 (P10, P15).
- Spec follow-ups in the implementation PRs: a new `peer-port.md` spec; `VideoDecoder` in `output-ports.md` (or a decode section of `backend-ports.md`); `scene-sources.md` (the guest type, capture class, settings); `recording.md` / `output-ports.md` (the guest share output kind); `control-api.md`; `user-data.md` (STUN list, reserved TURN list, defaults); `performance-budgets.md` (idle check: no socket while Installed); glossary terms *Share*, *invite code*, *answer code*, *guest*. `docs/vision.md` item 11 and `docs/roadmap.md` note that a first, built-in version comes before 1.0 by this ADR.
- The UI (Share dialog, code fields, Add source › Guest, the source's status) follows rule 16: proposal and dark and light screenshots before merge.
- ADR-0006's process-tier row stays true for a later plugin; this ADR is a built-in like the browser runner.

## Reference tools (CLAUDE.md rule 17)

- **VDO.Ninja** gives browser guests over WebRTC with its own public signaling server and a hosted page; the guest's picture enters OBS as a browser source, so it is decoded in Chromium and copied again. **OBS Studio** has no guest input; its WHIP output only sends. **vMix Call** and **Riverside** route through their own servers and need accounts.
- **Where this falls short:** no browser guests, no relay for hard networks, no roles or moderation, two pastes instead of one link.
- **What RBS does by this decision:** no third-party server or account in the media path, a guest with a full studio and hardware encoding, the guest's picture and sound locked to the host's master clock, and a guest source that is a native source (no browser in between) with typed reasons when it fails.

## Alternatives considered

- **A hosted guest page and signaling server (VDO.Ninja style).** One link and browser guests on day one, but it needs a server, a domain and HTTPS hosting, which the owner ruled out for now. It is phase 3.
- **A single link with an open port (UPnP or NAT-PMP).** One paste, but it fails on many routers and opens a port on the user's router. It may be an extra path later; codes work wherever ICE works.
- **A public free broker (MQTT, a public relay) for signaling.** One link, but the session depends on a third party we do not control.
- **SRT instead of WebRTC.** Already a dependency on the send side and simpler, but browser guests and screen sharing need WebRTC anyway, and SRT does not adapt the bitrate to the network. Kept as an optional second adapter.
- **Both SRT and WebRTC in phase 1.** Roughly one to two more weeks and every feature tested twice; deferred.
- **libwebrtc (Google).** Complete, but a very large C++ build with its own toolchain, against P11 and build-speed goals.
- **webrtc-rs.** A port of the Pion stack with its own async runtime and threads; str0m's sans-IO design fits RBS's own threading and test clock better.
- **A plugin first (ADR-0006 process tier).** There is no plugin loader; waiting for it would delay guests past 1.0. The port boundary keeps the plugin path open.

## Review

Independent review per ADR-0014. Record: reviewer, date, verdict, main concerns and how each was resolved.
