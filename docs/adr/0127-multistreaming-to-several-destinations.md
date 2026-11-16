# ADR-0127: Multistreaming: one stream to several destinations, one shared encoder, each destination isolated

- Status: Proposed
- Date: 2026-11-16
- Deciders: Salih (owner)
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0047](0047-several-outputs-per-canvas.md), [ADR-0051](0051-user-data-profiles-and-scene-collections.md), [ADR-0087](0087-output-cost-estimate-and-hardware-limits.md), [ADR-0093](0093-vertical-second-canvas-from-the-same-sources.md), [ADR-0102](0102-output-size-distinct-from-canvas.md), [ADR-0111](0111-vertical-outputs-start-with-the-main-ones.md), [ADR-0124](0124-pause-recording-and-hold-stream.md)
- Spec: [output-ports](../specs/output-ports.md) (RTMP transport), [recording](../specs/recording.md) (Studio), [control-api](../specs/control-api.md), [ui-design-system](../specs/ui-design-system.md) (Settings, Stream), [ui glossary](../ui/glossary.md)

## Context

Streamers send one show to several services at once (YouTube, Twitch, Kick, Facebook, X). OBS cannot do this by itself: users add the obs-multi-rtmp plugin, which opens a second encoder per destination by default, or pay a relay service such as Restream, which adds a hop, latency and a monthly fee. RBS can do it natively and cheaply.

What exists:

- A studio runs up to `MAX_OUTPUTS_PER_CANVAS` (2) outputs per canvas, `MAX_OUTPUTS` (4) in all (ADR-0047, ADR-0093 decision 6): in practice one recording and one stream per canvas. Each output has its own output partition (its own worker), so a stalled transport stalls only its own queue. Encoder chains (export + encoder) are shared between outputs whose `VideoSettings`/`AudioSettings` are equal, and a new output joins a running encoder at its next keyframe. Partitions are added at studio start and get a worker only while something is scheduled on them ([ADR-0027](0027-engine-loop.md)), so unused slots cost nothing.
- An output node owns one `Transport`. Transports (`rbs-transport-rtmp`, `rbs-transport-srt`) never block in `send`, run their own I/O thread, reconnect with a `ReconnectPolicy`, drop whole GOPs when their queue (8 MiB / 3 s by default, packets are shared references, not copies) overflows, and report `Connected`/`Reconnecting`/rejection per session.
- The output profile has one `stream` section per canvas (`stream`, `stream-vertical`): one server, one key in the OS credential store (`stream-key/<profile id>`), one encoding, auto-record, output size, hold settings.
- `rtmps://` is refused everywhere (RTMP adapter, app, CLI). Facebook Live accepts only RTMPS; Kick's and X's ingest URLs are RTMPS; YouTube offers both. `rustls` 0.23 (ring) is already in the tree for `rbs-api-ws`.
- Control API 1.67 is on main: `stream.start/stop/status`, `stream.state`, `stream.hold/resume`, `canvas: "all"`. `stream.start` takes no URL or key, so a `control` token cannot redirect a stream.

Forces: encoding is the dominant cost of a stream (P13, rule 17); a failing or slow service must never stop the others; a user's quality settings are theirs (ADR-0047); hot paths change only with numbers and sign-off (P15); unused features cost nothing (rule 10).

Not in scope: signing in to platform accounts (OAuth), setting titles, reading chat, creating YouTube broadcasts through their API. That is a later, separate decision. Multistreaming needs none of it: every service accepts a push to a server URL with a stream key the user copies from the service's dashboard, and no RBS component runs in the background for it.

## Decision

### Model

1. **A stream has destinations.** Each canvas's stream (`stream`, `stream-vertical`) keeps one encoding and gains a list of up to `MAX_DESTINATIONS_PER_CANVAS` (6) destinations. A destination is `{ id, name, service, server, key_saved, enabled, reconnect, srt_latency_ms?, encoding: "shared" | own Encoding + output size }`. `id` is a stable `u32` within the profile. The existing `server`/`key_saved`/`reconnect`/`srt_latency_ms` fields of the `stream` section are destination 0, so an existing profile is a one-destination stream and reads unchanged. Destinations 1 and up live in a new section `stream-destinations` (`stream-destinations-vertical`) with its own `format_version = 1`, because the existing sections deny unknown fields in older builds (as ADR-0111 did for its flags). Each destination's key is in the credential store under `stream-key/<profile id>/<destination id>` (destination 0 keeps today's account), read only at start, on the control thread.
2. **Each destination is its own studio output.** Going live starts one `Studio` output per enabled, configured destination: its own `OutputId`, output node, transport and output partition. Nothing new is added to the graph or to a node to fan out: one encoder's `packets` pin already feeds several links, each with its own packet queue (ADR-0047). The per-canvas limit becomes one recording plus `MAX_DESTINATIONS_PER_CANVAS` streams; `Partitions` gains enough output partitions for that, and encoder partitions for the recording, the shared stream chain and up to `MAX_OWN_ENCODINGS_PER_CANVAS` (2) destinations with their own encoding. These partitions have no worker until used ([ADR-0027](0027-engine-loop.md)). `TooManyOutputs` keeps its meaning past the limits.

### Encoding

3. **One shared encoder by default; own encoding by choice.** A destination with `encoding = "shared"` uses the canvas stream's encoding, so all such destinations share one export, one video encoder and one audio encoder by ADR-0047 decision 3 (their settings are equal by construction). The host starts the first destination, then the others; each later one joins at the encoder's next keyframe (at most one keyframe interval, 2 s by default, before it sends). A destination with its own encoding (another bitrate or output size, ADR-0102) gets its own export and encoder chain, which costs a second readback and encoder session, and Settings says so before the user picks it. Two own encodings with equal settings share with each other by the same rule. The estimate (ADR-0087) counts every session before Go live; a hardware session limit moves the extra chain to the next candidate encoder, as for any refused open.
4. **A shared encoder serves its destinations alike** (ADR-0047 decision 6). A destination's `KeyframeRequested` after a reconnect is not forwarded to a shared encoder; the destination resumes at the next natural keyframe. Adaptive bitrate (ADR-0090), when it is built for streams, acts on a chain and so on every destination sharing it; per-destination adaptation needs an own encoding.

### Isolation, state and reconnect

5. **One destination's failure is its own.** A destination that is refused at start (no key, unsupported scheme, the server rejects the key), gives up reconnecting, or fails its output node is stopped with `stop_output` and marked failed with its reason; every other destination, the shared encoder and the recording are untouched. A failure of a shared encoder chain fails every destination using it (as in ADR-0047 decision 8) and is reported once, as the stream's failure.
6. **Per-destination state.** Each destination has a state `starting | connecting | live | reconnecting | failed | stopped`, its counters (connects, reconnect attempts, bytes sent per second, GOPs dropped by its transport queue), its error text with the URL and key scrubbed, and its time on air. The canvas's stream is *live* while at least one destination is `live` or `reconnecting`. It ends when the user ends it, or when every destination has failed; that end follows today's rules for a failed stream (an auto-started recording keeps running).
7. **Go live and End stream act on the stream.** Go live starts every enabled and configured destination of the canvas (and of the vertical canvas under ADR-0111's flag). It succeeds when at least one destination starts; the others report why not, per destination, and are shown failed. A destination that is enabled but not configured (no server or no key) is skipped with a notice before going live. End stream (which asks, U4) stops all destinations of the canvas in one draining apply, so the shared encoder is flushed for each.
8. **Start or stop one destination while live.** A destination can be started (joining the shared encoder at its next keyframe), retried after it failed, or stopped while the others run. Stopping one destination takes it off air on that service, so the UI asks in the frame (U4). Enabling or disabling it in Settings while live changes only the next Go live.
9. **Reconnect stays per transport.** Each destination has its own `ReconnectPolicy` (on by default: 1 s doubling to 30 s, 10 attempts; a rejected key is not retried). There is no group reconnect and no cross-destination priority: a slow uplink shows up as GOP drops in each affected destination's own queue. Settings shows the upload the stream needs: the sum of the video and audio bitrates of every enabled destination, since each sends its own copy.

### RTMPS

10. **The RTMP adapter publishes to `rtmps://`.** The adapter declares `rtmps` in `transport.schemes`. The TLS session (rustls 0.23, ring provider, TLS 1.2 and 1.3, SNI from the host, default port 443) runs on the transport's I/O thread only; `send` and the hand-off are unchanged. Certificates are verified against the operating system's trust store through `rustls-platform-verifier` (a new dependency of the adapter crate only, never of a port or core crate; it honours the roots a company or user installs, as browsers do, and needs no bundled list that ages). A certificate that does not verify is a rejection: not retried, reported with the reason, never bypassed by a setting. The app's and the CLI's URL checks accept `rtmps://`. The I/O loop becomes generic over a plain or TLS stream; that is a transport send loop (hot path, P15): the RTMP `send` bench and a plain RTMP run are measured before and after, and RTMPS's CPU is measured on Mac and SC-PC.

### Service presets

11. **A built-in list, data only.** A preset is `{ id, name, servers: [{ label, url }], server_from_dashboard: bool, key_help, limits }` in a TOML file compiled into `rbs-app` (no network access, no background check). `limits` are what the service publishes for H.264/AAC ingest (maximum video bitrate, keyframe interval, maximum size and frame rate, audio sample rates), each with the date it was checked. First list: YouTube (RTMPS and RTMP servers), Twitch (automatic ingest server), Kick (server from the dashboard, RTMPS), Facebook Live (RTMPS server), X (server from Media Studio, RTMPS), Custom. A preset fills the server field, which stays editable, and sets the destination's name; a service whose ingest URL is per account (Kick, X) shows an empty server with the help text. Limits only warn: Settings and Go live say "Twitch: 8000 kbit/s is above the 6000 kbit/s Twitch accepts" before the stream starts; RBS never changes a user's setting by itself. Presets are data, not engine code (rule 4): the engine never sees a service name.

### Interactions

12. **Hold (ADR-0124).** Hold stream and Break act on the canvas's stream as a whole: the hold gate sits in the shared export and audio encoder, so every destination sharing them holds together, and the host holds own-encoding chains of the same canvas at the same tick. Holding one destination alone is not offered (it would need a gate in the output node, a hot-path change); a later decision can add it. A destination started during a hold joins held.
13. **Recording.** Unchanged. The recording shares the stream's encoder only when settings match (ADR-0047); "Record when the stream starts" starts with the first destination and stops when the stream ends, not when one destination fails. Pause recording on a shared chain works as ADR-0124 decision 5 says, whatever the number of destinations.
14. **Vertical (ADR-0093, ADR-0111).** The vertical canvas has its own destination list and encoding (for example YouTube Shorts and TikTok-style services in 9:16 while the main canvas goes to Twitch). "Go live with main" starts both lists. A destination belongs to one canvas; sending one canvas to a destination of the other is not offered.

### Control API 1.68 (additive, number final at merge)

15. - `stream.destinations {canvas?}` (scope `read`): `[{ id, name, service, canvas, enabled, configured, encoding: "shared" | "own" }]`. Never the server URL or the key.
    - `stream.destination.start {id}`, `stream.destination.stop {id}` (scope `control`, effect `live`, on-air permission check): start or stop one destination; start while the canvas's stream is not live starts the stream with that destination only. `not_found` for an unknown id, `conflict` when it is already in the asked state, still ending, or not configured.
    - `stream.destination.set_enabled {id, enabled}` (scope `control`, not undoable): whether Go live uses it next time.
    - `stream.state` and `stream.canvas_state` add `destinations: [{ id, name, service, state, connects, reconnect_attempts, kbps, dropped_gops, on_air_ms, error? }]`; the existing `connected` means "at least one destination connected". An older client sees what it sees today.
    - No method sets a server or a key: a `control` token still cannot redirect a stream (the rule of protocol 1.1). Editing destinations through `settings.set` is a later minor version.
    - Every method is a registry command (U20), so the RBS menu, shortcuts, Stream Deck and MCP get "Start / stop destination <name>" and "Use destination <name>".

### UI (visual approval before merge, rule 16)

16. Mock-ups in dark and light go to the owner before any UI code; after implementation, screenshots in dark and light; merge after approval. Proposed shape, within the design system and the compact rule:
    - **Settings, Stream:** a Destinations card above the encoding: one `CompactRow` per destination with a `Toggle` (use on Go live), the name, the service, "Key saved" / "No key yet", the canvas only when a vertical canvas exists, and an edit button; "Add destination" under the list (disabled at the limit, with the reason). The edit dialog: Service (`Select` of the presets and Custom), Server (prefilled, editable), Stream key (as today), Encoding ("Same as the stream" / "Own settings", with the cost notice), Reconnect, SRT latency for `srt://`, `DialogActions`. Under the card: the upload the stream needs, and each preset's limit warnings.
    - **While live:** the live indicator reads LIVE when every enabled destination is on air and "LIVE 2/3" otherwise, in the warning colour while one is reconnecting or failed. Clicking it opens a Destinations popover: one row per destination with a state dot, kbps, drops and time, and Retry or Stop for that row. A destination that drops or gives up shows a toast naming it; the others are not mentioned.
    - **Glossary:** *Destination* (a service or server a stream is sent to), *Own settings*.

### Order of work

17. (a) This ADR, its independent review and the owner's acceptance; specs. (b) RTMPS in `rbs-transport-rtmp` with app and CLI URL checks: useful alone for Facebook and Kick, measured (decision 10). (c) `rbs-record`: partition pools and limits, tests. (d) Host: profile sections, keys per destination, Go live/End stream over destinations, per-destination state; control API 1.68. (e) UI mock-ups, approval, implementation, screenshots, approval. (f) Short hardware runs: Mac (VideoToolbox) and SC-PC (NVENC) to three local listeners, one of them dropped and restarted mid-run.

## Consequences

- Multistreaming without a plugin or a relay, at the cost of one encoder for any number of destinations with shared settings. Each extra shared destination adds one output worker and one transport I/O thread, both idle between packets, and its transport queue of shared packet references (no copy). A destination with its own encoding adds an export (a readback per frame) and an encoder session.
- No graph or node code changes for the fan-out; the RTMP I/O loop changes for TLS and is measured (P15). Nothing runs for a stream that has one destination beyond what runs today, and nothing at all for a stream that is not live (rule 10).
- Public contract changes: `rbs-record` limits and partitions, output profile sections, transport capabilities (`rtmps`), control API 1.68. They need this ADR's independent review.
- Upload bandwidth is the user's real limit; Settings shows it before going live. RBS cannot measure the uplink yet (a later decision).
- Tests (ADR-0032, `TestClock`, waits on packet counts): three destinations share one encoder session; a destination joining late starts at a keyframe; a transport that rejects or gives up stops only its destination; stopping one leaves the others delivering; End stream flushes all; an own-encoding destination gets its own chain and two equal own encodings share; the limits refuse with `TooManyOutputs`; old profiles read as one destination; the new sections round-trip and a newer section is refused; RTMPS against a local TLS test server with a test-only root; a certificate that does not verify is a rejection; API 1.68 methods, errors and that no answer contains a URL or key.
- Docs: output-ports (RTMPS), recording (Studio limits), control-api (1.68), ui-design-system (Settings, Stream), glossary, status.

## Alternatives considered

- **One encoder per destination always** (obs-multi-rtmp's default): simple, but pays an encoder session and a readback per service for the same picture. Rejected as the default; it is what own encodings get.
- **One output node with several transports in one partition:** saves one mostly idle thread per destination, but changes the output node's `process` (hot path), needs per-transport failure handling inside a node, and makes starting or stopping one destination a node parameter instead of the existing `start_output`/`stop_output`. Deferred: if thread count matters in measurements, a later decision can merge destinations into one node.
- **A relay process or service** (Restream-style, or a local relay that receives one stream and re-sends it): adds a hop and latency, and a local relay still needs the same transports. Rejected.
- **A background "social media engine" with webhooks:** not needed for multistreaming; a desktop app cannot receive webhooks without a public relay. Platform account features are a separate later decision.
- **Bundled root certificates (`webpki-roots`) for RTMPS:** works offline, but ignores roots that companies install and ages with each release. Rejected for the OS store.
- **Native-TLS per OS (Secure Transport, SChannel):** three code paths and FFI in an adapter that is pure Rust today. Rejected.
- **An unlimited number of destinations:** partitions are created at start, so a fixed small limit keeps that free; six covers the services above. Raising it later is additive.

## Review

(Left for the independent reviewer, ADR-0014.)
