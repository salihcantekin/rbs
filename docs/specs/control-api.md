# Spec: Control API

- Status: Draft
- Owner crate(s): `rbs-api` (planned)
- Related ADRs: [0008](../adr/0008-ui-as-api-client.md)

## Purpose

The single way to control and observe the engine, used by the desktop UI, web panels, CLI, Stream Deck integrations, scripts and tests.

## Shape

- **Commands** change state (add source, set transform, start stream). Every command is recorded in the command log and can be undone where meaningful.
- **Queries** read state (scene document, devices, adapter capabilities).
- **Events** stream changes and metrics to subscribers.
- Schema defined once (candidate: protobuf or a Rust-first schema with generated TypeScript and JSON Schema). Versioned with semver.
- Transports: local IPC for the desktop app; WebSocket with token authentication for remote clients.

## Compatibility

An OBS WebSocket compatibility layer (subset) is a plugin, so existing Stream Deck and remote tools can work with RBS.

## Open questions

- Schema technology choice (needs an ADR).
- Preview transport for remote clients (WebRTC from the engine is a candidate).
