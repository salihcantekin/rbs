# ADR-0111: Vertical outputs start and stop with the main ones, and each output shows its own state

- Status: Accepted
- Date: 2026-11-13
- Deciders: Salih

## Context

ADR-0093 decision 6 gives each canvas its own recording and its own stream. Protocol 1.29 exposes them through an optional `canvas` on `record.*` and `stream.*` ([control API](../specs/control-api.md#outputs-of-the-vertical-canvas-protocol-129)). In the desktop app, the vertical outputs live behind the ▾ of Record and Go live, as `record.vertical.toggle` and `stream.vertical.toggle`.

The owner tried this on Windows on 2026-11-13 and found it unclear:

- Choosing Record vertical in the ▾ menu starts a recording right away. Nothing tells the user whether Record must also be pressed.
- Record then starts and stops only the main recording. The vertical one keeps running, and stopping it means finding the same ▾ entry again.
- Nothing on the button shows that a vertical recording is running.
- While a vertical canvas exists, the menu entry still reads "Add vertical output...". Its Remove button only appears inside the dialog.

The same paths must work from a control surface such as a Stream Deck (rule U20, ADR-0050). There, one key must be able to start or stop each output on its own, and one key must be able to start or stop both. A key's light must show each output's state.

## Decision

1. **Where the choice lives.** The output profile gains a section of its own, `[outputs-vertical]`, with `record_with_main` (default `true`) and `stream_with_main` (default `true`). The section carries its own `format_version = 1`, so a newer section is refused as `Newer`. An absent section or key reads as the default. A section that does not load reads as the defaults and is not overwritten until the user changes a flag. The flags stay out of `recording-vertical` and `stream-vertical`, which deny unknown fields in older builds. Writing them must not trigger the copy-on-first-change of those sections (UI design system spec). Older builds keep unknown sections on save (user-data spec). The flags live in the output profile, not in the scene collection, because output settings live there (ADR-0102 decision 1).
2. **What "with main" does.** While a vertical canvas exists and a flag is true, Record or Go live starts the main output and then the vertical output of that kind, and stops both. While the flag is false, Record and Go live act on the main canvas only, as they do today.
   - A **vertical stream that is not configured** (no vertical server saved) is skipped, with outcome `not_configured`. This is not an error and shows no notice, so the default does not make Go live fail for people who never set up a vertical stream.
   - **Auto record** stays per canvas (1.29). A recording that the main stream starts records the main canvas only, whatever `record_with_main` says.
3. **`canvas: "all"` (protocol 1.42).**
   - **Methods.** `record.start`, `record.stop`, `stream.start` and `stream.stop` accept `canvas: "all"`. The value is its own parameter type for these four methods, so `record.start_to_file`, `record.status` and `stream.status` reject it with `invalid_params`. The effect is `live`, as for the other values.
   - **What it acts on.** `"all"` means the main canvas, then the vertical canvas when one exists. The control thread runs it as one step, with no other request handled in between.
   - **Errors, in this order:**
     1. `invalid_params`.
     2. `conflict` while an output profile or collection switch is pending (for the whole call).
     3. `not_found` for an unknown `profile`.
     4. The main canvas's refusal, including a main output that is still ending (`conflict`), fails the whole call. Nothing else starts, and the error answer carries no `outputs`.
     5. The vertical canvas's refusal appears only in its entry.

     "Refusal" means a refusal at start. A later connection failure or reconnect give-up stays per canvas in `*.canvas_state`.
   - **Result.** `{ "outputs": [ { "canvas": "main" | "vertical", "outcome": "started" | "running" | "stopped" | "not_running" | "not_configured" | "refused", "video_encoder"?, "audio_encoder"?, "error"?: { "code", "message", "data"? } } ] }`. There is one entry per canvas, main first. The encoder fields appear on `started`, as in the single-canvas answer, and `error` appears on `refused`.
   - **Transitional states.** For start, an output that is starting or running is `running`. A vertical output that is still ending is `refused` with `conflict` in its entry. A start is `conflict` when no entry is `started`, matching a single-canvas start of a running output. For stop, an output that is starting or running is stopped (`stopped`). An output that is ending or idle is `not_running`. A stop is `conflict` only when no entry is `stopped`.
   - **Versioning.** This is a minor version. The control API spec's Versioning section is amended in the same change: a new value of a request field, which older servers refuse with `invalid_params`, is minor. The answer schema of these four methods becomes a `oneOf` of the single-canvas answer and the `outputs` answer, selected by the request, and the schema check classifies adding a `oneOf` branch as minor. 1.29 to 1.41 servers refuse `"all"` with `invalid_params`. A 1.28 server ignores `canvas` and acts on the main canvas, so a client must check for 1.42 before sending `"all"`.
4. **The setting through the API.**
   - `output.set_with_vertical { "kind": "record" | "stream", "on": bool }` has scope `control`, so a control-surface token can flip the choice. It is not undoable and is allowed while outputs run, because it acts at the next start or stop.
   - `settings.get` and `settings.changed` show `vertical: { "record_with_main", "stream_with_main" }`, present even with no vertical canvas. `vertical` is an object; the vertical recording and stream settings join it in a later minor version.
   - `settings.set` does not write these flags in 1.42.
   - A client of a server older than 1.42, which has no `vertical`, must assume main only.
5. **The registry (U20).**
   - **Fixed parameters.** A registry `Entry` gains `fixed: &[(&str, &str)]`, string parameters merged into every method call the entry runs. The id's own parameters keep mapping one to one. `record.main.toggle` and `stream.main.toggle` ("Record main only", "Go live main only") carry `canvas = "main"`. The existing vertical toggles move their `canvas = "vertical"` from `menu_bar.rs` into `fixed`.
   - **Resolved toggle.** `record.toggle` and `stream.toggle` become `Kind::ResolvedToggle { on, off }`. `rbs-ui` names the methods only. The parameters come from a pure resolver in `rbs-app`, `outputs::resolve_toggle(id, flags, has_vertical) -> Params`, so `rbs-ui` has no knowledge of profiles. It returns `canvas: "all"` while the flag is true and a vertical canvas exists, and `canvas: "main"` otherwise. The toggle's on/off state (including `checked` in `ui.command.list` and a key light) is the main canvas's state. Its off action is the stop with the same resolved `canvas`.
   - **The choice** is a `Picker` per kind, `output.with_vertical?kind=record|stream`, with the two choices `on` and `off` ("Main + vertical", "Main only"). It runs `output.set_with_vertical`, and the menus draw it as a radio pair.
   - **Menu entries.** While a vertical canvas exists, `canvas.add_vertical` is titled "Vertical output settings...". A new entry `canvas.remove_vertical` ("Remove vertical output") runs the method of the same name. `ui.command.list` reports the current title.
   - **The U20 test** calls the resolver and `fixed`, and checks the method and parameters for both flag values and with and without a vertical canvas.
6. **The UI shows each running output.** While a vertical canvas exists:
   - The ▾ menus begin with the radio pair, followed by the single-canvas entries. A single-canvas entry reads "Stop …" with its elapsed time while its output runs.
   - Beside Record and Go live, each running vertical output shows a compact chip with the vertical mark, its state (recording, live or connecting), the elapsed time, and a stop button for that output alone. The chip exists only while that output runs (P13).
   - When the user stops the main output and the vertical output of that kind keeps running, a notice says so with a Stop action. It shows once per such stop, and it is not a modal.
   - Ending both streams uses the existing End stream confirmation once, naming both.
   - When main starts and the vertical output is `refused`, the existing error notice names the vertical output and the reason.
   - While no vertical server is saved, the Go live radio pair shows the hint "Vertical stream not set up" with a link to Settings, Stream.
   - UI text says "vertical", not an aspect ratio, because a custom vertical canvas need not be 9:16. "Vertical output" goes into the glossary. Visual details follow rules 15 and 16.
7. **Load checks.** When ADR-0087 (estimate before start) and ADR-0090 (disk guard) are built, a start with `"all"` checks the load of every output it would start, together.

## Consequences

- One press records or streams both shapes, which is what a user who adds a vertical output usually wants. A single-canvas choice is still one click away, and every action is one key on a control surface.
- `canvas: "all"` gives the API a partial-success result. A client must read each entry, not only the call's success.
- The registry gains a kind whose parameters depend on a profile setting. This is new: until now, a command's parameters were static.
- The default changes behaviour for people who already use a vertical canvas: after this ships, Record starts both. That user group is small, since the feature shipped on 2026-11-10.
- Work: control API spec (1.42 section, compatibility note, `settings` field, registry rows), recording spec (`[outputs-vertical]`), status and implementation-status rows, glossary, UI design system (menus, chip, notice), `rbs-api` method params, `rbs-app` host (`canvas: "all"`), registry entries, tests for partial failure and for the toggles' method mapping (command_registry), tokens for the chip if none fit (rule 15).
- Not covered here: a vertical picture on Windows (ADR-0093 decision 7), manual framing on the vertical canvas, and subject tracking. Each needs its own change.

## Alternatives considered

- **Only make the ▾ entries clearer (labels, a chip), with Record staying main-only.** The owner's main complaint, that stop does not stop everything, stays.
- **A modal "Record both?" prompt on every press.** This adds a click to the most frequent action, and it does not work from a control surface.
- **No `"all"`, with the client sending two calls.** The UI command would then run two methods, and a client would have to rebuild the main-first, partial-failure rule itself. U20 wants the command and the API to match one to one.
- **Store with-main in the scene collection.** Output settings already live in the output profile (ADR-0102). Switching collections should not change what Record does.

## Review

- 2026-11-13, independent AI session (round 1, given only this ADR, the specs, the principles and the code): **changes requested**. Ten concerns were raised. Each was resolved in the decisions above:
  1. Storing the flags in `recording-vertical` and `stream-vertical` clashed with `deny_unknown_fields` and with the copy-on-first-change rule. They now live in a separate `[outputs-vertical]` section (decision 1).
  2. The `"all"` result shape was not defined. The exact JSON, its own parameter type and the versioning class are now in decision 3.
  3. The order of errors and what counts as a failure were not spelled out. Both are now in decision 3, which also says `"all"` runs as one step.
  4. Transitional states and the toggle's on/off state were not defined. The starting and ending outcomes are now in decision 3; the toggle's state follows the main canvas (decision 5).
  5. With the defaults, Go live would fail for anyone without a vertical server saved. An unconfigured vertical stream is now skipped as `not_configured` (decision 2).
  6. The `settings` field needed room for future vertical settings. It is now an object that is always present, and it can be changed while outputs run (decision 4).
  7. The registry could not express parameters that depend on a setting. A new registry kind, radio entries, a dynamic title and the U20 test are in decision 5.
  8. Auto record with the main stream records the main canvas only (decision 2).
  9. UI text says "vertical", not "9:16", and the glossary is updated. The notice shows once per stop (decision 6).
  10. Missing spec follow-ups and load checks are now in decision 7 and Consequences.
- 2026-11-13, independent AI session (round 2, same inputs): **approve with changes**. All eight items were resolved in the text:
  1. The registry could not carry fixed or resolved parameters. Decision 5 now adds `fixed`, `Kind::ResolvedToggle` with its resolver in `rbs-app`, and a `Picker` for the choice.
  2. Setting the flag through `settings.set` needed `admin`. The flag is now set by `output.set_with_vertical` with scope `control` (decision 4).
  3. Start had no `conflict` rule, and a main output that is still ending contradicted the error order. Both are fixed in decision 3.
  4. The Versioning section did not cover a new request value. It is amended, with the `oneOf` answer schema (decision 3).
  5. The `settings` semantics were unclear. `settings.set` no longer writes the flags, and the rule for older servers is stated (decision 4).
  6. The section's `format_version` and load failure were not covered. Both are now in decision 1.
  7. Partial failure and an unconfigured vertical stream had no UI. Decision 6 now has the error notice and the "Vertical stream not set up" hint.
  8. The registry kind's name was too narrow. It is now `ResolvedToggle`.
- 2026-11-13: Owner (Salih) accepts this ADR, including the behaviour change: while a vertical canvas exists, Record and Go live start and stop both outputs by default.
