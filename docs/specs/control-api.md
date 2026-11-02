# Spec: Control API

- Status: Draft
- Owner crate(s): `rbs-api` (planned)
- Related ADRs: [0008](../adr/0008-ui-as-api-client.md)

## Purpose

The single way to control and observe the engine, used by the desktop UI, remote clients, CLI, Stream Deck integrations, scripts and tests.

## Shape

- **Commands** change state (add source, set transform, start stream). Every engine command is recorded in the command log and can be undone where meaningful. Client-scoped `ui.*` commands are the exception (see below).
- **Queries** read state (scene document, devices, adapter capabilities).
- **Events** stream changes and metrics to subscribers.
- Schema defined once (candidate: protobuf or a Rust-first schema with generated TypeScript and JSON Schema). Versioned with semver.
- Transports: local IPC for the desktop app; WebSocket with token authentication for remote clients.

## Client-scoped UI commands

Some commands change the state of one UI client, not of the engine: for example `ui.workspace.list`, `ui.workspace.switch(name)`, `ui.panel.open(id)` and `ui.panel.close(id)`. They live in the `ui.*` namespace with these rules:

- They are not written to the engine command log and are not undoable through it. The client keeps its own layout undo history (UI rule U21).
- The engine only routes them. Each connected UI client registers with a client id and a display name ("Studio PC"). A caller such as a Stream Deck plugin sends a `ui.*` command with a target client id, or without one to reach the desktop client that was focused most recently.
- The engine stores no UI state for them; layout and workspaces belong to the client (UI design system spec, "Panels and workspaces").

## Compatibility

An OBS WebSocket compatibility layer (subset) is a plugin, so existing Stream Deck and remote tools can work with RBS.

## Open questions

- Schema technology choice (needs an ADR).
- Preview transport for remote clients: UI rule U5 requires a reduced-rate encoded preview for remote panels; WebRTC from the engine is the candidate.
- Every UI command must exist as a control API command (UI rule U20).
