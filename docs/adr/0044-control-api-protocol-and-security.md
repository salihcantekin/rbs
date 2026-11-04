# ADR-0044: Control API: one typed request layer, JSON-RPC over swappable transports, scoped token authentication

- Status: Proposed
- Date: 2026-11-04
- Deciders: Salih (owner)

## Context

[ADR-0008](0008-ui-as-api-client.md) decided that every user interface is a client of a versioned control API (commands, queries, events, metrics), with local IPC for the desktop app and WebSocket for remote clients. The [control API spec](../specs/control-api.md) names its users: the desktop UI, `rbs-cli`, Stream Deck and other hardware panels, scripts, user-built apps and tests. UI rule U20 requires that every UI command is a control API command. The roadmap lists "Control API schema technology" as an open decision, and Phase 1 lists "headless engine with control API".

Nothing of it exists yet. `rbs-app` drives the studio through a private `Command`/`Update` pair in `crates/rbs-app/src/studio_host.rs` on its own control thread (ADR-0041 decision 7); `rbs-cli` calls `rbs-record` directly. Each new UI action adds another private command, and none of them can be reached from outside the process. The longer this goes on, the more UI behaviour has to be re-plumbed later.

Forces:

- **Layering (P1, P2).** Engine and control logic must not name a network, IPC or serialization library beyond `serde`. Transports must be swappable behind a port with a conformance suite.
- **Zero cost when unused (P13, ADR-0013 rule 4).** Most users never connect an outside app. A network service is an optional subsystem: no thread, socket or per-frame work unless enabled, and no event production without a subscriber.
- **Real-time paths (P6).** No control API work may run in `Node::process`, audio callbacks or the render tick. A slow or hostile client must not stall the control thread.
- **Security.** A control API can start recordings, write files and, later, load plugins. obs-websocket's history shows the risks: unauthenticated localhost services, browser pages reaching `ws://localhost` (cross-site WebSocket hijacking, DNS rebinding), and passwords sent in clear.
- **Contracts are versioned (P9).** Clients written by users must keep working across RBS releases; breaking changes only in a major version, previous major supported for one release cycle.
- **Clients in any language.** Stream Deck plugins and browser panels are JavaScript; scripts are often Python. A format those languages read without a code generator lowers the barrier.

## Decision

1. **One request layer, `rbs-api`.** The crate defines the typed protocol: requests (commands and queries), responses, typed errors and events, grouped by namespace (`studio.*`, `scene.*`, `source.*`, `audio.*`, `record.*`, `stream.*`, `edit.*`, `ui.*`, `session.*`, `api.*`), plus the session state machine (handshake, authentication, scopes, subscriptions, limits). It depends on `rbs-core`, `serde` and, for authentication only, the RustCrypto `hmac` and `sha2` crates. It must not depend on an engine crate, an adapter or a network, IPC, async-runtime or UI crate.
2. **Commands are handled by a `ControlTarget` that the composition root supplies.** `rbs-api` calls a trait implemented on the control thread that owns the studio and scene collection (today `studio_host`, later an engine-layer crate). All requests, from every client and from the desktop UI, are applied in order on that one thread. Commands that edit the scene collection go through its operations and undo records (scene-model spec); live actions (program switch, record and stream start/stop) and `ui.*` commands are not put on the undo stack (ADR-0021, control API spec).
3. **The desktop UI uses the same request types.** The UI sends `rbs-api` requests to the control thread through an in-process channel, with no serialization. Its private `studio_host::Command` enum is replaced step by step. A request the UI can send but `rbs-api` cannot express must not exist (U20); a test enumerates them.
4. **Transports are a port.** `rbs-api` defines `ControlTransport`: accept a connection, exchange whole text messages, report the peer (loopback or not, `Origin` and `Host` headers when present), close with a reason. Adapters: `rbs-api-ws` (WebSocket, TLS optional) and later `rbs-api-ipc` (named pipe on Windows, Unix domain socket elsewhere). WebSocket, TLS and async-runtime crates live only in adapters. Every adapter passes one conformance suite. Only `rbs-app` and `rbs-cli` choose which transports run.
5. **Wire format: JSON-RPC 2.0 over UTF-8 JSON text messages.** Requests and responses are JSON-RPC calls with `namespace.verb` method names; events are JSON-RPC notifications with method `event` and a `topic`. Batch requests are refused in protocol version 1. The WebSocket subprotocol is `rbs.v<major>`. Other encodings (MessagePack, CBOR) may be negotiated later in the handshake without changing the types.
6. **The Rust types are the schema.** The single source of truth is the `serde` types in `rbs-api`. A JSON Schema document is generated from them (build-time tool, not a runtime dependency), checked into the repository, and CI fails when it changes without a matching version bump. The schema is served at run time by `api.describe`, and client libraries (TypeScript first) are generated from it. Protocol version is `major.minor`: adding methods, optional fields or event topics is minor; removing or changing meaning is major. The server supports the current and the previous major version.
7. **Handshake and authentication.** A connection must complete `session.hello` before any other call: the client lists the protocol majors it speaks; the server answers with the chosen version, its own version, a nonce and a salt. The client then calls `session.authenticate` with `HMAC-SHA256(key, nonce)`, where `key = SHA-256(token || salt)`; the token itself never crosses the wire. The server stores only `key`, never the token. Unauthenticated connections get no data and are closed after a short timeout. Tokens are generated by RBS (at least 128 bits of OS randomness), shown to the user once, named, and revocable.
8. **Scopes.** Every token carries scopes; every method declares the scope it needs in the schema; the server refuses calls outside the token's scopes with a typed error. Scopes in version 1: `read` (queries and events), `control` (scene and source changes, mute and volume, transitions, record and stream start/stop to configured destinations, `ui.*`), `admin` (settings, file paths and destinations, plugin management, token management). A Stream Deck or panel token is `read` + `control`.
9. **Exposure.** Remote control is off by default. When on, the WebSocket adapter binds to loopback only unless the user enables network access, and the app shows that it is reachable from the network. Non-loopback connections require TLS. Connections that send an `Origin` header are refused unless that origin is on a user-managed allow list; `Host` headers must name a loopback address or a configured host name (DNS rebinding).
10. **Limits and back-pressure.** Each session has a maximum message size, a request rate limit and a bounded outgoing queue. Requests are queued to the control thread through a bounded channel; when it is full, the client gets a typed `busy` error, the control thread never waits on a client. Latest-value topics (levels, statistics) are coalesced per session. When a state-event queue overflows, the session receives `events.lost` with the last sequence number and must re-query; state events carry a per-topic sequence number so clients can detect gaps.
11. **Zero cost.** Off: the server is not constructed: no thread, socket, timer or event buffer. On with no session: one accept loop, no event production. Events are produced on the control thread only for topics at least one session subscribed to, when the state changes; high-rate topics are sampled from values the engine already publishes, at a client-chosen rate with a cap (initially 30 Hz levels, 1 Hz statistics), on the API's own thread. Nothing is added to `Node::process`, audio callbacks or the render tick.
12. **Out of scope here.** Remote preview video (control API spec open question, UI rule U5) and the OBS WebSocket compatibility layer, which stays a plugin that translates obs-websocket v5 calls into `rbs-api` requests under the same scopes (control API spec, "Compatibility").

## Consequences

- Every UI action becomes scriptable and reachable from Stream Deck, panels and user apps as soon as it exists, with no second code path.
- `rbs-app` must move its studio commands onto `rbs-api` requests; until then the U20 test lists the gaps.
- New crates: `rbs-api` (Control layer) and `rbs-api-ws` (adapter). `rbs-api` adds `hmac` and `sha2` as dependencies; the PR that adds them states the justification (pure Rust, audited, no C toolchain). The overview's layer table changes: `rbs-api` no longer depends on engine crates.
- Choosing JSON keeps the wire larger and slower to parse than a binary format; control traffic is small, and high-rate topics are capped, so this is accepted. A binary encoding can be negotiated later.
- Tests: a transport conformance suite (framing, size limit, close reasons, peer info); session tests on `TestClock` for handshake timeout, authentication, scopes, rate limits, queue overflow and `events.lost`; a schema snapshot test with the version-bump check; an idle-cost check that "off" starts no thread and "on, no session" produces no events.
- Spec updates: the control API spec is rewritten from this ADR in the same PR; the roadmap's open question is answered; follow-ups: TypeScript client generation, `rbs-cli call`, a settings page "Remote control" (UI rules apply), the IPC adapter, the OBS compatibility plugin.

## Alternatives considered

- **Protobuf with gRPC.** Strong schema evolution rules and many generators, but needs a code generator in the build, browsers need gRPC-Web and a proxy, and Stream Deck and script authors face a much higher barrier. Rejected for version 1; the JSON Schema and type layer leave room to add it as another encoding.
- **Own JSON envelope (like obs-websocket's op codes).** Full control, but every client writes its own framing; JSON-RPC 2.0 has client libraries in every common language and covers requests, responses and notifications.
- **HTTP REST plus polling.** Easy to call from anything, but no push events; meters and state changes need them. A REST bridge can be added later as another transport.
- **Speak the obs-websocket protocol natively.** Instant compatibility, but ties the API to OBS's scene model and its future changes. Kept as a compatibility plugin instead.
- **A message broker (MQTT and similar).** Good for many machines, but adds an external service users must run. Possible later as a transport adapter.
- **Password sent in the first message, or no authentication on loopback.** Simpler, but leaks the secret to anything that can read the traffic and leaves localhost open to other local apps and browser pages. Rejected.
- **Put command handling into `rbs-api` with a direct engine dependency (the overview's original plan).** Couples the protocol crate to engine internals and makes it harder to test; the `ControlTarget` trait keeps `rbs-api` testable with a fake target.

## Review

Pending: independent review per ADR-0014.
