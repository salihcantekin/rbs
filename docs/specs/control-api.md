# Spec: Control API

- Status: Draft
- Owner crate(s): `rbs-api` (planned), `rbs-api-ws` (planned adapter), `rbs-api-ipc` (planned adapter)
- Related ADRs: [0008](../adr/0008-ui-as-api-client.md), [0013](../adr/0013-zero-cost-when-unused.md), [0021](../adr/0021-scene-collection-model.md), [0044](../adr/0044-control-api-protocol-and-security.md) (Proposed)

## Purpose

The single way to control and observe the engine, used by the desktop UI, remote clients, CLI, Stream Deck and other hardware panels, scripts, user-built apps and tests.

## Shape

- **Commands** change state (add source, set transform, start stream). Every engine command that edits the scene collection is recorded in the command log and can be undone where meaningful. Live actions (program switch, record and stream start/stop) and client-scoped `ui.*` commands are not put on the undo stack (see below, ADR-0021).
- **Queries** read state (scene document, devices, adapter capabilities).
- **Events** stream changes and metrics to subscribers.
- The Rust `serde` types in `rbs-api` are the schema. A JSON Schema document is generated from them, checked in, served by `api.describe`, and used to generate client libraries (TypeScript first). Versioned as `major.minor` (below).
- All requests, from every client and from the desktop UI, are applied in order on one control thread, through a `ControlTarget` trait the composition root implements.

## Layers

| Part | Crate | Depends on |
| --- | --- | --- |
| Types, schema, session state machine, scopes, subscriptions, limits, `ControlTarget` and `ControlTransport` traits | `rbs-api` | `rbs-core`, `serde`, `hmac`, `sha2`; no engine, adapter, network, IPC, async-runtime or UI crate |
| WebSocket transport (TLS optional) | `rbs-api-ws` | `rbs-api`, WebSocket and TLS crates |
| Local IPC transport (named pipe, Unix domain socket) | `rbs-api-ipc` | `rbs-api`, OS IPC crates |
| In-process transport for the desktop app | `rbs-api` (channel, no serialization) | - |
| Which transports run, the `ControlTarget` implementation | `rbs-app`, `rbs-cli` | everything |

`ControlTransport` accepts connections, exchanges whole UTF-8 text messages, reports the peer (loopback or not, `Origin` and `Host` headers when present) and closes with a reason. Every adapter passes the transport conformance suite.

## Wire protocol

JSON-RPC 2.0 over UTF-8 JSON text messages. WebSocket subprotocol: `rbs.v<major>` (version 1: `rbs.v1`).

- **Request:** `{"jsonrpc":"2.0","id":7,"method":"record.start","params":{...}}`. Method names are `namespace.verb`.
- **Response:** `{"jsonrpc":"2.0","id":7,"result":{...}}` or `{"jsonrpc":"2.0","id":7,"error":{"code":-32010,"message":"...","data":{"kind":"scope_denied", ...}}}`. `data.kind` is the stable machine-readable error; `message` is for logs.
- **Event:** a notification `{"jsonrpc":"2.0","method":"event","params":{"topic":"record.state","seq":42,"data":{...}}}`. `seq` increases by one per topic, so a client can detect a gap.
- Batch requests are refused in version 1 (`batch_unsupported`).
- Requests on one connection are answered in order.

### Handshake and authentication

1. Client: `session.hello {protocols: [1], client: {name, version}}`.
2. Server: `{protocol: 1, server: {name, version}, nonce, salt}`. `nonce` is fresh per connection; `salt` belongs to the token store.
3. Client: `session.authenticate {token_name, proof}` with `proof = base64(HMAC-SHA256(key, nonce))` and `key = SHA-256(token || salt)`.
4. Server: the granted scopes, or `auth_failed` and close.

Before step 4 succeeds every other method returns `not_authenticated`. A connection that has not authenticated within the handshake timeout is closed. The server stores only `key` per token, never the token. Tokens have at least 128 bits of OS randomness, a user-given name, and can be revoked; revoking closes that token's sessions.

### Scopes

| Scope | Allows |
| --- | --- |
| `read` | Queries, `api.describe`, event subscriptions |
| `control` | Scene and source changes, mute and volume, transitions, program switch, record and stream start/stop to configured destinations, undo/redo, `ui.*` |
| `admin` | Settings, explicit file paths and stream destinations, plugin management, token management |

Every method declares its scope in the schema. A call outside the session's scopes returns `scope_denied`. A hardware panel token is `read` + `control`.

### Subscriptions and events

- `events.subscribe {topics: [...], rates: {"audio.levels": 20}}` and `events.unsubscribe`.
- State topics (`sources.changed`, `record.state`, `canvas.changed`, ...) are sent when the state changes.
- Latest-value topics (`audio.levels`, `stats`) are sampled from values the engine already publishes, at the client's rate, capped (30 Hz levels, 1 Hz statistics in version 1), and coalesced per session.
- When a session's queue for state topics overflows, the server drops its queued state events and sends `events.lost {topics, last_seq}`; the client re-queries.

### Versioning

Protocol version `major.minor`. Adding methods, optional fields, enum values clients must ignore when unknown, or event topics is a minor change. Removing or renaming anything, or changing meaning, is a major change. The server supports the current and the previous major for at least one release cycle (P9). CI compares the generated schema with the checked-in one and fails on a difference without a version bump.

## Exposure and limits

- Off by default. When off, no server exists: no thread, socket, timer or event buffer (ADR-0013 rule 1).
- On: the WebSocket transport binds to loopback. Network access is a separate setting; the app shows when RBS is reachable from the network. Non-loopback connections require TLS.
- Connections with an `Origin` header are refused unless the origin is on the user's allow list. The `Host` header must name a loopback address or a configured host name.
- Per session: maximum message size, request rate limit, bounded outgoing queue. Requests reach the control thread through a bounded channel; when it is full the client gets `busy`; the control thread never waits for a client.
- Connections, authentication failures and `admin` calls are written to the log.

## Zero cost

| State | Cost |
| --- | --- |
| Installed (setting off) | None: server not constructed |
| Standby (on, no session) | One accept loop; no event production |
| Active (sessions) | Events produced on the control thread only for subscribed topics, on state change; sampling of latest-value topics on the API's own thread. Nothing in `Node::process`, audio callbacks or the render tick |

## Initial method set (version 1, first slice)

The first slice covers what the desktop app does today; the full catalogue follows the scene-model operations.

| Method | Scope | Purpose |
| --- | --- | --- |
| `session.hello`, `session.authenticate` | none | Handshake |
| `api.describe` | `read` | Protocol version, methods with scopes, JSON Schema |
| `studio.get` | `read` | Canvas size and rate, encoders, whether recording is possible |
| `source.list`, `source.targets` | `read` | Program scene sources; capture targets of a kind |
| `source.add`, `source.delete`, `edit.undo` | `control` | Edit the program scene |
| `record.start`, `record.stop`, `record.status` | `control` / `read` | Record to the configured folder and settings |
| `canvas.set` | `admin` | Change canvas size and rate |
| `events.subscribe`, `events.unsubscribe` | `read` | Topics `source.changed`, `record.state`, `canvas.changed`, `audio.levels` |

## Client-scoped UI commands

Some commands change the state of one UI client, not of the engine: for example `ui.workspace.list`, `ui.workspace.switch(name)`, `ui.panel.open(id)` and `ui.panel.close(id)`. They live in the `ui.*` namespace with these rules:

- They are not written to the engine command log and are not undoable through it. The client keeps its own layout undo history (UI rule U21).
- The engine only routes them. Each connected UI client registers with a client id and a display name ("Studio PC"). A caller such as a Stream Deck plugin sends a `ui.*` command with a target client id, or without one to reach the desktop client that was focused most recently.
- The engine stores no UI state for them; layout and workspaces belong to the client (UI design system spec, "Panels and workspaces").

## Compatibility

An OBS WebSocket (v5) compatibility layer (subset) is a plugin that translates its calls into `rbs-api` requests under the same scopes, so existing Stream Deck and remote tools can work with RBS.

## Tests

- Transport conformance suite: framing, size limit, close reasons, peer information.
- Session tests on `TestClock`: handshake timeout, authentication success and failure, scopes, rate limit, `busy`, queue overflow and `events.lost`, sequence numbers.
- Schema snapshot test with the version-bump check.
- U20 test: every desktop UI command maps to a control API method.
- Idle-cost check: off starts no thread; on without a session produces no events.

## Open questions

- Preview transport for remote clients: UI rule U5 requires a reduced-rate encoded preview for remote panels; WebRTC from the engine is the candidate.
- Stable addressing for layout commands. `rbs_ui::LayoutCommand` addresses windows and tab stacks by index, which shifts after each change; it is internal to the desktop shell. The `ui.*` commands for remote callers address panels by panel id (open, close, float, dock, move to an edge) and need stable window ids before they can target a window or stack.
- Where tokens and the token-store salt are kept (settings file with user-only permissions, or the OS credential store).
- TLS certificates for network access: a self-signed certificate with a fingerprint the user confirms, or a user-supplied one.
- Which WebSocket crate and runtime `rbs-api-ws` uses (adapter choice, decided in its PR).
