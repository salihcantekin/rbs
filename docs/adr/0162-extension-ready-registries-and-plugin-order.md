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
| Node type table (`rbs_sources::NodeTypeTable`) | Rebuilt by `SourceTypes::node_types`, which matches `SourceClass` to pick descriptors; `live_settings`, `live_keys` and `activity` match `class_of(type_id)` | Partly: ids are open, behaviour comes from `SourceClass`, and an entry added from outside is lost on the next rebuild |
| Scene file: sources and filters | Stored by type id string (`NodeTypeId`); an unknown type keeps its item and settings (`BindProblem::UnknownSourceType` / `UnknownFilterType`); the API shows it with `SourceInfo.kind: null` | Yes |
| Scene file: transitions | `rbs-user-data` `scenes.rs` refuses the whole document on an unknown transition kind (`BodyError`) | No |
| Video filter types | `rbs_filters::catalogue() -> &'static [&'static dyn FilterType]`; `FilterType::name`, `fields` and `passes` return `&'static` data | No, a compiled list, and the trait assumes compiled data |
| Audio filter types | `rbs_audio_filters::catalogue()`, same shape (`name`, `fields` are `&'static`) | No |
| Source classes | `rbs_sources::SourceClass` enum; `rbs_api::SourceKind` enum | No |
| Transitions | `rbs_scene::TransitionKind` enum, saved in the scene format | No |

Each closed list was right when it was written. The risk is that every new feature adds another closed `match` in the host or engine, and the cost of opening them grows. OBS shows the opposite failure: plugins reach into internals (libobs), so updates break them and one bad plugin takes the program down (ADR-0006 context).

## Decision

1. **Plugins see a contract of their own, never our internal traits.** The plugin contract is the versioned `rbs:plugin@1` WIT world for the sandbox tier, the same messages over IPC for the process tier, and a versioned `#[repr(C)]` vtable for the native tier (plugin SDK spec). `rbs-plugin-host` wraps a plugin into the existing internal traits (`FilterType`, `AudioFilterType`, `NodeDescriptor`, `AdapterFactory`). Core, port and engine crates must not depend on `rbs-plugin-host` or on a WASM runtime; only the composition roots wire the host in (P1, as ADR-0160 does for adapters). Internal traits may change between releases; the plugin contract follows P9.

2. **Tier order: sandbox first, process second, native last.** Sandbox (WASM on `wasmtime`, GPU work as WGSL passes the host runs) is the first loader, because it cannot crash the engine and covers video filters and automation first. Sandbox code on a real-time thread (an audio block, a filter's `uniforms` on the render thread) is allowed only under decision 2a. Process comes second and generalises what the browser source already proved (`rbs-browser-ipc`, `rbs-browser-shm`, supervisor and heartbeat, ADR-0109 and ADR-0157): AI models, NDI, VST3/CLAP. Native comes last, signed or explicitly trusted (ADR-0006).

   2a. **Plugin code on real-time threads (P6, P7).** A plugin call on the audio thread or the render thread must not grow memory after instantiation (linear memory sized at instantiation), must not make host calls that allocate, lock or do I/O, and is bounded (epoch or fuel) against its per-call budget. A missed budget or a trap bypasses that one node (last good frame or silence) and reports it; other nodes keep running. The exact limits are part of the WIT ADR (step 3), which may defer audio-thread plugins; until that ADR, no plugin code runs on a real-time thread. So the first sandbox video filters are data-only: WGSL passes plus a value-to-uniform mapping declared in the manifest (field key to uniform slot, with a fixed set of conversions), which the host evaluates the way built-in `uniforms` does; no plugin code runs per frame.

3. **Plugins are installed through Add-ons.** A plugin is an add-on of kind `plugin` (ADR-0145, `add-ons.md`: `Kind` is "later: Plugin"). Download, checksum, unpack, status and remove are the add-ons code; there is no second store. The manifest's permissions are shown and approved at install. The add-on catalogue today is a curated list compiled into the app; third-party plugins need publisher identity, version pinning and the permission prompt, so step 3 extends the add-ons trust model under its own review.

4. **Every list a plugin could extend is a runtime registry.** Video and audio filter types (step 1), then source types with their node-type behaviour and transitions (step 2 and 5), then output kinds and automation triggers, are registries filled at start-up and kept for the session (a rebuild of a derived table, such as `NodeTypeTable`, reads the registries again, so plugin entries survive it). Built-in types register and are looked up the same way as plugin types; their trust and tier may differ, and the `org.rbs.*` id prefix is reserved for them. Code looks types up by id and must not `match` on a closed list of built-in types in the engine or the studio host to decide behaviour; behaviour comes from what the type declares (fields, pins, activity traits, capabilities).

5. **Saved documents name types by id and survive a missing type.** Scene files, looks and API payloads name a type by its reverse-DNS id string. A document that names a type nobody registered (a plugin was removed) must load, keep that item and its settings unchanged, show it as missing with one sentence (in the API: `kind: null`, as `SourceInfo` does today), and save it back unchanged. This is already true for sources and filters and step 1 pins it with round-trip tests. Transitions break this rule today (an unknown kind refuses the document); step 2 changes that loader.

6. **Settings and UI come from a schema.** A type declares its settings with `rbs-params` (unit, range, default, help, disclosure level). The Properties form, the control API and MCP are generated from it. Plugins never draw UI: a panel is a tree of design-system components the host renders with theme tokens (plugin SDK spec, "Panels"; ADR-0015, ADR-0057). So plugin UI cannot break the token rules.

7. **Zero cost before use.** Registering a plugin type reads only its manifest: the name, fields and pins the UI lists come from declarations, not from running code (today's `Manifest` has no fields or pins; adding them is a plugin SDK format change in step 3). No `wasmtime::Engine`, compiled module, shader module or pipeline exists, no process is started and no library is loaded until the first instance becomes Active (ADR-0013, plugin SDK rule 6). Per-call time budgets, bypass and restart follow P7 and plugin SDK rule 4.

8. **New features are built extension-ready (binding once this ADR is Accepted).** A new source, filter, transition, output, adapter or automation trigger must: (a) register through a registry from decision 4; while that registry does not exist yet, it may extend the existing closed lists (`SourceClass`, `rbs_api::SourceKind`, `TransitionKind`), but must not add a new closed list or a new `match` on built-in types outside them; (b) be stored and addressed by a type id; (c) declare its settings as a schema; (d) reach the rest of RBS only through this list: port traits and the adapter registry, the `rbs-params` schema and parameter channels, frames and buffers lent for a call (GPU textures included, as a native plugin would get them), and the host services the plugin SDK spec names; not another feature's internal state. The native tier is the upper bound, so zero-copy GPU work is allowed; a built-in that needs more (the browser supervisor, `YuvDownload`) names it in its ADR. A feature that cannot meet one of (a)-(d) says why in its ADR, and the reviewer checks it. A feature does not have to ship as a plugin. P16 and CLAUDE.md rule 19 carry this rule and are marked Proposed until this ADR is Accepted.

9. **Rollout. Each step is its own pull request; none starts before this ADR is Accepted.**
   1. Video and audio filter catalogues become registries (`FilterRegistry`, `AudioFilterRegistry`) pre-filled with today's built-ins; callers (`rbs-sources`, `rbs-studio-host`, `rbs-app`, `rbs-cli`, `rbs-import-obs`, tests and benches) take the registry. This is an internal API change with its own review: `rbs_params::Field` and `FieldKind` (`key`, `label`, `help`, `unit`, `options`) and `Pass::shader` hold `Cow<'static, str>` / `Cow<'static, [..]>` instead of `&'static`, so a schema read at runtime fits without leaking; `FilterType::name`, `fields` and `passes` return data borrowed from the type (`&str`, `&[Field]`, `&[Pass]`), with no refcount per call. `FilterNode::process` calls `passes()` and `uniforms` every frame (hot paths), so the step brings before/after numbers, the `hot-path-review` checklist and the owner's sign-off (P15) even though the call shape stays the same. The node keeps its `Arc<dyn FilterType>`. `video-filters.md`, `audio-filters.md` and the `rbs-params` docs are updated with it. Adds the missing-type round-trip tests of decision 5.
   2. Source types: a registry replacing the `SourceClass` matches, with `live_settings`, `live_keys` and `activity` moved into each type's record; the transition loader stops refusing unknown kinds (decision 5). `rbs_api::SourceKind` and `SceneTransition.kind` gain a type-id form, and the scene format stores unknown transition ids; both are contract changes with their own review (P14).
   3. Sandbox loader: the `rbs:plugin@1` world for video filters (WGSL) and audio filters, `wasmtime` in `rbs-plugin-host`, add-on kind `plugin`, the manifest's declared fields, pins and uniform mapping. The `check-layers` rule for `Class::Plugin` (today: only `rbs-plugin-sdk`) is widened to the exact crates the wrappers need (`rbs-core`, `rbs-graph`, `rbs-params`, `rbs-filters`, `rbs-audio-filters`), and a new rule lets only composition roots depend on `rbs-plugin-host`. The WIT world, SDK 1.0 and these layering changes get their own ADR (rule 11).
   4. Process tier: the browser runner's IPC and shared memory generalised into a process-plugin host. Own ADR.
   5. Transitions as a registry, automation triggers, then the native tier with signing. Own ADRs.

## Consequences

- Later plugin work is known and ordered; steps 1 and 2 are small and can land well before any loader.
- Each new feature carries a little more work (a registry entry, a schema, a type id), and its ADR has one more thing to state.
- The control API and scene format keep their current enums until step 2; adding the id form is a contract change with its own review (P14).
- `wasmtime` is a large dependency; it lives only in `rbs-plugin-host`, which only composition roots may use (enforced by `check-layers` from step 3).
- Step 1, which looked like a small refactor, edits a type called from `FilterNode::process`, so it needs measurements and the owner's hot-path sign-off.
- Follow-ups in this change: plugin SDK spec "Registries" and "Order" sections; principle P16 and CLAUDE.md rule 19 (marked Proposed); a dated line in ADR-0006's Consequences. Later: `add-ons.md` `Kind::Plugin` (step 3), `scene-model.md` missing-type rule (step 2), `video-filters.md` and `audio-filters.md` catalogue sections (step 1).

## Alternatives considered

- **Native tier first, as ADR-0006 allowed.** Fastest path to GPU filters, but it repeats OBS's crash and ABI problems before the safe tiers exist, and needs signing first.
- **Open the registries only when the loader is built.** Every feature until then adds closed lists; the later change touches more code and contracts.
- **Load OBS plugins.** Rejected in ADR-0006; still rejected.
- **Process tier first.** Reuses proven code (ADR-0109, ADR-0157) and needs no `wasmtime`, but every plugin pays a process, IPC and a frame copy or texture share, the opposite of zero cost for small filters; it stays second, for the plugins that need it.
- **Data-only plugins first (WGSL plus a declared schema, no WASM).** Covers most video filters at no runtime cost and is a good first step 3 deliverable, but covers no audio filters, automation or logic; it is kept as the first consumer of the sandbox tier, not as a tier of its own.
- **A scripting language first (Lua or JS) instead of WASM.** Lighter to start, but a second runtime with its own sandbox; WASM components already cover scripts compiled from many languages.

## Review

Reviewer: independent AI session (given only the diff, principles, specs, ADRs and code)
Date: 2026-11-21
Verdict: changes requested (round 1)
Concerns:
1. `NodeTypeTable` is not really open: behaviour matches `class_of`, and the table is rebuilt -> Context row corrected; decision 4 says rebuilds read the registries; step 2 moves `live_settings`, `live_keys`, `activity` into the type record.
2. Decision 5 conflicts with the transition loader -> scoped: true for sources and filters (pinned in step 1), transitions fixed in step 2; `kind: null` named as the API form.
3. Step 1 is a trait change and `uniforms` is on a hot path -> step 1 names the `&'static` changes, keeps `Arc<dyn FilterType>` and the `uniforms` call shape, else P15.
4. Plugin code on real-time threads not addressed -> decision 2a.
5. Manifest has no fields or pins; P13 wording -> decision 7 names the SDK format change (step 3) and lists the engine, module and pipeline.
6. Decision 8 (d) uncheckable, outputs and triggers lack registries -> (d) is a named list with the native tier as the upper bound; decision 4 adds output kinds and triggers.
7. Rules binding before acceptance -> P16 and rule 19 marked Proposed.
8. Partial supersession not recorded in ADR-0006 -> dated line added to its Consequences.
9. "No private path" untrue for built-ins -> reworded: same registration and lookup, trust and tier may differ.
10. Missing alternatives -> process tier first and data-only plugins added.
11. Add-ons trust model for third parties -> decision 3 says step 3 extends it under its own review.

Reviewer: independent AI session (fresh, round 2)
Date: 2026-11-21
Verdict: changes requested (round 2); all round-1 resolutions confirmed in the text and against the code
Concerns:
1. `rbs_params::Field`, `FieldKind` and `Pass::shader` are `&'static` too -> step 1 names the `Cow` change, the seven crates and the spec updates.
2. `passes()` is called in `FilterNode::process` every frame -> borrowed return, no refcount; step 1 brings P15 numbers and sign-off unconditionally.
3. `check-layers` limits `rbs-plugin-host` to `rbs-plugin-sdk`; nothing enforces "only composition roots" -> step 3 widens the rule to named crates and adds the composition-root rule, in its own ADR.
4. Rule 19 (a) cannot be met by a new source before step 2 -> extending the existing closed lists is allowed until their registry lands; only new closed lists are forbidden.
5. Decision 2a blocks a sandbox filter's `uniforms` -> first sandbox filters are data-only with a manifest uniform mapping evaluated by the host.
6. nit: date of decision 8 vs P16 -> "binding once Accepted" everywhere.
7. nit: node types listed with step 1 -> decision 4 order aligned with the steps.
8. nit: transition id form is a scene format and API change -> step 2 says so and gives it its own review.

Round 3: pending. Then owner approval.
