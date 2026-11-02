# ADR-0006: Three-tier plugin model

- Status: Accepted
- Date: 2026-11-02

## Context

OBS plugins run in-process as native code against an unstable ABI: one bad plugin crashes everything and updates break plugins. A single model cannot serve both trusted, performance-critical code (encoders, capture) and untrusted community code (overlays, chat, automation).

## Decision

One set of node and adapter interfaces, three runtime tiers chosen by the plugin manifest:

| Tier | Technology | Typical use |
| --- | --- | --- |
| Native | `cdylib` with a stable C ABI (`#[repr(C)]` vtables, versioned) | Encoders, capture backends, GPU filters from trusted authors |
| Sandbox | WebAssembly components on `wasmtime`, interfaces defined in WIT | Audio filters, text/overlay sources, automation, integrations |
| Process | Separate process with shared memory, shared GPU textures and IPC | Browser source (CEF), AI models, vendor SDKs (NDI, Decklink, VST3), remote guests |

Details: [plugin SDK spec](../specs/plugin-sdk.md).

## Consequences

- Untrusted plugins cannot crash the engine; native plugins are signed and opt-in.
- Plugins can be written in any language that compiles to WASM components.
- Three hosts to maintain; the sandbox and process tiers can arrive after the native tier.

## Alternatives considered

- Native only: fast, but repeats OBS's stability problems.
- WASM only: safe, but GPU access and vendor SDKs become impossible or slow.
- Loading libobs plugins: huge GPL API surface, ties us to OBS internals. Rejected; we provide importers instead.
