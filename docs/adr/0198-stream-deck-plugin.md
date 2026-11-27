# ADR-0198: Stream Deck plugin as a Control API client in `integrations/stream-deck/`

- Status: Accepted (2026-11-27)
- Date: 2026-11-27
- Deciders: Salih (owner)
- Refines: [ADR-0044](0044-control-api-protocol-and-security.md) (Control API, tokens, events), [ADR-0006](0006-three-tier-plugin-model.md) (add-ons stay outside the core), [ADR-0013](0013-zero-cost-when-unused.md)

## Context

The owner wants Elgato Stream Deck buttons to drive RBS (switch scene, start and stop recording and streaming, show or hide a source, mute) and to show RBS's state on the key (active scene, recording, live, muted). OBS plugin compatibility is not wanted: RBS's own API is richer (full-state events with `seq`, fine-grained scopes, typed schemas), so the plugin is written for RBS (owner, 2026-11-27).

A Stream Deck plugin is a separate process started by the Stream Deck app. Elgato's Node.js SDK (`@elgato/streamdeck`, 3.x, Node 24, Stream Deck 7.1 or newer) talks to the app; the plugin opens its own connection to RBS. The Control API already offers everything needed (analysis in the owner's thread of 2026-11-27: transport, handshake, tokens, events, 192 methods). Nothing has to change inside RBS.

## Decision

1. **A separate package in this repository**, `integrations/stream-deck/`: TypeScript, Elgato Node.js SDK, its own `package.json` and lockfile. It is not part of the Cargo workspace, no Rust crate depends on it, and `cargo xtask` does not build it. The Control API protocol ([spec](../specs/control-api.md)) is the only contract between the two. The plugin's behaviour is specified in [stream-deck.md](../specs/stream-deck.md).
2. **A plain Control API client.** The plugin connects over WebSocket to loopback (default port 4456), speaks `rbs.v1`, authenticates with a token of access level *Control* (`read` + `control`), and calls only methods that exist in the checked-in schemas. It adds no method, scope or event to RBS. A change that would need one is a separate change with its own spec update.
3. **Token by hand.** The user pastes the token and port in the property inspector; they are stored as the plugin's *global settings*, never in action settings (action settings are exported with profiles). No pairing flow in this version. The client sends the token only to a loopback address; it refuses any other host.
4. **The RBS client is a library inside the package** (`src/rbs-client/`) that imports nothing from Stream Deck, so it can become its own package for third parties later. It does the handshake, request and response matching, subscriptions, `seq` filtering, reconnect with back-off and the protocol version check. Its request and answer types are generated from `crates/rbs-api/schemas` by a script; the generated file is checked in and `npm test` fails when it is out of date. Neither `cargo xtask verify` nor CI (manual) runs it, so a schema change does not by itself touch the plugin: whoever changes a schema of a method the plugin calls runs `npm test` in the folder.
5. **Zero cost when unused** (P13). No connection exists while no RBS key is visible on any deck. The first visible key connects; after the last one disappears the connection closes after a short grace period (page switches on the deck must not reconnect).
6. **Key pictures are generated** as SVG by the plugin from the state (one function per action, colours and sizes in one tokens file), so there are no per-state image files to keep in sync. Visuals are proposed to the owner before they ship (CLAUDE.md rules 15 and 16 apply by analogy to the plugin's pictures).
7. **Compatibility.** The plugin requires protocol major 1 and at least minor 6 (scenes, stream) and ignores unknown fields, enum values and error kinds. A server below that, or with another major, shows a "RBS too old" or "plugin too old" state on the keys.
8. **Distribution.** A `.streamDeckPlugin` built with the Stream Deck CLI (`streamdeck pack`) that the owner installs by double-click. Marketplace submission (Maker Console account, plugin UUID, icons, DRM) is a later step with its own decision; the plugin UUID is provisional (`com.rbs.streamdeck`) until then, because it cannot change after publishing.
9. **Repository tooling.** `cargo xtask affected` treats `integrations/` as never compiled (like `docs/`), so a change there selects no crate; `.gitignore` covers `node_modules`, `dist` and the build output. Installs use the checked-in lockfile with `npm ci --ignore-scripts`.
10. **Tests are fast.** The client is tested against an in-process fake WebSocket server on an ephemeral loopback port with injected timers; no test sleeps. Action rendering and state mapping are pure functions with unit tests. The plugin is checked on a real Stream Deck by the owner (Windows).

## Consequences

- The Control API gets a real third-party client, which tests its documentation and typed schemas.
- Users need Stream Deck 7.1 or newer (Node 24 runtime) and a Control token from Settings > Remote control.
- A protocol change that removes or renames a method the plugin uses must update the plugin in the same change; the drift test (decision 4) shows it when run.
- Stream Deck keeps the token in plain text in its settings; mitigated by a plugin-only revocable token (spec).
- GitHub CI does not build the package (CI stays manual, owner decision D3); `npm test` in the folder is the check.

## Alternatives considered

- **OBS WebSocket compatibility adapter in RBS.** Would reuse Elgato's OBS plugin, but needs a new adapter crate, speaks a poorer protocol and hides RBS's own features. Rejected by the owner.
- **A Rust or C# plugin binary.** Needs per-OS binaries and signing. The Node SDK is the lowest-friction path and the API is JSON-RPC.
- **A separate repository.** Slower to keep in step with protocol changes and schemas; a folder here lets one change update both.
- **Pairing flow or a port file.** Better first-run experience, but a visible RBS change that needs its own ADR; pasting is enough for the first version.

## Review

Round 1 (independent AI session, 2026-11-27): changes requested; all 10 concerns applied (seq reset per connection, `affected` ignores `integrations/`, drift check scope stated, on-air permission for every action, master mute and protocol 1.15, main canvas only, locally computed elapsed time, handshake error mapping, token storage and Origin/Host notes, stale presses). Owner approved 2026-11-27.
