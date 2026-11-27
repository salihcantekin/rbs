# Spec: Stream Deck plugin

- Status: Draft
- Owner folder: `integrations/stream-deck/` (not a Rust crate)
- Related ADRs: [0198](../adr/0198-stream-deck-plugin.md), [0044](../adr/0044-control-api-protocol-and-security.md), [0013](../adr/0013-zero-cost-when-unused.md)

## Purpose

Let a user press Elgato Stream Deck keys to control RBS and see RBS's state on the keys. The plugin is a client of the [Control API](control-api.md); RBS itself does not change.

## Scope

In scope: the plugin's actions, their settings and pictures, how it connects, authenticates and recovers, how it is built, packaged and tested.

Out of scope: OBS WebSocket compatibility, touch-strip layouts beyond the built-in volume layout, pairing or discovery, network (non-loopback) connections, Marketplace submission, other languages.

## Requirements on the user's side

- Stream Deck app 7.1 or newer (Node.js 24 runtime), Windows 10 or macOS 12 or newer.
- RBS with *Settings > Remote control > Allow remote control* on, and a token of access level *Control* (`read` + `control`). A *View only* token shows state but every press fails with `scope_denied`.
- Recording and streaming starts are `live` actions. While an output is active, every action of the plugin changes what is on air (scene switch, source visibility, mute, master mute). A token without "Allow actions that cannot be undone" gets `not_permitted` (`irreversible` or `on_air`); the plugin shows the alert state and the reason in the property inspector (see Errors). The permission is a trade-off the user makes with the token: without it the keys work only while nothing records or streams.
- Stream Deck stores the plugin's global settings (including the token) as plain text in its own settings folder, readable by the same user's processes. The plugin therefore never logs the token (its logger redacts it), and the inspector tells the user to use a token made for the plugin, which can be revoked on its own.

## Layout of the package

```
integrations/stream-deck/
  package.json, package-lock.json, tsconfig.json, rollup.config.mjs
  scripts/gen-types.mjs             schemas -> src/rbs-client/generated.ts
  src/plugin.ts                     entry: registers actions, wires connection and actions
  src/rbs-client/                   no Stream Deck imports (client library)
  src/actions/                      one file per action: behaviour, settings type, picture
  src/pictures/                     SVG builders and the tokens file
  com.rbs.streamdeck.sdPlugin/      manifest.json, ui/ (property inspector), imgs/, bin/ (build output)
  test/                             vitest
```

## Connection

- **Target.** `ws://127.0.0.1:<port>`, subprotocol `rbs.v1`; port default 4456. The host is fixed; the client refuses to send a token anywhere but a loopback address (spec: *Channels*). The token and port are the plugin's global settings, edited in the property inspector of any RBS action.
- **Lazy.** No socket exists while no RBS key is visible. The first `onWillAppear` connects; after the last `onWillDisappear` the client waits 10 s, then closes (1000). A new appearance within the grace period cancels the close.
- **Handshake.** `session.hello {protocols: ["1.0"], client: {name: "rbs-stream-deck", version}}`, then `session.authenticate {token}`. A request error `unsupported_protocol` (-32003) on `session.hello` or close 4003 is `incompatible` (plugin too old); `auth_failed` (-32002, then close 4002) is `auth_failed`. The answer's `protocol` (the server's) must be major 1 and minor 6 or higher, checked by the client (`incompatible`, RBS too old, otherwise). The socket is opened without an `Origin` header (the server refuses any) and with `Host` a literal loopback address. The scopes of the authenticate answer are kept; a missing `control` shows `read_only` in the property inspector.
- **Subscribe, then snapshot.** `events.subscribe {topics: ["scene.changed", "source.changed", "record.state", "stream.state", "replay.changed", "vcam.changed"]}`, then `scene.list`, `source.list`, `record.status`, `stream.status`, `output.profiles` and, when the server has them, `replay.status` and `vcam.status` (a `method_not_found` there is ignored). Every state answer (all but `output.profiles`, which is a plain list) is `{seq: {key: n}, data}`; every event is `{topic, seq, data}` carrying the whole state of its key. The client holds the latest `data` per key and applies an event or snapshot only when its `seq` is higher than the held one (equal or lower is dropped; skipped values are normal). `seq` restarts with each server run (`instance`), so every new connection clears all held `seq` values and data before it subscribes; nothing carries over a reconnect. Event payloads: `scene.changed` is the `scene.list` data, `source.changed` the `source.list` data, `record.state` and `stream.state` the `record.status` and `stream.status` data of the main canvas.
- **Main canvas only.** Keys follow the main canvas: `canvas` is never sent (the default is main) and `all` is never used; a vertical canvas's recording or stream (`*.canvas_state` topics) is not shown in this version.
- **State of the link**, shown on every key and in the property inspector:

| Link state | Meaning | Keys show | Retry |
| --- | --- | --- | --- |
| `idle` | no visible key | n/a | no socket |
| `connecting` | socket opening, handshake or snapshot running | RBS logo, dimmed | n/a |
| `ready` | authenticated, subscribed, snapshot applied | live state | n/a |
| `no_token` | no token saved | "Set token" | none until settings change |
| `unreachable` | connect refused or closed | "Not connected" (the inspector hints at RBS running and *Allow remote control*) | back-off below |
| `auth_failed` | close 4002 or `auth_failed` | "Token rejected" | none until the token changes |
| `revoked` | close 4005 | "Token revoked" | none until the token changes |
| `incompatible` | protocol major or minor out of range, or close 4003 | "Update RBS" or "Update plugin" | none until reconnect is asked (settings change or key press) |

- **Back-off** for `unreachable` (also 4001, 4004, 4006, 1001, network errors): 1 s, doubling, capped at 15 s, reset after a link reached `ready`. Timers are injected (`Clock`), never real in tests. A key press while the link is not `ready` and not terminal asks for an immediate retry.
- **Requests** carry increasing integer ids; a response matches by id. A request has a 10 s timeout (`timeout` error) and fails with `closed` when the socket closes. At most 8 requests are in flight. Unknown response fields are ignored; unknown error kinds are shown generically.

## Actions

All actions are keys (`Controllers: ["Keypad"]`) except the volume dial (`Encoder`, Stream Deck +). The action UUID is the plugin UUID plus the suffix.

| Suffix | Press | Settings | Picture |
| --- | --- | --- | --- |
| `scene` | `scene.switch {scene}` | scene (id and the name last seen) | scene name; accent highlight while it is the program scene; dimmed when the scene no longer exists |
| `record` | idle: `record.start {profile}`; recording: `record.stop` | none (uses the profile selected in RBS) | idle: record icon; recording: red icon with elapsed time; paused: paused icon |
| `stream` | idle: `stream.start`; live: `stream.stop` | none | idle: broadcast icon; live: red icon, "Connecting" until `connected`, "Reconnecting" while it retries |
| `source-visible` | `item.set_visible {item, visible: !visible}` on the source's item in the program scene | source (id and last name) | eye open or closed; dimmed with "Not in scene" when the source has no item in the program scene |
| `record-pause` | `record.pause_toggle` while recording; nothing otherwise | none | Pause icon, Resume while paused, dimmed when not recording |
| `scene-step` | `scene.switch` to the next or previous scene that navigation reaches (`active` not false), wrapping | direction | arrow with the target scene's name |
| `replay` | `replay.save`; refused when the buffer is not armed | none | dimmed "Replay off", "Save replay", "Saving" |
| `vcam` | `vcam.start` or `vcam.stop` | none | accent while sending |
| `marker` | `marker.add` | none | dimmed while not recording |
| `volume` (dial) | turn: `source.set_volume` or `canvas.set_master_volume`, a step of 1 to 25 percent per tick; press: mute | source or Master, step | built-in `$B1` layout: title, percent or Muted, bar |
| `mute` | `source.set_muted {source, muted: !muted}`, or `canvas.set_master_muted` for the target Master | target: Master or a source | speaker or crossed speaker |

Master state is the `master {volume, muted}` field of the scene list (absent means not muted; protocol 1.15). Below 1.15 the Master target is shown as unavailable. A source's `muted` is known only for sources in the program scene (`source.changed` lists those); for another source the key shows its state as unknown (neutral picture) and a press sends the opposite of the last known value, defaulting to mute.

- **Source targets are ids**, not names, so a rename keeps the key working. The plugin keeps the last seen name in the action settings only to label a key whose source is missing.
- **Source visibility** acts on the program scene (`source.changed` is the program scene's list): `item` comes from the list entry whose `id` equals the key's source. Changing the program scene re-evaluates every key.
- **Titles.** Pictures contain the text (scene or source name, ellipsised to fit); the Stream Deck title stays empty and the user's own title, if set, is drawn by the app on top.
- **Elapsed time** is not streamed: `recorded_ms` and `paused_ms` arrive only when the state changes. The key computes the time itself from the event's `recorded_ms` and the local time it arrived, with a 1 s timer that exists only while a recording runs and a record key is visible (P13) and is paused with the recording.
- **Pictures** are compared with the last one sent to the key; `setImage` is called only when the picture changed (every `source.changed` re-evaluates all keys).
- **Stale presses.** A press decided on held state (for example start while RBS already records) returns `conflict`; this is expected, the key alerts and is not retried.
- **Multiple keys** for the same target are independent and all follow the same state.
- **Press handling.** The call is sent on key down. While a call is in flight further presses on that key are ignored. A failed call shows the alert state (`showAlert`); `not_permitted`, `scope_denied`, `not_found`, `conflict` and `failed` also set the key's last-error text, which the property inspector shows (kind and `data.reason`).

## Property inspector

HTML in `ui/` using Elgato's sdpi-components (vendored locally, not loaded from the network). Every action shows the **Connection** section (port, token as a password field, status line with the link state and the RBS version) bound to global settings, then its own fields. Lists (scenes; every source of the collection, not only the scene on air, plus Master for mute and volume) come from the plugin, which answers the inspector's `getScenes`, `getSources` and `getMuteTargets` requests from the held state and refreshes the inspector when the state changes. With no connection the lists are empty and the status line says why.

## Errors

| Situation | Behaviour |
| --- | --- |
| Request error `scope_denied` | alert; status line: "This token cannot do this: needs Control access" |
| `not_permitted` (`irreversible`, `on_air`) | alert; "Turn on 'Allow actions that cannot be undone' for this token" |
| `not_found` for a scene or source | picture dims; alert on press |
| `conflict`, `failed`, `busy`, `rate_limited` | alert; kind shown in the status line |
| Socket closes during a call | alert; the call is not retried (a press is an intent at one moment) |

## Zero cost

| State | Cost |
| --- | --- |
| Installed (plugin present, no RBS key placed) | Stream Deck starts the plugin process (Node), no socket, no timers |
| Standby (RBS keys placed, RBS not running) | One failing connect per back-off step (at most every 15 s) |
| Active | One WebSocket; four state topics; work only on events |

## Build, package, tests

- Supply chain: `package-lock.json` is checked in, installs use `npm ci --ignore-scripts`, and the Stream Deck CLI and rollup are dev dependencies only.
- Transport choice: WebSocket, not local IPC (a named pipe works from Node on Windows, but WebSocket is the same on every platform and needs no extra code).
- `npm ci`, `npm run build` (rollup bundles `src/plugin.ts` to `bin/plugin.js`), `npm test` (vitest), `npm run pack` (`streamdeck pack` writes `dist/com.rbs.streamdeck.streamDeckPlugin`), `npm run gen-types` (schemas to `src/rbs-client/generated.ts`; `npm run check-types` and `npm test` fail when the checked-in file differs; neither `cargo xtask verify` nor CI runs them (CI is manual), so whoever changes a schema the plugin uses runs `npm test` in the folder).
- **Tests.** Client: a fake server (`ws`, ephemeral loopback port) covers handshake, wrong token (4002), protocol mismatch, `seq` filtering, reconnect back-off with an injected clock, request timeout, close during a request, token refused for a non-loopback host. Pictures and state mapping: pure functions. No test sleeps; waiting is on events.
- **Install for testing** (Windows or macOS): double-click `com.rbs.streamdeck.streamDeckPlugin`; or, for development, `npm i -g @elgato/cli`, `streamdeck link com.rbs.streamdeck.sdPlugin`, `npm run watch`.

## Implementation status

Draft: not implemented. This spec and ADR-0198 come first; the client, actions and package follow in the same folder.

**Pictures (0.2).** Recording and live keys blink their dot once a second and show the time (the stream's time only when its start was seen while connected); the 1 s timer exists only while such a key is visible. A scene key takes an optional colour for its active state. When the user sets a Stream Deck title on a key, the picture has no text.
