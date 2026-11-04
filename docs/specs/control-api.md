# Spec: Control API

- Status: Draft
- Owner crate(s): `rbs-api` (planned), `rbs-api-ws` (planned adapter), `rbs-api-ipc` (planned adapter)
- Related ADRs: [0008](../adr/0008-ui-as-api-client.md), [0013](../adr/0013-zero-cost-when-unused.md), [0021](../adr/0021-scene-collection-model.md), [0035](../adr/0035-meters-as-native-layers.md), [0044](../adr/0044-control-api-protocol-and-security.md) (Proposed)

## Purpose

The single way to control and observe the engine, used by the desktop UI, remote clients, CLI, Stream Deck and other hardware panels, scripts, user-built apps and tests.

## Shape

- **Commands** change state (add source, set transform, start stream). Commands that edit the scene collection go through its operations and the one shared user undo stack. Live actions (program switch, record and stream start/stop) and client-scoped `ui.*` commands are not put on the undo stack (see below, ADR-0021).
- **Queries** read state (scene document, devices, adapter capabilities, settings).
- **Events** stream changes and metrics to subscribers.
- The Rust `serde` types in `rbs-api` are the schema; every request, response and event type is `Serialize + Deserialize`. A JSON Schema document is generated from them, checked in, served by `api.describe`, and used to generate client libraries (TypeScript first).
- All requests, from every client and from the desktop UI, are applied one at a time on the control thread, through a `ControlTarget` trait the composition root implements. That thread owns the studio, the scene collection, recording profiles, output destinations and canvas settings.

## Layers

| Part | Crate | Depends on |
| --- | --- | --- |
| Types, JSON codec (behind `Codec`), session state machine, scopes, subscriptions, limits, `ControlTarget` and `ControlTransport` traits, API thread | `rbs-api` | `rbs-core`, `serde`, `serde_json`, `sha2`, `subtle`, `getrandom`; `schemars` only with the `schema` feature. No engine, adapter, network, IPC, async-runtime or UI crate |
| WebSocket transport, TLS | `rbs-api-ws` | `rbs-api`, WebSocket and TLS crates, optionally an async runtime |
| Local IPC transport (Unix domain socket, named pipe) | `rbs-api-ipc` | `rbs-api`, OS IPC crates |
| In-process path for the desktop app | `rbs-api` (channel, no serialization) | - |
| Which transports run; the `ControlTarget` implementation | `rbs-app`, `rbs-cli` | everything |

`ControlTransport` adapters accept connections, exchange whole UTF-8 messages and report the peer: transport kind, loopback or not, `Origin` and `Host` headers when present, OS user for IPC. They run their own I/O and push `Connected`, `Message` and `Closed` events into a bounded inbox owned by `rbs-api`; `rbs-api` sends through a per-connection handle. Every adapter passes the transport conformance suite.

## Threads

| Thread | Exists | Does |
| --- | --- | --- |
| Adapter I/O | While its transport is on | Accept, read, write; pushes into the API inbox |
| API thread | While remote control is on | Owns all sessions; handshake, auth, scopes, limits, queues; serializes each event once; blocks on its inbox until the session state machine's next deadline, no periodic wake-up |
| Sampling (on the API thread's deadline) | While a session subscribes to a latest-value topic | Reads levels and statistics at the requested rate from read-only, lock-free handles the composition root provides; never wakes the control thread |
| Control thread | Always (existing) | Applies requests from two queues (local UI, API) behind one wake-up, UI first; sends responses and events tagged with session and request ids into an output channel it never blocks on |

- The session logic is a pure state machine, `handle(event, now)` and `next_deadline()`, that tests drive directly with `TestClock` (ADR-0032); the API thread only wraps it.
- Adapters apply back-pressure per connection: a connection that has used its queue quota is not read until it drains, so one client cannot fill the shared inbox.
- Each session has at most one request in flight to the control thread and a bounded queue behind it; the API thread forwards from sessions in round-robin order. Responses on one connection come in request order, so a long command (for example `canvas.set`, which restarts the studio) delays only that connection's later requests. A dispatched command is never cancelled.
- Output from the control thread never blocks it: responses are bounded by the one-in-flight rule; events are coalesced per topic, and a session that falls behind gets `events.lost`.
- Every state topic has a change counter `seq` that the control thread increments on every change of that state, subscribed or not. It builds event payloads only for topics in a subscriber mask. The in-process UI is a permanent subscriber registered on the control thread and gets the same event types and `seq`. The API thread sets its sessions' mask bits before it forwards `events.subscribe`; the control thread answers with the `seq` read after that.
- The desktop UI's level meters and preview do not go through `rbs-api`: the local UI gets the shared meters (ADR-0035) and the preview endpoint as local-only attachments.

## Wire protocol

JSON-RPC 2.0 over UTF-8 JSON text messages. WebSocket subprotocol: `rbs.v<major>` (version 1: `rbs.v1`).

- **Request:** `{"jsonrpc":"2.0","id":7,"method":"record.start","params":{...}}`. Method names are `namespace.verb`.
- **Response:** `{"jsonrpc":"2.0","id":7,"result":{...}}` or `{"jsonrpc":"2.0","id":7,"error":{"code":-32010,"message":"...","data":{"kind":"scope_denied", ...}}}`. `data.kind` is the stable machine-readable error; `message` is for logs.
- **Event:** a notification `{"jsonrpc":"2.0","method":"event","params":{"topic":"record.state","seq":42,"data":{...}}}`.
- Batch requests are refused in version 1 (`batch_unsupported`).

### Handshake and authentication

1. Client: `session.hello {protocols: ["1.0"], client: {name, version}}`.
2. Server: `{protocol: "1.0", instance}`. `instance` is a random id for this server run; `seq` counters restart with it.
3. Client: `session.authenticate {token}`.
4. Server: `{scopes, server: {name, version}}`, or `auth_failed` and close.

On IPC, a peer the OS identifies as the same user is authenticated by step 2, which then also returns its scopes. Before authentication every other method returns `not_authenticated`. A connection that has not authenticated within the handshake timeout is closed. `auth_failed` is the same for an unknown and a wrong token.

**Tokens.** Text `rbs1_` followed by 64 lowercase hex digits (32 bytes of OS randomness). The server stores `SHA-256` of the token's UTF-8 bytes, as lowercase hex, and compares in constant time. Each token has a user-given name and scopes, is shown once at creation (UI or `rbs-cli token create`), and can be revoked; revoking closes its sessions.

Test vector: token `rbs1_000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f` is stored as `73aa00dce5b7afd18306d160c3de0e6e56db9ade3300340da67c2021c402f04b`.

**Channels.** A token is sent only over loopback, IPC, or TLS whose certificate the client verified (a CA or a pinned fingerprint). Client libraries must refuse to send it after a certificate mismatch. IPC peers are identified by the OS: a peer running as the same user is authenticated without a token.

### Scopes

| Scope | Allows | From |
| --- | --- | --- |
| `read` | Queries, `api.describe`, event subscriptions | Any authenticated peer |
| `control` | Scene and source changes, capture-target lists, mute and volume, transitions, program switch, record and stream start/stop with configured profiles and destinations, undo and redo, `ui.*` | Any authenticated peer |
| `admin` | Settings, explicit file paths and destinations (an explicit file is never overwritten; its path is logged) | Loopback and IPC; remote only with the separate "Allow remote admin" setting |
| `plugins` | Plugin management | Loopback and IPC only |
| `tokens` | Token management | Loopback and IPC only |

Every method declares one scope in the schema; where a parameter would need a different scope it is a separate method; a call outside the session's scopes returns `scope_denied`. A hardware panel token is `read` + `control`. The in-process UI and same-user IPC peers have all scopes.

### Subscriptions and events

- `events.subscribe {topics: [...], rates: {"audio.levels": 20}}` returns the current `seq` per topic; `events.unsubscribe`.
- State queries return `{seq: {topic: n}, data}`, so a client knows which events its snapshot already includes and ignores events with an equal or lower `seq`.
- State topics are sent when the state changes. Latest-value topics (`audio.levels`, `stats`) are sampled at the client's rate, capped (30 Hz levels, 1 Hz statistics in version 1), and coalesced per session.
- When a session's state-event queue overflows, the server drops its queued state events and sends `events.lost {last_seq: {topic: n}}`; the client re-queries.

### Versioning

Protocol version `major.minor`, independent of the `rbs-api` crate's semver. Minor: new methods, event topics, optional request fields, response fields, enum values in responses, error kinds. Major: removing or renaming anything, changing a type or meaning, a new required request field. Clients ignore unknown response fields and treat unknown enum values and error kinds generically. The server supports the current and the previous major for at least one release cycle (P9). A CI tool compares the generated schema with the checked-in one, classifies each difference, and fails unless the version bump matches.

## Exposure and limits

- Off by default. When off, no server exists: no thread, socket, timer or event buffer (ADR-0013 rule 1).
- On: WebSocket binds to loopback. Network access is a separate setting; the app shows when RBS is reachable from the network. Non-loopback connections require TLS.
- Before the WebSocket upgrade completes: a request with an `Origin` header (including `Origin: null`) is refused unless the origin is on the user's allow list (`null` can never be added to it); the `Host` header must be a loopback name or address, a configured host name, or an address of a bound interface.
- IPC endpoint: current user only. Unix: socket mode 0600 in a user-private directory. Windows: named pipe with a DACL for the current user's SID and `PIPE_REJECT_REMOTE_CLIENTS`.
- Caps: total connections, pending handshakes, connections per peer address (not applied on loopback, where every client shares one address). Pre-authentication messages are small (4 KiB). Repeated `auth_failed` delays further attempts with growing back-off: per connection on loopback, per peer address otherwise.
- Per session: maximum message size, request rate, queue length (`busy` when full).
- Connections, authentication failures and `admin`, `plugins` and `tokens` calls are logged with the token name.

## Zero cost

| State | Cost |
| --- | --- |
| Installed (setting off) | None: server not constructed |
| Standby (on, no session) | Adapter accept and the API thread blocked on its inbox; no wake-ups, no event production |
| Active (sessions) | Events produced on the control thread only for subscribed topics, on state change; sampling only while a latest-value topic is subscribed. Nothing in `Node::process`, audio callbacks or the render tick |

## Initial method set (version 1, first slice)

The first slice covers what the desktop app does today; the full catalogue follows the scene-model operations.

| Method | Scope | Purpose |
| --- | --- | --- |
| `session.hello`, `session.authenticate` | none | Handshake |
| `api.describe` | `read` | Protocol version, methods with scopes, JSON Schema |
| `studio.get` | `read` | Canvas size and rate, encoders, whether recording is possible |
| `source.list` | `read` | Program scene sources |
| `source.targets` | `control` | Capture targets of a kind (window titles can be private) |
| `source.add`, `source.delete`, `edit.undo`, `edit.redo` | `control` | Edit the program scene |
| `output.profiles` | `read` | Recording profiles: id, name, container, encoder, bitrates (folders only through `settings.get`) |
| `record.start {profile}`, `record.stop` | `control` | Record with a configured profile into its folder |
| `record.start_to_file {profile, path}` | `admin` | Record to an explicit file; refused if it exists |
| `record.status` | `read` | Recording state |
| `settings.get`, `settings.set` | `admin` | Recording profiles, canvas size and rate |
| `events.subscribe`, `events.unsubscribe` | `read` | Topics `source.changed`, `record.state`, `canvas.changed`, `settings.changed`, `audio.levels` |

## Client-scoped UI commands

Some commands change the state of one UI client, not of the engine: for example `ui.workspace.list`, `ui.workspace.switch(name)`, `ui.panel.open(id)` and `ui.panel.close(id)`. They live in the `ui.*` namespace with these rules:

- They are not written to the engine command log and are not undoable through it. The client keeps its own layout undo history (UI rule U21).
- The engine only routes them. Each connected UI client registers with a client id and a display name ("Studio PC"). A caller such as a Stream Deck plugin sends a `ui.*` command with a target client id, or without one to reach the desktop client that was focused most recently.
- The engine stores no UI state for them; layout and workspaces belong to the client (UI design system spec, "Panels and workspaces").

## Undo

One user undo stack for the scene collection, shared by all clients. `edit.undo` from a panel can undo the desktop user's last edit, as in a shared document. An undo record used out of order returns `undo_conflict`. The serialisable operation type joins the schema as a minor addition with the scene document format ADR.

## Compatibility

An OBS WebSocket (v5) compatibility layer (subset) is a separate optional adapter crate that speaks that protocol on its own endpoint and turns its calls into `rbs-api` requests in process, under a token's scopes, so existing Stream Deck and remote tools can work with RBS. It follows the same channel rules. obs-websocket authenticates with a salted password challenge, which the token store cannot answer, so the adapter keeps its own secret store; how is decided in its own review. Packaging it as a plugin waits for a plugin SDK host call that submits requests (plugin SDK spec, own review).

## Tests

- Transport conformance suite: framing, size limits, close reasons, peer information, Origin and Host cases (`null`, allow list, IP literals), upgrade refused before completion.
- Session state machine tests on `TestClock`: handshake timeout, IPC same-user handshake, auth success, uniform failure, token test vector, scopes, remote refusal of `admin`/`plugins`/`tokens`, connection and handshake caps, back-off, pre-auth size, rate limit, `busy`, round-robin fairness, UI priority, per-connection back-pressure, control thread never blocked by API output, `seq` across subscribe, overflow and `events.lost`, `seq` and instance id.
- JSON round-trip test of every request, response and event type.
- Schema snapshot test with difference classification.
- U20 test: every desktop UI command maps to a control API method.
- Idle-cost check: off starts no thread; on without a session has no wake-ups and produces no events (thread count and wake-ups, performance budgets spec).

## Open questions

- Preview transport for remote clients: UI rule U5 requires a reduced-rate encoded preview for remote panels; WebRTC from the engine is the candidate.
- Stable addressing for layout commands. `rbs_ui::LayoutCommand` addresses windows and tab stacks by index, which shifts after each change; it is internal to the desktop shell. The `ui.*` commands for remote callers address panels by panel id (open, close, float, dock, move to an edge) and need stable window ids before they can target a window or stack.
- Where the token store lives (settings file with user-only permissions, or the OS credential store).
- TLS certificates for network access: a generated self-signed certificate with a fingerprint the user pins, or a user-supplied one; channel binding (RFC 9266) if pinning proves insufficient.
- Exact caps and timeouts (connections, handshakes, back-off, queue lengths), set with the first implementation and its tests.
- Which WebSocket crate and runtime `rbs-api-ws` uses (adapter choice, decided in its PR).
