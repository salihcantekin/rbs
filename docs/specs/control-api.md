# Spec: Control API

- Status: Draft
- Owner crate(s): `rbs-api` (first slice implemented, see "Implementation status"), `rbs-api-ws` (loopback WebSocket adapter implemented; TLS planned), `rbs-api-ipc` (local IPC adapter implemented and served by `rbs-app`)
- Related ADRs: [0008](../adr/0008-ui-as-api-client.md), [0013](../adr/0013-zero-cost-when-unused.md), [0021](../adr/0021-scene-collection-model.md), [0035](../adr/0035-meters-as-native-layers.md), [0044](../adr/0044-control-api-protocol-and-security.md)

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
| WebSocket transport, TLS | `rbs-api-ws` | `rbs-api`, `polling` (readiness), `httparse` (upgrade request), `sha1` (`Sec-WebSocket-Accept`); a TLS crate when network access lands. Framing is its own (RFC 6455 server side), so a message over the limit can be skipped without buffering it and the connection kept; no async runtime |
| Local IPC transport (Unix domain socket, named pipe) and its blocking client | `rbs-api-ipc` | `rbs-api`, `libc` (Unix), `windows` (Windows) |
| In-process path for the desktop app | `rbs-api` (channel, no serialization) | - |
| Which transports run; the `ControlTarget` implementation | `rbs-app`, `rbs-cli` | everything |
| Remote control settings file (`remote-control.toml`: on or off, port, token hashes), the config directory and the `tokens.*` answers on that file, shared by the composition roots | `rbs-remote-settings` | `rbs-api`, `serde`, `toml`; no toolkit, engine or adapter crate |

`ControlTransport` adapters accept connections, exchange whole UTF-8 messages and report the peer: transport kind, loopback or not, `Origin` and `Host` headers when present, OS user for IPC. They run their own I/O and push `Connected`, `Message` and `Closed` events into a bounded inbox owned by `rbs-api`; `rbs-api` sends through a per-connection handle. Every adapter passes the transport conformance suite (`rbs_api::conformance`, feature `conformance`).

## Threads

| Thread | Exists | Does |
| --- | --- | --- |
| Adapter I/O | While its transport is on | Accept, read, write; pushes into the API inbox |
| API thread | While remote control is on | Owns all sessions; handshake, auth, scopes, limits, queues; serializes each event once; sleeps on one wake primitive shared by the transport inbox, the response channel and the event slots, until the state machine's next deadline; no periodic wake-up |
| Sampling (on the API thread's deadline) | While a session subscribes to a latest-value topic | Reads levels and statistics at the requested rate from read-only, lock-free handles the composition root provides; never wakes the control thread |
| Control thread | Always (existing) | Applies requests from two queues (local UI, API) behind one wake-up, UI first; sends responses tagged with session and request ids, and fills per-state-key event slots; never blocks on API output |

- The session logic is a pure state machine, `handle(event, now)` and `next_deadline()`, that tests drive directly with `TestClock` (ADR-0032); the API thread only wraps it.
- Adapters apply back-pressure per connection: a connection that has used its queue quota is not read until it drains, so one client cannot fill the transport inbox.
- Each session has at most one request in flight to the control thread and a bounded queue behind it; the API thread forwards from sessions in round-robin order. Responses on one connection come in request order, so a long command (for example `canvas.set`, which restarts the studio) delays that connection's later requests; while it runs on the single control thread, other sessions' and the UI's requests also wait for it. A dispatched command is never cancelled.
- Output from the control thread never blocks it and never loses a response. Responses go into their own channel sized by the number of requests that can be in flight: one per connection under the connection cap. A closed session's in-flight request stays counted, and its connection slot stays taken, until its response is drained and discarded. So a send cannot fail, whatever fills the transport inbox. Events go into per-state-key latest-value slots; a newer event replaces the older one. A single `events pending` flag replaces one message per write: the API thread atomically swaps it to clear before it reads the slots, and a writer that changes it from clear to set signals the wake primitive. The wake primitive is permit-based, so a signal sent before the API thread sleeps is not lost; pushes to the transport inbox and the response channel signal it the same way.
- Every state key has a change counter `seq` that the control thread increments on every change of that state, subscribed or not. It builds event payloads only for topics in a subscriber mask. The in-process UI is a permanent subscriber registered on the control thread and gets the same event types and `seq`. The API thread sets its sessions' mask bits before it forwards `events.subscribe`; the control thread answers with the `seq` read after that.
- The desktop UI's level meters and preview do not go through `rbs-api`: the local UI gets the shared meters (ADR-0035) and the preview endpoint as local-only attachments.

## Wire protocol

JSON-RPC 2.0 over UTF-8 JSON text messages. WebSocket subprotocol: `rbs.v<major>` (version 1: `rbs.v1`). A client that offers subprotocols must offer `rbs.v1` (the upgrade is refused otherwise); a client that offers none speaks version 1. Binary messages close the connection with 1003.

WebSocket close codes: 1001 when the server stops, 1009 for a message before authentication over the limit, and in the private range `handshake_timeout` 4001, `auth_failed` 4002, `unsupported_protocol` 4003, `too_many_connections` 4004, `revoked` 4005, `too_slow` 4006. The close frame's reason text is that name. Protocol violations close with 1002 (1007 for text that is not UTF-8).

- **Request:** `{"jsonrpc":"2.0","id":7,"method":"record.start","params":{...}}`. Method names are `namespace.verb`.
- **Response:** `{"jsonrpc":"2.0","id":7,"result":{...}}` or `{"jsonrpc":"2.0","id":7,"error":{"code":-32010,"message":"...","data":{"kind":"scope_denied", ...}}}`. `data.kind` is the stable machine-readable error; `message` is for logs.
- **Event:** a notification `{"jsonrpc":"2.0","method":"event","params":{"topic":"record.state","seq":42,"data":{...}}}`.
- Batch requests are refused in version 1 (`batch_unsupported`).

### Local IPC

The same JSON-RPC messages over a local stream (`rbs-api-ipc`):

- **Endpoint.** Linux: `$XDG_RUNTIME_DIR/rbs/control.sock`, or `/tmp/rbs-<uid>/control.sock` without it; macOS: `$TMPDIR/rbs/control.sock`; Windows: the named pipe `\\.\pipe\rbs-control-<user SID>`. The Unix directory is made with mode 0700 and refused unless it is a real directory (not a symbolic link) owned by the current user that nobody else can enter; the socket is mode 0600. While serving, the server holds an advisory lock on `control.sock.lock` beside it: a second server is refused before it touches the socket file, and under the lock a socket file left by a process that ended is replaced. The pipe is created with a protected DACL that grants access to the current user's SID only, `PIPE_REJECT_REMOTE_CLIENTS`, and `FILE_FLAG_FIRST_PIPE_INSTANCE` for its first instance (a name someone else holds stops the server); one instance always waits, so the name stays the server's.
- **Peers.** The server asks the OS who the peer is (`SO_PEERCRED` on Linux, `getpeereid` on macOS and the BSDs; on Windows the user of the connection's own token, read while briefly impersonating the client at identification level, not looked up by process id) and closes the connection of any other user before it reaches the API. Every accepted peer is a same-user IPC peer, which `session.hello` authenticates with all scopes. The client checks the server the same way (Windows: the token user of the server process, found by `GetNamedPipeServerProcessId`; it opens the pipe with `SECURITY_IDENTIFICATION`, so the server can read its identity but not act as it). A Windows client whose pipe instances are all busy for a moment waits for one (`WaitNamedPipeW`) within its connect timeout and sends nothing to a server of another user.
- **Framing.** An accepted connection starts with the server's 4-byte preamble `RBS` `0x01` (framing version 1). Then each message in either direction is a 4-byte big-endian header, bit 31 clear and the low 31 bits the length, followed by that many bytes of UTF-8 text. A header with bit 31 set is a close frame: a big-endian `u16` code (the WebSocket close codes above; 1000 from a client closing normally) and up to 123 bytes of reason text. A connection refused at accept (connection cap) gets a close frame with 4004 instead of the preamble. A message over the connection's limit is skipped as it arrives and reported as `Oversized`; the connection keeps working. A message that is not UTF-8 ends the connection.
- **Threads.** One accept thread while serving, and a reader and a writer thread per connection, each blocked in the OS until there is work: no periodic wake-up. The reader stops reading a connection that used its inbox quota until the API thread drains it; output per connection is bounded (1 MiB, then `Full`, `Writable` when half has drained). A write that makes no progress for 10 s ends the connection, so a peer that stops reading cannot hold its slot. When the server closes a connection, what is queued and then the close frame are written, also while its reading is paused. On stop, the queued output is dropped, every connection gets its close frame (1001), and after one grace period for all of them (1 s) the streams are cut. The accept thread waits on the listener and a wake-up of its own, so stopping does not depend on the socket file.

### Handshake and authentication

1. Client: `session.hello {protocols: ["1.0"], client: {name, version}}`.
2. Server: `{protocol: "1.0", instance}`. `instance` is a random id for this server run; `seq` counters restart with it.
3. Client: `session.authenticate {token}`.
4. Server: `{scopes, server: {name, version}}`, or `auth_failed` and close.

Until the `session.authenticate` answer arrives, the pre-authentication message limit (4 KiB) applies, so a client waits for it before sending larger requests. On IPC, a peer the OS identifies as the same user is authenticated by step 2, which then also returns its scopes. Before authentication every other method returns `not_authenticated`. A connection that has not authenticated within the handshake timeout is closed. `auth_failed` is the same for an unknown and a wrong token.

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

Event topics declare a scope the same way: `settings.changed` carries recording folders, which only `admin` reads (`settings.get`), so subscribing to it needs `admin`; every other topic needs `read`.

### Subscriptions and events

- `events.subscribe {topics: [...], rates: {"audio.levels": 20}}` returns the current `seq` per state key; `events.unsubscribe`.
- State queries return `{seq: {state_key: n}, data}`, so a client knows which events its snapshot already includes and ignores events with an equal or lower `seq`.
- A state event carries the whole current state of its state key, never a delta, so a newer event replaces an older one without loss. Topics are kept small. A state key is a topic name plus an optional object id (for example one key per source); `seq`, coalescing and the `seq` maps in queries and `events.subscribe` use the state key, while subscriptions and the subscriber mask are per topic name. The first slice's topics have no object id: `source.changed` carries the whole program-scene source list. A client applies an event only when its `seq` is higher than what it holds; skipped `seq` values are normal and need no re-query.
- Sampling of a latest-value topic starts when `events.subscribe` is handled on the API thread, so a sample can arrive before the subscribe answer. Samples carry a per-server sample counter as `seq`, which has no entry in `seq` maps.
- State topics are sent when the state changes. Latest-value topics (`audio.levels`, `stats`) are sampled at the client's rate, capped (30 Hz levels, 1 Hz statistics in version 1), and coalesced per session.
- Each session's outgoing queue keeps only the latest event per state key, so it is bounded by the number of live state keys and cannot overflow. There is no `events.lost`.

### Versioning

Protocol version `major.minor`, independent of the `rbs-api` crate's semver. Minor: new methods, event topics, optional request fields, response fields, enum values in responses, error kinds. Major: removing or renaming anything, changing a type or meaning, a new required request field. Clients ignore unknown response fields and treat unknown enum values and error kinds generically. The server supports the current and the previous major for at least one release cycle (P9). A CI tool compares the generated schema with the checked-in one, classifies each difference, and fails unless the version bump matches.

## Exposure and limits

- Off by default. When off, no server exists: no thread, socket, timer or event buffer (ADR-0013 rule 1).
- On: WebSocket binds to loopback. Network access is a separate setting; the app shows when RBS is reachable from the network. Non-loopback connections require TLS.
- Before the WebSocket upgrade completes: a request with an `Origin` header (including `Origin: null`) is refused unless the origin is on the user's allow list (`null` can never be added to it); the `Host` header must be a loopback name or address, a configured host name, or an address of a bound interface.
- IPC endpoint: current user only. Unix: socket mode 0600 in a user-private directory. Windows: named pipe with a DACL for the current user's SID and `PIPE_REJECT_REMOTE_CLIENTS`.
- Caps: total connections, pending handshakes, connections per peer address (not applied on loopback, where every client shares one address). Pre-authentication messages are small (4 KiB). Repeated `auth_failed` delays further attempts with growing back-off: per connection on loopback, per peer address otherwise.
- Per session: maximum message size, request rate, queue length (`busy` when full).
- Responses come in request order, with one exception: refusals that protect the server are sent at once and can overtake the answer to an earlier request still in flight, because queueing them would make the queue unbounded. They are `busy`, `rate_limited`, `message_too_large`, and errors for messages whose request id cannot be read (answered with `"id": null`). Every other answer, including `scope_denied` and decoding errors that carry an id, waits its turn.
- Before authentication the server reveals only the handshake: a message naming any other method, known or not, gets `not_authenticated`, without the method list or parameter details.
- Transports read at most the connection's current message limit (4 KiB before authentication) and report a longer message instead of buffering it, so unauthenticated peers cannot make the server hold large messages.
- The pending-handshake cap counts and refuses only non-loopback peers, and the last slots of the connection cap are reserved for loopback and IPC peers, so remote clients, even a flood of connections the server refuses, cannot lock out local clients.
- Per-address caps and back-off group IPv6 addresses by their /64 prefix.
- Changing a token's name or scopes, or revoking it, closes its sessions; the client reconnects with what the token allows now.
- A peer that does not read its answers is closed once more than a limit of answers wait for it (events do not count: only the latest per state key waits).

Version 1 limits (from the first implementation, `rbs_api::Limits::default`):

| Limit | Value |
| --- | --- |
| Connections (including closed ones whose request is still in flight) | 64 |
| Pending handshakes (non-loopback peers) | 16 |
| Connection slots only loopback and IPC peers can take | 8 of the 64 |
| Connections per non-loopback address (IPv6: per /64) | 8 |
| Handshake timeout (connect to authenticated) | 10 s |
| Message size before / after authentication | 4 KiB / 1 MiB |
| Request rate per session | 50 per second, bursts of 100 |
| Requests queued behind the one in flight | 16 |
| Answers waiting for a peer that does not read | 64 |
| Unread messages per connection in the transport inbox | 8 |
| Authentication back-off per non-loopback address | 500 ms after the first failure, doubling, at most 30 s; forgotten 30 s after its delay ends; 1024 addresses remembered; an attempt released after its delay waits again if another attempt from the address failed meanwhile |
- Connections, authentication failures and `admin`, `plugins` and `tokens` calls are logged with the token name.

## Zero cost

| State | Cost |
| --- | --- |
| Installed (setting off) | None: server not constructed |
| Standby (on, no session) | Adapter accept and the API thread asleep on its wake primitive; no wake-ups, no event production |
| Active (sessions) | Events produced on the control thread only for subscribed topics, on state change; sampling only while a latest-value topic is subscribed. Nothing in `Node::process`, audio callbacks or the render tick |

## Errors

`error.data.kind` is stable; codes are fixed so clients that only read `code` still work.

| Kind | Code | Meaning |
| --- | --- | --- |
| `parse_error` | -32700 | Not JSON |
| `invalid_request` | -32600 | Not a JSON-RPC 2.0 request (no integer or string `id`, wrong `jsonrpc`), or a handshake method after authentication |
| `method_not_found` | -32601 | Unknown method |
| `invalid_params` | -32602 | Parameters do not match the method's schema; `params` must be an object or absent |
| `internal` | -32603 | Server bug |
| `not_authenticated` | -32001 | Method before authentication |
| `auth_failed` | -32002 | Unknown or wrong token |
| `unsupported_protocol` | -32003 | No common protocol major |
| `scope_denied` | -32010 | Scope missing |
| `busy` | -32020 | Request queue full |
| `rate_limited` | -32021 | Request rate exceeded |
| `message_too_large` | -32022 | Message over the size limit |
| `batch_unsupported` | -32023 | JSON array of requests |
| `not_found` | -32030 | Object does not exist |
| `conflict` | -32031 | Conflicts with the current state (for example a recording runs) |
| `undo_conflict` | -32032 | Undo record used out of order |
| `file_exists` | -32033 | Explicit file exists |
| `failed` | -32040 | Valid but could not be carried out |

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
| `tokens.list` | `tokens` | Access tokens: name and scopes, never the token or its hash |
| `tokens.create {name, scopes}` | `tokens` | Make a named token; the answer holds the token, the only time it exists in readable form. `scopes` is one of the desktop app's access levels: `["read"]`, `["read", "control"]` or `["read", "control", "admin"]`; other sets, an empty, too long or taken name, or a name with control characters are refused (`invalid_params`, `conflict` for a taken name) |
| `tokens.revoke {name}` | `tokens` | Revoke a token by name (`not_found` if none); its sessions are closed |

A token change is in effect before its answer is sent: a new token authenticates, and a revoked token's sessions are closed, for any message sent after the answer arrived. Only loopback and IPC sessions can hold `tokens`, and no token the app makes has it, so in practice the callers are same-user IPC peers such as `rbs-cli token`.

## Client-scoped UI commands

Some commands change the state of one UI client, not of the engine: for example `ui.workspace.list`, `ui.workspace.switch(name)`, `ui.panel.open(id)` and `ui.panel.close(id)`. They live in the `ui.*` namespace with these rules:

- They are not written to the engine command log and are not undoable through it. The client keeps its own layout undo history (UI rule U21).
- The engine only routes them. Each connected UI client registers with a client id and a display name ("Studio PC"). A caller such as a Stream Deck plugin sends a `ui.*` command with a target client id, or without one to reach the desktop client that was focused most recently.
- The engine stores no UI state for them; layout and workspaces belong to the client (UI design system spec, "Panels and workspaces").

## Undo

One user undo stack for the scene collection, shared by all clients. `edit.undo` from a panel can undo the desktop user's last edit, as in a shared document. An undo record used out of order returns `undo_conflict`. `edit.undo` with nothing to undo returns `conflict`. The serialisable operation type joins the schema as a minor addition with the scene document format ADR.

## Compatibility

An OBS WebSocket (v5) compatibility layer (subset) is a separate optional adapter crate that speaks that protocol on its own endpoint and turns its calls into `rbs-api` requests in process, so existing Stream Deck and remote tools can work with RBS. It follows the same channel rules. obs-websocket authenticates with a salted password challenge, which the token store cannot answer, so the adapter keeps its own secret store, decided in its own review; each entry carries scopes applied the same way, and the remote limits on `admin`, `plugins` and `tokens` apply unchanged. Packaging it as a plugin waits for a plugin SDK host call that submits requests (plugin SDK spec, own review).

## Tests

- Transport conformance suite: framing, size limits, close reasons, peer information, Origin and Host cases (`null`, allow list, IP literals), upgrade refused before completion.
- Session state machine tests on `TestClock`: handshake timeout, IPC same-user handshake, auth success, uniform failure, token test vector, scopes, remote refusal of `admin`/`plugins`/`tokens`, connection and handshake caps, back-off, pre-auth size, rate limit, `busy`, round-robin fairness, UI priority, per-connection back-pressure, control thread never blocked by API output, responses and events delivered while the transport inbox is full, no response lost under an event flood or with a session closed while its request is in flight, an event written while the API thread drains the slots, event coalescing per state key, `seq` across subscribe, `seq` and instance id.
- JSON round-trip test of every request, response and event type.
- Schema snapshot test with difference classification.
- U20 test: every desktop UI command maps to a control API method.
- Idle-cost check: off starts no thread; on without a session has no wake-ups and produces no events (thread count and wake-ups, performance budgets spec).

## Implementation status

- `rbs-api` implements the types and JSON codec of the first slice, tokens and the token store, scopes, the session state machine (`Server`), the control-thread side (`ControlTarget`, `ControlEndpoint`, `EventPublisher` with `seq` per state key and the event slots), the `ControlTransport` port with the bounded `TransportInbox`, the shared `Origin`/`Host` check for WebSocket adapters (`UpgradePolicy`), and the API thread (`ApiServer`).
- `rbs-app` serves the API from its control thread (`StudioHost`): an `ApiTarget` implements `ControlTarget` for studio, source, edit, output, record and settings commands and publishes `source.changed`, `canvas.changed`, `settings.changed` and `record.state` events through one `EventPublisher`. API requests are drained after the UI's own commands. The control thread holds the authoritative copy of the recording profile; the UI sends it each change, and an API `settings.set` comes back to the UI, which still writes the settings files. The control thread serves it once remote control is on (below).
- `rbs-api-ws` serves the API over WebSocket on loopback: one I/O thread on the OS readiness poller (no periodic wake-up), the HTTP upgrade completed only after the `Origin`/`Host` check, its own RFC 6455 framing that skips a message over the connection's limit as it arrives, bounded output per connection (`Full`, then `Writable`; a connection whose output is backed up is not read, so pings from a peer that never reads cannot grow it), an upgrade deadline (10 s), a cap on connections still sending their upgrade (16) and on sockets in any state (128), refusals answered before the socket closes, and accepting paused briefly after an accept error such as running out of file descriptors. A non-loopback address is refused at start until TLS lands. It passes the transport conformance suite in `rbs_api::conformance`.
- `rbs-app` has a Remote control section in the Settings window: "Allow remote control" (off by default), the port (4456 by default, not obs-websocket's 4455; 1024 to 65535), and access tokens. Turning it on starts the API thread and `rbs-api-ws` on `127.0.0.1` and `::1` at that port and attaches the endpoint to the control thread; turning it off, or a port change, stops both (connected apps get 1001), so while off nothing of the API exists. A token is made with a name and one of three access levels: View only (`read`), Control (`read` + `control`, the default, for panels) and Full control (`read` + `control` + `admin`); `plugins` and `tokens` are not offered. The token is shown once with a Copy button; revoking it closes its sessions (Undo puts it back). Connections, authentication and privileged calls go to the developer log (stderr). The settings live in `remote-control.toml` in the app's config directory: only token hashes, and on Unix the file is mode 0600 (on Windows the per-user application data folder is user-only already). A file that cannot be read, or that other users can write, leaves remote control off and says why. `rbs-cli` does not serve the API yet.
- `rbs-api-ipc` implements the local IPC transport above (Unix domain socket, Windows named pipe) and a blocking client (`IpcClient`), and passes the transport conformance suite. `rbs-app` serves it, at the current user's endpoint, while remote control is on, next to the loopback WebSocket; not serving it (another RBS serves the endpoint, or its directory cannot be used) is logged and leaves the WebSocket running. While remote control is off nothing of it exists.
- `rbs-app` answers `tokens.list`, `tokens.create` and `tokens.revoke` on its control thread from `remote-control.toml` (`rbs_remote_settings::token_command`), also after the studio failed: under the settings lock, which it never waits for (`failed` with "try again" while another program holds it), it reads the file, makes the change and saves it, then gives the server the saved tokens (`rbs_api::TokenHandle`) before the lock is released and the answer is sent. The Settings window is told and shows the list as saved.
- `rbs-cli token list`, `rbs-cli token create <name> [--access view|control|full]` and `rbs-cli token revoke <name>` manage the same tokens in the same file, through the same code (`rbs-remote-settings`). `create` prints the token once, alone on standard output; `list` shows names and access levels, never tokens. A settings file that cannot be used is reported and left untouched. Names with control characters are refused. A change holds an advisory lock on `remote-control.lock` from reading the file to saving it, in the app too, and each save writes its own temporary file, so changes made at the same time are applied one after the other. If the token cannot be printed, it is revoked again. A deleted settings file means no tokens. The app reads the token list from the file again before each change it makes, when remote control starts and when its Settings window opens, so a change from `rbs-cli` is not undone by the app. While RBS runs with remote control on, `create` and `revoke` are sent to it over local IPC (`tokens.create`, `tokens.revoke`) and are in effect at once; `rbs-cli` changes the file itself only when nothing serves the IPC endpoint, or says why the running RBS could not be used and then changes the file. A change made in the file reaches a running app the next time its Settings window opens or remote control starts. A `revoke` is refused (non-zero exit, nothing changed) when RBS is running but could not be used, since a token revoked only in the file would keep working there. When `tokens.create` gets no answer (timeout, connection lost), `rbs-cli` revokes the name on a new connection, or says to check and revoke it. With `RBS_CONFIG_DIR` set, `rbs-cli` changes that directory's file and does not use IPC, since a running app may use another directory; an app started with `RBS_CONFIG_DIR` is reached over IPC by an `rbs-cli` without it, which then changes the app's file while `list` reads the default one. `list` always reads the file. The control thread answers `tokens.*` with file I/O (read, write, `fsync`), which can hold it for tens of milliseconds on a slow disk; token changes are rare.
- Not yet: TLS and network access; `edit.redo` (answers `failed` until the scene collection keeps a redo stack); concurrent settings edits from the UI and an API client are last-writer-wins, and an in-progress UI edit is replaced without notice;
  the `schema` feature with the generated JSON Schema, its snapshot test and difference classifier (`api.describe` lists methods, scopes and topics until then); `stats` sampling; `ui.*` routing; the U20 test, which needs the UI's own commands on `rbs-api` too; the idle-cost check by thread count (by construction today: off constructs nothing; on without a session sets no deadline, tested).

## Open questions

- Preview transport for remote clients: UI rule U5 requires a reduced-rate encoded preview for remote panels; WebRTC from the engine is the candidate.
- Stable addressing for layout commands. `rbs_ui::LayoutCommand` addresses windows and tab stacks by index, which shifts after each change; it is internal to the desktop shell. The `ui.*` commands for remote callers address panels by panel id (open, close, float, dock, move to an edge) and need stable window ids before they can target a window or stack.
- TLS certificates for network access: decided in [ADR-0046](../adr/0046-network-access-over-tls.md) (a generated key the client pins, or the user's own certificate; one port with TLS detected per connection); channel binding (RFC 9266) if pinning proves insufficient.
