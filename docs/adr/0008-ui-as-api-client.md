# ADR-0008: User interfaces are clients of the control API

- Status: Accepted; toolkit candidates clause superseded by [ADR-0016](0016-native-desktop-ui.md)
- Date: 2026-11-02

## Context

OBS's scene model and plugins depend on Qt; remote control was added later. We want desktop UI, web panels, CLI, Stream Deck and scripts to be equal citizens, and we want the UI toolkit to be replaceable.

## Decision

- The engine exposes a versioned control API: commands, queries, event subscriptions and metrics. Transport: local IPC for the desktop app, WebSocket for remote clients. The schema is defined once and generated for clients.
- The desktop UI is a separate crate that only talks to the engine through this API. The preview is a shared GPU surface, not pixels over IPC.
- Plugin settings are declared as schemas; UIs generate forms from them. Plugins never link a UI toolkit.
- Toolkit choice for the desktop app (Slint or Tauri are the candidates) is a separate, reversible decision.

## Consequences

- The engine can run headless on a server or as a background service.
- Every UI feature is automatable.
- Some UI features (preview, meters) need efficient streaming paths in the API.

## Alternatives considered

- UI and engine in one process with direct calls: simpler at first, recreates OBS's coupling.
