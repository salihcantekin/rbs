# Spec: Plugin SDK

- Status: Draft
- Owner crate(s): `rbs-plugin-sdk`, `rbs-plugin-host` (planned)
- Related ADRs: [0006](../adr/0006-three-tier-plugin-model.md), [0003](../adr/0003-ports-and-adapters.md)

## Purpose

Let third parties extend RBS with new nodes, adapters, panels and automation, in a way that survives application updates and cannot crash the engine unless the user explicitly trusts the plugin.

## Extension points

| Extension point | Provides |
| --- | --- |
| `Source`, `VideoFilter`, `AudioFilter`, `Transition` | Graph nodes |
| Port adapters (capture, audio I/O, codec, mux, transport) | Backends, through the same traits as built-in adapters |
| `Service` | Platform integration: stream keys, chat, events |
| `Panel` | UI panels, declared as a schema or as a web view |
| `Automation` | Reactions to events, macros (Lua, JS or WASM) |

## Manifest

Every plugin ships `rbs-plugin.toml`:

```toml
id = "com.example.chroma-pro"
name = "Chroma Pro"
version = "1.2.0"
sdk = "^1.0"            # supported SDK range
tier = "sandbox"        # native | sandbox | process
provides = ["video-filter"]
permissions = ["gpu-shader"]   # network, filesystem:read, camera, microphone, ...
```

## Tiers

- **Native**: `cdylib` exporting `rbs_plugin_entry_v1` that returns a `#[repr(C)]` vtable. Only Rust and C types with fixed layout cross the boundary. Must be signed or explicitly trusted by the user.
- **Sandbox**: WebAssembly component targeting the `rbs:plugin@1.x` WIT world. GPU work is expressed as WGSL shaders and passes run by the host. CPU and memory limits per instance.
- **Process**: separate executable launched by the host; frames shared through GPU texture sharing or shared memory, control over IPC. Restarted on crash.

## Rules

1. The SDK is semver-versioned. The host supports the current and previous major version.
2. Settings are declared as a schema; the UI generates forms. Plugins never depend on a UI toolkit.
3. Frames and audio buffers are lent to the plugin for the duration of a call. Plugins cannot change timestamps.
4. Each call has a time budget (default 2 ms per video frame for filters). Over-budget plugins are reported, then bypassed.
5. Plugins request permissions in the manifest; the user approves them at install time.
6. A plugin registry with signature checks and a `cargo generate` template is part of the 1.0 scope.

## Open questions

- Shared GPU access for sandboxed plugins beyond WGSL passes.
- Distribution format and update channel for the registry.
