# Spec: MCP server for AI agents

- Status: Draft
- Owner crate(s): `rbs-mcp` (planned)
- Related ADRs: [0044](../adr/0044-control-api-protocol-and-security.md), [0046](../adr/0046-network-access-over-tls.md), [0051](../adr/0051-user-data-profiles-and-scene-collections.md), [0052](../adr/0052-ai-agent-control-through-mcp.md)

## Purpose

Let AI agents (any Model Context Protocol client) operate RBS: read state, edit scenes, sources, profiles and scene collections, and start or stop recordings and streams, with exactly the rights of the agent's own control API token.

## Scope

In scope: the `rbs-mcp` program, its tools, prompts and resources, how it authenticates and what it never exposes.

Out of scope: the control API itself ([control API spec](control-api.md)), agent applications, and MCP over HTTP and RBS on another computer (each needs its own review, ADR-0052 decision 2).

## Contract

### Process and connection

- `rbs-mcp` is started by the agent application and speaks MCP over stdio. It connects to the RBS of the current user over local IPC (`rbs-api-ipc` client) and ends when stdin closes.
- It sends `session.hello`, checks the protocol version it needs, then `session.authenticate {token}`. The token comes from the `RBS_TOKEN` environment variable when set (agent configuration files keep it in plain text, so the documented way is the file), otherwise from `--token-file <path>` (default: `agent-token` in the config directory; on Unix a file other users can read is refused; on Windows the per-user configuration folder is user-only already, as for `remote-control.toml`); there is no command-line option for the token itself. Without a token, or on `auth_failed`, it exits non-zero with a message on stderr that never contains the token.
- The session has exactly the token's scopes and permissions, also on IPC (control API spec, "Handshake and authentication", planned addition). `rbs-mcp` fails closed: it sends nothing after `session.hello` until the `session.authenticate` answer arrives; the server's protocol minor must support narrowing and that call must succeed (an older server's "already authenticated" refusal is a failure); tools are built only from the scopes and `irreversible` in that answer. At start, a failed handshake or authentication with a reachable RBS ends `rbs-mcp` with a non-zero exit; an RBS that cannot be reached does not (below).
- If RBS cannot be reached at start, only `studio_summary` is listed, and it answers with how to turn on remote control. `rbs-mcp` declares the `tools.listChanged` capability and tries again on each tool call. A later connection runs the same handshake: if it fails, tool calls return tool errors and no RBS tool is listed; if it succeeds, and after RBS restarts with another instance id, it reads `api.describe` again and sends `notifications/tools/list_changed`.

### Tools

- **Generated tools.** One per method that `api.describe` lists and the session's scopes allow, except `session.*`, `tokens.*`, `events.*`, plugin methods, `edit.undo` and `edit.redo` (the undo stack is shared with the user). Only methods built into `rbs-api` are listed; their descriptions and schemas contain no text from state or plugins. Name: the method with `.` replaced by `_` (`source.add` becomes `source_add`). Description: the method's doc comment from the schema. Input schema: the method's parameters from the generated JSON Schema. Result: the method's answer as JSON (`structuredContent`), and the same JSON as text.
- **Annotations** from the method's `effect`: `read` sets `readOnlyHint: true`; `edit` sets `readOnlyHint: false, destructiveHint: false`; `live` and `destructive` set `destructiveHint: true`; `stream.start` and methods that add or re-address a source of a network type also set `openWorldHint: true`. A missing or unknown effect is handled as `destructive`. Tools whose effect the token's permissions do not allow (ADR-0052 decision 4) are not listed.
- **Options.** `--tools <namespace,...>` lists only those namespaces (default: all allowed).
- **Task tools** (fixed, written by hand, each built only from API requests the session could send itself):
  - `studio_summary`: canvases, scenes with their items, sources, current profile and collection, recording and stream state, in one compact JSON value.
  - `scene_build {canvas, scene, sources: [{type, name, settings, layout}]}`: creates or fills a scene through one `edit.apply`, so it is one undo step named after the scene.
  Each task tool names the protocol minor it needs and is not listed for an older server. More task tools are added only with a spec change.
- **Errors.** An API error becomes a tool error (`isError: true`) with its kind (`scope_denied`, `not_permitted`, `conflict`, `invalid_params`, ...) and message, so the agent can correct itself. After a successful start, protocol failures (lost connection, a failed reconnection handshake) are tool errors, never a crash of `rbs-mcp`.

### Prompts and resources

- Server `instructions`: a short text (what RBS is, read `studio_summary` first, edits are undoable and live actions are not).
- Prompts: one per documented workflow (for example "set up a vertical canvas", "prepare a recording"), from Markdown files compiled into `rbs-mcp`.
- Resources: `rbs://guide/<topic>` (the same Markdown files) and `rbs://schema` (the connected RBS's JSON Schema).
- Prompt, resource and instruction text comes only from the build. Text from RBS state is never placed in it.

### Never

- Never logs, returns or stores the token, stream keys or the stream server address (the API does not return the last two at all).
- Never offers `tokens.*` or plugin methods, whatever the token's scopes.
- Never reaches RBS other than through the control API.

## Errors and failure behaviour

Each tool call is independent: a failed call leaves the session usable. An edit refused by the API changed nothing (`edit.apply` is all or none).

## Performance requirements

None inside RBS: `rbs-mcp` is a separate process, and RBS serves it as any API client, under the same rate limits. `rbs-mcp` reads `api.describe` once per connection.

## Conformance tests

- Against a fake `ControlTarget` over real IPC: tools listed match the scopes and permissions of the token (View only lists only `read` tools); a `control` token never sees `admin` tools; a token without the irreversible-actions permission sees no `live` or `destructive` tools; `tokens.*`, `edit.undo` and `edit.redo` are never listed, also with a token that holds `tokens`.
- Against a server one protocol minor older (no narrowing), `rbs-mcp` exits non-zero and lists no tool.
- Server side, bypassing `rbs-mcp`, against the real `rbs-app` target: a View only token gets `scope_denied` for a `control` method; a token without the permission gets `not_permitted` for `record.start`, for an edit of a source in a nested scene of the program while recording, and for `edit.undo` of an on-air step, and each refusal is audited. The on-air function itself is tested directly in `rbs-scene`.
- `profile.import` of a file with a stream server leaves the imported profile without one; `profile.export` writes no server; an export to an existing path gets `file_exists` and leaves the file unchanged.
- `session.authenticate` after another request on a same-user IPC session is refused; a request pipelined by `rbs-mcp` before the answer does not exist (the client test checks it sends nothing).
- Without the permission: `canvas.set` and `settings.set {canvas}` on a recording canvas, a volume change of a source in its mix, adding a network source, an image source with a UNC path or an `http://` URL, a UNC recording folder, a plugin or unknown source type with opaque settings, and `tokens.create` asking for the permission get `not_permitted`; an `edit.apply` with one such operation is refused as a whole and changes nothing.
- `profile.import` of a file that is not a profile returns kind, line and column, and no text of the file.
- A tool name for every built-in method matches MCP's name pattern, and no two methods map to the same name.
- A missing or unknown effect gives `destructiveHint: true`.
- An edit made through `rbs-mcp` is undone by the app's own undo.
- Without a token, `rbs-mcp` exits non-zero; the token never appears in stdout, stderr or any tool result (searched for in all output of the test run).
- Names, input schemas and annotations of generated tools match a snapshot of the schema.
- `scene_build` makes exactly one undo step; a refused operation in its list leaves the collection unchanged.
- A source name and a window title containing instructions are returned as values and appear in no tool description, input schema, prompt or instruction (`tools/list` and `prompts/list` searched).

## Open questions

- Which MCP library (the official Rust SDK is the first candidate), decided with its dependency justification.
- MCP over HTTP and remote RBS (WebSocket with a pinned key): own reviews.
- Mapping event subscriptions to MCP resource updates.
