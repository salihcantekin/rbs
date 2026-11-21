# ADR-0162: Extension-ready registries, and the order the plugin tiers arrive in

- Status: Proposed
- Date: 2026-11-21
- Deciders: Salih (owner)
- Builds on: ADR-0003 (ports and adapters), ADR-0006 (three-tier plugin model), ADR-0013 (zero cost when unused), ADR-0023 (parameter channels), ADR-0145 (add-ons), ADR-0160 (one platform adapter list)
- Supersedes in part: ADR-0006, the consequence "the sandbox and process tiers can arrive after the native tier" (decision 2 below reverses the order). The three tiers themselves are unchanged.

## Context

ADR-0006 chose three plugin tiers and the [plugin SDK spec](../specs/plugin-sdk.md) (Draft) lists the extension points. In the tree only the manifest exists: `rbs-plugin-sdk` parses `rbs-plugin.toml` and `rbs-plugin-host` reads installed manifests and loads nothing (`docs/status.md`). The owner does not want the loader now (2026-11-21), but wants two things written down:

1. What we will do when plugins are built, so the work is known.
2. That every feature built from now on fits that plan instead of making it harder.

Some of the plumbing is already open to new types and some is closed:

| Part | Today | Open to a plugin? |
| --- | --- | --- |
| Adapter registry (`rbs_core::registry::Registry`) | Filled at start-up by `rbs-platform` | Yes |
| Node type table (`rbs_sources::NodeTypeTable`) | Filled by `SourceTypes::node_types` | Yes |
| Scene file: sources and filters | Stored by type id string (`NodeTypeId`) | Yes |
| Video filter types | `rbs_filters::catalogue() -> &'static [&'static dyn FilterType]` | No, a compiled list |
| Audio filter types | `rbs_audio_filters::catalogue()`, same shape | No |
| Source classes | `rbs_sources::SourceClass` enum; `rbs_api::SourceKind` enum | No |
| Transitions | `rbs_scene::TransitionKind` enum, saved in the scene format | No |

Each closed list was right when it was written. The risk is that every new feature adds another closed `match` in the host or engine, and the cost of opening them grows. OBS shows the opposite failure: plugins reach into internals (libobs), so updates break them and one bad plugin takes the program down (ADR-0006 context).

## Decision

1. **Plugins see a contract of their own, never our internal traits.** The plugin contract is the versioned `rbs:plugin@1` WIT world for the sandbox tier, the same messages over IPC for the process tier, and a versioned `#[repr(C)]` vtable for the native tier (plugin SDK spec). `rbs-plugin-host` wraps a plugin into the existing internal traits (`FilterType`, `AudioFilterType`, `NodeDescriptor`, `AdapterFactory`). Core, port and engine crates must not depend on `rbs-plugin-host` or on a WASM runtime; only the composition roots wire the host in (P1, as ADR-0160 does for adapters). Internal traits may change between releases; the plugin contract follows P9.

2. **Tier order: sandbox first, process second, native last.** Sandbox (WASM on `wasmtime`, GPU work as WGSL passes the host runs) is the first loader, because it cannot crash the engine and covers filters, audio filters and automation. Process comes second and generalises what the browser source already proved (`rbs-browser-ipc`, `rbs-browser-shm`, supervisor and heartbeat, ADR-0109 and ADR-0157): AI models, NDI, VST3/CLAP. Native comes last, signed or explicitly trusted (ADR-0006).

3. **Plugins are installed through Add-ons.** A plugin is an add-on of kind `plugin` (ADR-0145, `add-ons.md`: `Kind` is "later: Plugin"). Download, checksum, unpack, status and remove are the add-ons code; there is no second store. The manifest's permissions are shown and approved at install.

4. **Every list a plugin could extend is a runtime registry.** Video filter types, audio filter types and node types, then source types and transitions, are registries filled at start-up. Built-in types register through the same call a plugin type would use; the only privilege of a built-in is the reserved `org.rbs.*` id prefix. Code looks types up by id and must not `match` on a closed list of built-in types in the engine or the studio host to decide behaviour; behaviour comes from what the type declares (fields, pins, activity traits, capabilities).

5. **Saved documents name types by id and survive a missing type.** Scene files, looks and API payloads name a type by its reverse-DNS id string. A document that names a type nobody registered (a plugin was removed) must load, keep that item and its settings unchanged, show it as missing with one sentence, and save it back unchanged.

6. **Settings and UI come from a schema.** A type declares its settings with `rbs-params` (unit, range, default, help, disclosure level). The Properties form, the control API and MCP are generated from it. Plugins never draw UI: a panel is a tree of design-system components the host renders with theme tokens (plugin SDK spec, "Panels"; ADR-0015, ADR-0057). So plugin UI cannot break the token rules.

7. **Zero cost before use.** Registering a plugin type reads only its manifest: the name, fields and pins the UI lists come from declarations, not from running code. WASM is compiled, a process is started or a library is loaded only when an instance becomes Active (ADR-0013, plugin SDK rule 6). Per-call time budgets, bypass and restart follow P7 and plugin SDK rule 4.

8. **New features are built extension-ready (from 2026-11-21).** A new source, filter, transition, output, adapter or automation trigger must: (a) register through a registry from decision 4, or, while that registry does not exist yet, keep its built-in special-casing in one place named in its ADR; (b) be stored and addressed by a type id; (c) declare its settings as a schema; (d) use only what a plugin of the matching tier could also be given (ports, lent frames and buffers, host services), not another feature's internals. A feature that cannot meet one of these says why in its ADR, and the reviewer checks it. A feature does not have to ship as a plugin.

9. **Rollout. Each step is its own pull request; none starts before this ADR is Accepted.**
   1. Video and audio filter catalogues become registries (`FilterRegistry`, `AudioFilterRegistry`) pre-filled with today's built-ins; callers (`rbs-sources`, `rbs-studio-host`, `rbs-app`, tests and benches) take the registry. No behaviour change; binding only, not `Node::process`, so not a hot path.
   2. Source types: a registry beside `SourceClass`, and the missing-type behaviour of decision 5 with tests. `rbs_api::SourceKind` gains a type-id form (control API change, own review).
   3. Sandbox loader: the `rbs:plugin@1` world for video filters (WGSL) and audio filters, `wasmtime` in `rbs-plugin-host`, add-on kind `plugin`. The WIT world and SDK 1.0 get their own ADR.
   4. Process tier: the browser runner's IPC and shared memory generalised into a process-plugin host. Own ADR.
   5. Transitions as a registry, automation triggers, then the native tier with signing. Own ADRs.

## Consequences

- Later plugin work is known and ordered; steps 1 and 2 are small and can land well before any loader.
- Each new feature carries a little more work (a registry entry, a schema, a type id), and its ADR has one more thing to state.
- The control API and scene format keep their current enums until step 2; adding the id form is a contract change with its own review (P14).
- `wasmtime` is a large dependency; it lives only in `rbs-plugin-host`, which only composition roots use.
- Follow-ups in this change: plugin SDK spec "Registries" and "Order" sections; principle P16; CLAUDE.md rule 19. Later: `add-ons.md` `Kind::Plugin` (step 3), `scene-model.md` missing-type rule (step 2), `video-filters.md` and `audio-filters.md` catalogue sections (step 1).

## Alternatives considered

- **Native tier first, as ADR-0006 allowed.** Fastest path to GPU filters, but it repeats OBS's crash and ABI problems before the safe tiers exist, and needs signing first.
- **Open the registries only when the loader is built.** Every feature until then adds closed lists; the later change touches more code and contracts.
- **Load OBS plugins.** Rejected in ADR-0006; still rejected.
- **A scripting language first (Lua or JS) instead of WASM.** Lighter to start, but a second runtime with its own sandbox; WASM components already cover scripts compiled from many languages.

## Review

Pending: independent review per ADR-0014 (`architecture-review` skill), then owner approval.
