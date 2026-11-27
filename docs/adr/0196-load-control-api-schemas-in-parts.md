# ADR-0196: Load control API schemas in parts

- Status: Proposed
- Date: 2026-11-27
- Deciders: Salih (owner)
- Refines: [ADR-0044](0044-control-api-protocol-and-security.md) (generated schema, served by `api.describe`), [ADR-0052](0052-ai-agent-control-through-mcp.md) (generated MCP tools), [ADR-0134](0134-ai-tools-settings-page.md)

## Context

Protocol 1.108 made `api.describe` carry an input and an output JSON Schema for every built-in method (192 methods, 591 KB minified, 956 KB as checked in). Two things follow:

- `api.describe` answers with all of it on every call, and the server parses the whole snapshot (`include_str!` plus `serde_json::from_str`) per call.
- `rbs-mcp` turns each method into one MCP tool, so `tools/list` hands an AI agent every schema at once. That is the part that burns the agent's context (roughly 150k tokens) before it does anything, and most sessions touch a handful of methods.

The owner asked (2026-11-27) for a structure where an MCP client loads only the parts it needs. The schemas stay generated from the Rust types and checked in, because ADR-0044 allows `schemars` only as an opt-in feature of `rbs-api`, so no normal build may depend on it.

## Decision

1. **`api.describe` takes optional parameters** `{ detail?, namespaces?, methods? }`. `detail` is `"index"` (the default) or `"schemas"`. `namespaces` and `methods` select what the answer covers (default: every method); an unknown name is `invalid_params`. `index` lists name, scope and effect only. `schemas` adds `input_schema` and `output_schema` for the selected methods. A client that sends no parameters now gets the index; since 1.108 the schema fields are optional, so a client already treats their absence as "unavailable". The topic list stays in every answer. Protocol 1.110.
2. **The snapshot is sharded by namespace**: `crates/rbs-api/schemas/<namespace>.json`, each file a map `method -> { input_schema, output_schema }`, pretty-printed with sorted keys. Each shard is embedded with `include_str!` and parsed on the first request that needs it, then kept (`OnceLock` per shard). Nothing is parsed at start-up, by an `index` answer, or when no client asks: the cost of an unused feature stays at one embedded string (P13). The `schema` feature's generator writes the shards, the existing drift test compares them with a fresh derivation, and `xtask verify` runs that test. The monolithic `method_schemas.json` is removed.
3. **`rbs-mcp` lists a small fixed set of tools by default** instead of one tool per method:
   - `studio_summary` (unchanged).
   - `api_index { namespace? }`: namespaces with method counts, or one namespace's methods with description (from the spec compiled into `rbs-mcp`), effect, and the call tool to use. It sends no schemas.
   - `api_schema { methods: [name] }`: input and output schema of the named methods, fetched with `api.describe { detail: "schemas", methods }` and cached for the connection.
   - `api_read { method, params }`, `api_edit { method, params }`, `api_live { method, params }`: call a method of effect `read`, of effect `edit`, or of effect `live` and `destructive`. Three tools keep MCP annotations true (`readOnlyHint`, `destructiveHint`); a method called through the wrong one is a tool error that names the right one.
   Every rule of the mcp-server spec applies to these calls exactly as it did to generated tools: never-offered namespaces and methods, token scopes, the irreversible-actions permission, missing effect handled as `destructive`, and the API's own checks. The three call tools are not listed in the hint state.
4. **`--tools <namespace,...>` keeps generating one tool per method** for those namespaces (`--tools all` for every allowed namespace, the old default). Their schemas come from one `api.describe { detail: "schemas", namespaces }` call; other namespaces stay reachable through the tools of decision 3.
5. **Schemas are never trusted more than before**: the existing validation (meta-schema, 64 KiB, 64 levels, local references only) runs on every schema `rbs-mcp` receives, whichever way it arrives; an invalid one refuses that request, not the connection.
6. **Plugin-ready (ADR-0006, P16).** The index and shards describe built-in methods only. A plugin's methods, when they exist, publish their own schema through the same `methods` selection, so the loader needs no list of namespaces in the client.

## Consequences

- `tools/list` is a few hundred tokens, not the whole catalog; schemas cost tokens only for the methods an agent asks about.
- `api.describe` on a connection that never asks for schemas costs a method table, no JSON parsing.
- A generated-code client (TypeScript, planned) fetches schemas per namespace.
- Breaking for `rbs-mcp` users who relied on one tool per method by default: they pass `--tools all`. Documented in the spec and the AI tools settings text is unchanged.
- Specs: `control-api.md` (`api.describe`, 1.110), `mcp-server.md` (Tools, Options). Tests: shard parse is lazy (a counter), index answers carry no schema, selection and unknown names, every method lies in exactly one shard, tool list stays under a fixed size, effect/tool mismatch, never-offered methods are refused by `api_*` too, drift test per shard.
- Follow-up, not done here: hoisting `$defs` shared by many methods (146 KB unique of 591 KB) into one shared document would shrink the snapshot further.

## Alternatives considered

- **Generate the schemas at build time** (`build.rs` with `schemars`): removes the checked-in files but makes every build compile `schemars`, against ADR-0044. Not chosen.
- **Keep one tool per method but shorten schemas**: still hundreds of tools in every `tools/list`.
- **MCP `tools/list_changed` with on-demand tool loading** (a "load namespace" tool that adds tools): depends on every client honouring list changes, which many do not. The `--tools` option keeps a static form of it.
- **One generic `api_call` tool**: its annotations cannot say "read-only" or "destructive" truthfully, so clients cannot gate it by effect.
- **Keep the default of `api.describe` as it is** (full) and add `detail: "index"` as an opt-in: leaves every old caller paying for the full answer and keeps the whole parse on the default path.

## Review

Pending independent review (ADR-0014).
