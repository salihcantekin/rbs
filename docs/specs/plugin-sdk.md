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
| `Panel` | UI panels, declared as a tree of design-system components and rendered natively by the host (ADR-0016) |
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
2. Settings are declared as a schema; the UI generates forms. Each setting declares a disclosure `level` (`simple`, `advanced`, `expert`; UI rule U2). Plugins never depend on a UI toolkit.
2a. Dock panels are provided only by sandbox- or process-tier plugins, run isolated from the main UI, are described declaratively and rendered natively by the host (no HTML, CSS or JavaScript), and are replaced by an error card if they crash or hang (UI rule U14).
3. Frames and audio buffers are lent to the plugin for the duration of a call. Plugins cannot change timestamps.
4. Each call has a time budget (default 2 ms per video frame for filters). Over-budget plugins are reported, then bypassed.
5. Plugins request permissions in the manifest; the user approves them at install time.
6. Plugins follow the Installed / Standby / Active lifecycle ([ADR-0013](../adr/0013-zero-cost-when-unused.md)). Before first use only the manifest is read: native libraries are not loaded, WASM modules are not compiled, processes are not started. Plugins must release everything on deactivation and must not start background threads or timers while not Active, except through host-provided lazy services.
7. A plugin registry with signature checks and a `cargo generate` template is part of the 1.0 scope.

## Panels

Plugin panels are declarative and rendered natively by the host (ADR-0016, UI rule U14).

- **Interface:** a versioned WIT world `rbs:panel@1.x`, available to sandbox- and process-tier plugins (the process tier speaks the same messages over IPC). Versioned with the plugin SDK under semver.
- **Content:** the plugin sends a tree of nodes. Each node is one component from the [design system](ui-design-system.md#components) list (layout containers, text, buttons, fields, lists, property grids, toggles, sliders) with typed properties. No free-form drawing, colours or fonts; semantic variants only.
- **Host components:** charts, audio meters and live video previews are host components the plugin references by data source (a metric id, an audio source, a source or canvas id). Pixels and samples never pass through the plugin.
- **State and updates:** the host keeps the tree. The plugin sends the full tree once, then patches (insert, remove, set property by node id). Updates are applied at most 30 times per second per panel; extra patches are coalesced.
- **Events:** user actions arrive at the plugin as `action(node_id, kind, value)` callbacks with no direct access to the UI thread.
- **Lifecycle:** a hidden panel moves to Standby (ADR-0013): no events, patches are queued and coalesced, and the plugin is told so it can stop work. A crashed or unresponsive panel is replaced by an error card with a reload button.
- **Accessibility and i18n:** every node with text carries a Fluent message id or plugin-provided localised string; interactive nodes require an accessible label.

## Open questions

- Shared GPU access for sandboxed plugins beyond WGSL passes.
- Distribution format and update channel for the registry.
