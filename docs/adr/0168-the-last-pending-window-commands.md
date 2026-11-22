# ADR-0168: The last fifteen Pending window commands

- Status: Proposed
- Date: 2026-11-22
- Deciders: Salih Cantekin (owner)

## Context

[ADR-0163](0163-one-command-path-for-the-window-and-the-api.md) sends every window `studio_host::Command` that has a control API method as `Command::Api`. [ADR-0164](0164-protocol-minor-for-the-window-commands-left.md) (protocol 1.100) moved what it could. 15 rows are still `Pending` in `rbs-studio-host` `studio_host/command_path.rs`; their reasons are in [the inventory](../research/command-path-inventory-2026-11.md).

**The window waits for an update the method's answer does not give.**

| Variant | The window waits for | The method today |
| --- | --- | --- |
| `SetCanvas` | `Update::Canvas`, also for an unchanged canvas or a refusal, because it closed its preview first | `settings.set` `canvas` sends no such update |
| `StopVirtualCamera` | `VirtualCamera(None)`, also when nothing sends | `vcam.stop` answers `conflict` when nothing sends |
| `SaveReplay` | `ReplaySaved` once the file is written | `replay.save` answers once the clip is requested |
| `SetSourceBox` | `BoxPlaced {seq}` | `item.set_box` answers `{seq}`. The Main canvas already runs it (`items.rs`); only the Vertical branch differs |
| `ListAddOns` | `Update::AddOns` | `addon.list` returns the same `AddOnList` |
| `StartSyncMeasure` | the Measure view's phase. The host keeps that phase on `measure.window` (`sync_measure.rs`) | `sync.measure_start` |
| `CloseSyncMeasure` | nothing. It drops the host's view and cancels any measurement that runs for the view's source | `sync.measure_cancel {measurement}` |

**The window path does more than the method.**

- **`SetFilterValues`.** A release with unchanged values puts the stored values back on the running filter (`Ok(false)` in `source_settings.rs`). `filter.set_settings` does not.
- **`BrowserControl`.** The host picks the message from `studio.browser_control`. A reload of a page that does not run becomes `source.reload`, and a script with no page running is quiet. `browser.reload` and `browser.run_script` refuse both cases.
- **`ForgetBrowserSignIn`.** It gives a running page the new profile first. `browser.forget_sign_in` is `busy` while the page runs.
- **`InteractInput`.** The window sends one `rbs_browser_ipc` event per pointer move or key, only while the page is open in Interact, and the host drops input to a page that is not there (`interact.rs`).
  - `browser.input` takes a list of the same kinds (move, leave, button, wheel, key, text; `request.rs`).
  - It refuses only a page that does not run.
  - Its `modifiers` carry shift, ctrl, alt and meta only. The window's moves also carry the held buttons (`LEFT_HELD` and the others in `rbs-browser-ipc` `input.rs`), which a drag inside a page needs, and a `leave` has no modifiers at all.
- **Add source.**
  - `AddSource`, `AddSourceWithAudio` and `AddGlobalSource` first ask the OS permission when the kind needs it, then replay the command (`prompt_first` / `ask_permission`, `sources.rs`).
  - A picker entry (`Target.picker`, only through `AddSource`) runs the OS picker on a thread. That thread posts `AddSource` with the chosen target, which no list holds (`pickers.rs`).
  - `source.add` looks a target up in `targets(kind)`, keeps the OS name unless `name` is given, and answers `not_found` for an unlisted target (`api_studio.rs`).
- **`ListTargets`.** It answers the host's `Target` with `app`, `recent`, `picker`, `mic` and `sound`. `source.targets` maps the same list, picker entries included (with an empty `target`), to `{name, adapter, target, is_default, size}`.

ADR-0164 step 6 set the pattern: a call the window sends runs with `ApiTarget.window` set, and only then the host also sends the update the window waits for (`failures.rs` `start_refused`, `api_answer_on_panic`, `api_vcam.rs`).

## Decision

1. **Final path of each row.** After this ADR, each of the 15 variants takes one path:
   - It is deleted, and the window sends `Command::Api`: `SetCanvas`, `StopVirtualCamera`, `SaveReplay`, `SetSourceBox`, `ListAddOns`, `StartSyncMeasure`, `SetFilterValues`, `ForgetBrowserSignIn`, `InteractInput`, `ListTargets`, `AddSource`, `AddSourceWithAudio`, `AddGlobalSource`.
   - `BrowserControl` stays and becomes `Translates` (`browser.reload`, `source.reload`, `browser.run_script`). The host keeps picking the call from the page's state, as today, and runs it through `ApiTarget`. A script for a page that does not run is quiet for the window.
   - `CloseSyncMeasure` stays and becomes `Translates` (`sync.measure_cancel`). The host builds the cancel from `measure.run` when its source is the view's source, then drops the view. This keeps today's behaviour: a measurement of that source is cancelled, whoever started it, also before the start's answer reached the window.
   - New `PickSource { kind, adapter }`, `LocalOnly`: it runs the OS picker for a picker entry. An OS dialog needs the person at this computer, which is the reason ADR-0163 decision 6 gives OS permission prompts.
     - It first runs `prompt_first` / `ask_permission` and replays itself, as `AddSource` does today.
     - The picker thread posts a new internal `Msg` with the chosen target (not a `Command`). The host adds that target on the window's own path (`add`), as today. It never goes through `source.add`, which would refuse a target it does not list.
2. **How the window gets its answer.** Two patterns, picked per row:
   - **A ticket** when the method's answer carries what the window needs.
     - `SetSourceBox` (`item.set_box`, `{seq}` replaces `BoxPlaced`).
     - `ListAddOns` (`addon.list`).
     - `ListTargets` (`source.targets`).
   - **A window-flag update** when the window waits for a state update that already exists. Only with `ApiTarget.window` set, the host must also send that update on success, on refusal and after a caught panic.
     - `SetCanvas`: `Update::Canvas`.
     - `StopVirtualCamera`: `VirtualCamera`; the call is quiet on `conflict`.
     - `SaveReplay`: `ReplaySaved`, when the worker ends or with the refusal's text.
     - `StartSyncMeasure` (`sync.measure_start`, `Quiet`): the host still sets `measure.window`'s reference and phase, a refusal included, as today. The phase stays host-owned, and the window needs no id.
     - `SetFilterValues`: on `Ok(false)` the host puts back the stored values.
     - `InteractInput`: `browser.input` with one event, `Quiet`, using decision 3's `held` field. The host drops input to a page not open in Interact.
     - Add source: the OS permission check runs in `Host::command`, before `ApiTarget::run`, for a window `Command::Api` whose call is `source.add`. When the kind needs a prompt, the host asks and replays that `Command::Api` with its reply, as `ask_permission` does today (`permission_asked` stops a second prompt). A client's `source.add` never prompts; a test checks that.
   - `api_answer_on_panic` must map each window-flag row to its update. Each row gets a test that a client receives no extra update.
3. **Protocol 1.101.**
   - `source.targets` rows gain `app`, `recent` (the rank in `settings.recent_targets`), `mic`, `sound` and `picker`. Each is omitted when absent or false. A 1.100 client already sees picker rows and ignores the new fields; a 1.101 client skips `picker: true` rows, which it cannot use. Nothing is removed.
     - A `control` token learns which app owns a window and the user's recent picks. Window names are already listed, and the picks already are too (`settings.recent_targets`, `read`), so no new kind of data leaves.
   - `browser.input` events gain `held`: the mouse buttons held during a `move` or `leave` (`["left", "middle", "right"]`, omitted when none). `leave` gains optional `modifiers`. A 1.100 server refuses both (`deny_unknown_fields`).
   - `browser.forget_sign_in` gains `restart` (bool, default `false`). With `true`, a running page restarts with the new profile, as the window does, instead of `busy`.
     - It stays `control` and `destructive`, so a token without the irreversible-actions permission is refused as today (`data.reason: "irreversible"`).
     - The window sends `true`.
     - A 1.100 server answers `invalid_params` (`deny_unknown_fields`).
4. **Ratchet.** After the moves:

   | Count | Before | After | Why |
   | --- | --- | --- | --- |
   | `PENDING` | 15 | 0 | |
   | `TRANSLATES` | 22 | 24 | `BrowserControl`, `CloseSyncMeasure` |
   | `VIEW` | 13 | 13 | |
   | `LOCAL_ONLY` | 10 | 11 | `PickSource` |

   This ADR's approval is the written approval for those rises (ADR-0163 decision 5). Every other count may only fall.
5. **Steps, one commit each, with tests, inventory rows and spec text:**
   1. The window-flag rows except Add source and `InteractInput`: `SetCanvas`, `StopVirtualCamera`, `SaveReplay`, `StartSyncMeasure`, `CloseSyncMeasure`, `SetFilterValues`, `BrowserControl`.
   2. The ticket rows: `SetSourceBox`, `ListAddOns`.
   3. Protocol 1.101.
   4. After the 1.101 fields exist: `ListTargets`, Add source, `PickSource` and `InteractInput`. `InteractInput`'s commit carries a measurement note: control-thread time per event at a 120 Hz pointer, before and after.

   No step may change what the window shows. One differs on purpose: `SetCanvas` with an unchanged canvas while recording is no longer refused, because nothing changes. A step that would change anything else on screen stops for the owner's approval (CLAUDE.md rule 16).

## Consequences

- No `Pending` row is left. Every window action with a method runs one handler, so a fix to it reaches clients too.
- The window flag grows, one branch per row, next to the refusal it answers. The per-row tests keep clients' answers and events unchanged.
- `restart: true` lets a token with the irreversible-actions permission restart a page and delete its kept profile. That is the same effect the window's Forget sign-in has.
- Follow-up:
  - the control-api spec's "Protocol 1.101" section
  - the browser and sync lines in that spec
  - the conformance JSON cases
  - the `docs/status.md` U20 row
- Device check after its step: Interact, the OS picker (Linux portal), the permission prompt on Add source, and the Measure window.

## Alternatives considered

- **Leave the rows `Pending`.** That keeps two handlers per action, the drift ADR-0163 exists to end.
- **Keep `InteractInput` window-only.** `browser.input` already takes the same event kinds, and only the held buttons are missing. One additive field closes that gap. The window flag can keep the Interact check.
- **Drop picker rows from `source.targets`.** 1.100 clients see them today, so dropping them would be a removal. A `picker` flag is additive.
- **The window picks `BrowserControl`'s call from its row state.** That races with the page's real state, and a refused script would show a new error bar.

## Review

### Round 1: independent AI session, 2026-11-22, changes requested

1. **`browser.input` already has key and text events.** Context and decision 2 now say so. `InteractInput` moves, and the Interact check stays on the window flag.
2. **`source.add` already looks the target up and fills the name.** The name-fill sub-decision and its Consequence were removed.
3. **A picked target is not listed, so `source.add` would refuse it.** `PickSource` now adds it on the window's own path (decision 1).
4. **The permission prompt before Add source was missed.** The window flag now asks first and replays the call (decision 2).
5. **Picker rows are already in `source.targets`.** They stay, with an additive `picker` flag (decision 3).
6. **The ratchet did not add up.** Decision 1 now lists every row's final path, and decision 4 recounts.
7. **`BrowserControl` and `SetFilterValues` should be decided on the host.** Done.
8. **The sync measure state lives on the host.** The phase stays host-owned, and the close keeps today's behaviour.
9. **A caught panic needed the `failures.rs` mapping.** It is now stated, with a test per row.
10. **`restart: true` security.** It stays `destructive` and counts as an on-air edit.

Nits applied:
- one rule for when a row uses a ticket and when it uses the window flag
- the `SetSourceBox` Main and Vertical difference
- the `SetCanvas` recording difference
- the step order, with `ListTargets` after 1.101.

### Round 2: the same independent AI session, 2026-11-22, approve with changes

The counts and the ticket vs window-flag split were confirmed.

1. **`browser.input` drops the held-button bits.** It now gains `held`, and `leave` gains `modifiers`, in 1.101 (decision 3). `InteractInput` moves in step 4.
2. **`CloseSyncMeasure` could miss its measurement.** It is now a host `Translates` row built from `measure.run`. The counts became Translates 24 and View 13.
3. **`PickSource` needs the prompt.** It now prompts and replays itself, and its thread posts an internal `Msg`.
4. **Where the prompt for a `Command::Api` runs.** It runs in `Host::command`, before `ApiTarget::run`, with a test that a client's `source.add` never prompts.
5. **The on-air sentence added nothing.** It was dropped: the irreversible-actions check already applies.

Nits applied:
- `StartSyncMeasure` is now under one pattern.
- One line on what `app` and `recent` tell a token.
- The `InteractInput` measurement note.
