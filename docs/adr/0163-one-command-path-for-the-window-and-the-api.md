# ADR-0163: One command path for the window and the control API

- Status: Accepted
- Date: 2026-11-21
- Deciders: Salih (owner)
- Builds on: ADR-0008 (control API), ADR-0050 (command registry), ADR-0144 (code quality rules, "one command path"), ADR-0159 (studio host crate), ADR-0162 (extension-ready registries)
- Plan: quality plan WP-20 (audit QS-03), designed with performance G-02 (perf plan C2)

## Context

The desktop window and the control API change the studio through two enums that both end on the control thread:

- `rbs_studio_host::Command`: 161 variants, sent by the window through `studio_ui::Studio::send` (about 175 call sites in `rbs-app`) and routed by `Host::command` to per-concern handlers (`studio_host/{sources,scenes,items,outputs,stream,source_settings}.rs`).
- `rbs_api::Command`: the versioned contract (protocol 1.99), routed by `ApiTarget::run` to `studio_host/control_api/api_*.rs`.

Since WP-L both routers are exhaustive matches, and both mostly call the same `Host` methods (`rename_scene`, `remove_scene`, ...). They differ in what wraps those calls: the window path answers with `Host::answer` and `Update::Error(String)`, the API path with `Result<Response, ApiError>`, its own parameter checks, `output_refusal` (which also refuses a start while a profile or collection switch is pending), refusal logging, and the events it publishes. A first pass ([inventory](../research/command-path-inventory-2026-11.md)) finds:

| Kind | Variants |
| --- | --- |
| Already run their API method(s) through `Host::run_ui_api` or `ApiTarget::run` (volume, mute, scene levels, master, vertical auto layout and size, loudness, look, sample, markers, destinations, pause, hold, break) | 21 |
| Have an API method but their own wiring, or call an `api_*` helper directly without `run_ui_api` | 83 |
| Have an API method that needs a contract change first: a `gesture` on the item and settings methods, the window's own start request for record, stream and virtual camera | 10 |
| Have no API method (shapes, image, media and browser files, voice chain, stinger file, fill-off border, fit text box, the shown region, drop-onto-row moves, rehearse, undo-only-a-delete) or no `settings.set` field (vertical profiles, destinations, separate files, replay, library lists, swap after transition) | 20 |
| Only exist for the window: views and pickers, stream keys and OS permission prompts, a filter slider's preview, settings `settings.set` covers that the window saves and then tells the host | 27 |

Two paths cost twice the wiring and checks, and they drift: the vertical canvas's picture drag calls `set_vertical_box` directly though `item.set_box` has had a `canvas` since protocol 1.44. A remote client, Stream Deck or an AI tool cannot do what the window does until someone adds the second path. The control API spec says everything a user can do in the app is doable through the API, except a short local-user list, and that the U20 test allows exactly those; today `rbs-app/tests/command_registry.rs` checks registry entries only, not what panels send.

G-02 (an item drag rebuilds the whole studio on every step) wants a layers-only path for move, resize and crop. Built behind a window command, drags would get a second design the API does not have.

## Decision

1. **The window sends control API commands.** `Command` gets `Command::Api { call: Box<rbs_api::Command>, ticket: u64 }`, and `studio_ui::Studio` gets `call(rbs_api::Command)` and `call_answered(..) -> ticket`. The host runs it through the same function as `run_ui_api`, which from step 0 is `ApiTarget::run` inside `rbs_crash::contain` plus `failures::log_api_refusal`: the same parameter checks, `output_refusal`, handler, events, logging and panic containment as a remote call.
2. **Answers.** A refusal or a caught panic reaches the window as `Update::Error` with the `ApiError`'s message. A call with a non-zero ticket is also answered with `Update::ApiAnswer { ticket, result: Result<Response, ApiError> }`, refusals and panics included. Step 0 adds this, because moved variants need it: `SetSourceBox` and `SetSourceCrop` (the window matches `BoxPlaced` / `CropPlaced` by `seq`, `ok` false on a refusal), the `Start*` outputs (the button must leave "starting" on a refusal or panic, today `answer_on_panic`), and the queries `ListTargets`, `ListTextFonts`, `ListAddOns` and `ListMonitorDevices`. `StartSyncMeasure` and `CloseSyncMeasure` use the ticket too, because the Measure view shows their refusals in its phase. A refusal the window ignores today is sent quiet: logged and not shown. That covers high-rate live input (`canvas.transition_progress` from the T-bar, `browser.input` from an Interact window), where stale events are ignored, and Undo or Redo with nothing to undo (`conflict`). The quiet cases are named per variant in its `Command::path` arm, never a blanket rule. State changes still come back through the usual `Update`s.
3. **The window is the local user.** Window calls skip the caller checks of `ControlTarget::handle_as` (token scopes, the irreversible-actions permission, on-air and network checks, local-only methods). Those protect against remote callers; the window is trusted as the person at the computer, as `run_ui_api` is today and as the spec gives the in-process UI all scopes. (The window asks confirmations only for irreversible actions, U4, not for every on-air edit.) A caller check that sits inside a handler today, such as `source.set_settings` refusing a text source's `file` "whatever the token's scope", moves into `handle_as` in the step that moves its variant, so the window keeps doing what it does now and remote callers stay refused.
4. **Every variant is classified at compile time.** `Command::path(&self) -> Path` is one exhaustive `match`, so a new variant does not build until it is classified:
   - `Path::Api`: `Command::Api`.
   - `Path::Translates(&'static [&'static str])`: the handler only builds the listed API methods from control-thread state and runs them through `run_ui_api` (the Mixer fader runs `scene.audio.set` while the program scene keeps its own levels, else `source.set_volume`; Remove markers runs `marker.remove` per marker). Nothing else.
   - `Path::WindowOnly(WindowOnly)`, with one reason: `View` (opens, closes or feeds a view of this window, a picker's thumbnails or the meters shown, and changes no collection, history or published state), `LocalOnly` (the spec's local-user list: stream server and keys with their accounts, the virtual camera's setup; plus the operating system's permission prompts, which need the person at this computer), `Preview` (a value shown on the running studio while a slider is dragged, and the stored value put back; never saved, never on the history) and `Mirror` (the window saved a settings file it owns and tells the host, or hands it a collection it loaded, for a setting the API already changes: recording profiles, stream settings and canvas through `settings.set`, which comes back to the window, and the collection `collection.switch` asks the window to load). A window-saved setting with no `settings.set` field is a parity gap and `Missing`.
   - `Path::Pending(&'static [&'static str])`: the API methods exist and do what the window's handler does; the variant has not moved yet.
   - `Path::Contract(&'static [&'static str])`: the API methods exist but need a protocol minor first (a `gesture`, a request parameter) to do what the window does.
   - `Path::Missing`: no API method, or no `settings.set` field, yet.
   - A variant that is part API call and part window or OS work (Add source asks for the permission and opens the system picker first) is split: the window does its part, then sends `Command::Api`; the window part keeps a `WindowOnly` variant only if it needs the control thread.
5. **The tests (fast tier, `rbs-studio-host`).** With `command_registry.rs` they are the U20 test of the control API spec.
   - A ratchet: the counts of `Pending`, `Contract`, `Missing`, `Translates` and of each `WindowOnly` reason are constants in the test; a count above its constant fails. Steps lower them; `Translates` may rise by as many as `Pending` falls in the same step (a moved handler that must pick a method from host state, such as Switch scene in studio mode). Raising a count otherwise needs the owner's written approval in the commit, as for `check-ui` exceptions. A new window operation is `Command::Api` (ADR-0144).
   - Every method named by `Translates`, `Pending` and `Contract` is in `rbs_api::request::METHODS`.
   - Every `View` and `Preview` variant runs on a test host and leaves the collection and the undo history unchanged, and every events `seq` except topics the test names for that variant (opening or closing a projector or Interact window publishes the projector state).
   - Every `Translates` variant runs on a test host that records the methods `run_ui_api` received; they are within its declared list.
6. **The spec's local-user list grows by these reasons, and only these.** The control API spec's "kept for the local user" paragraph is amended in step 0 to name the window-only kinds: the OS permission prompts with the local-user items, and `View`, `Preview` and `Mirror` as not user actions on the studio. `Mirror` rows go away when their file moves to the host (follow-up; loudness settings already did, and run `settings.set`). A view a remote client could want (projectors) can later become a `ui.*` method (`UI_METHODS`).
7. **Missing operations get API methods** in the step of their namespace (a protocol minor version and spec section as usual), or become `WindowOnly` with a reason above. "No API method yet" is never a reason.
8. **One edit path for drags (G-02).** A drag, nudge or slider gesture sends the same API method a remote client would (`item.set_box`, `item.set_crop`, `item.set_style`, ...) with its `gesture` and `canvas`. Today only `item.set_box` takes a `gesture`; `item.set_crop`, `item.set_style`, the transform methods, `item.set_scroll` and `source.set_settings` get an optional one in their step (`Contract` rows), so one drag stays one undo step (U4). Whether the host takes the layers-only path is decided inside that method's handler from what the edit changes, never from who sent it, so G-02 lands once for every caller. The full path is required whenever a hold (ADR-0063), a transition, glide (ADR-0100), a nested scene or a following canvas is involved, and a property test checks that the layers-only result equals the full rebind. G-02 itself is perf plan C2 with its own hot-path numbers; this ADR only fixes where it lives.
9. **Steps by namespace.** Step 0 adds `Command::Api`, the answers, `Command::path`, the tests and the spec paragraph, with no behaviour change. Then one step per namespace: scenes and transitions; items; sources and text; filters and looks; audio (mixer, monitor, loudness, scene levels, sync offset); outputs (record, stream, replay, virtual camera, markers, break, destinations); settings, profiles and collections; add-ons. Each moves its `Pending` and `Contract` variants, deletes their window arms and wrappers, lowers the constants, and drives its tests through the window path. The known differences each step meets are in the inventory; the window takes the API's behaviour (the switch-pending refusal, refusal logging) unless a visible text changes, which needs the owner's approval.
10. **Plugin-ready.** `Command::Api` carries whatever request form the control API has: today the closed `rbs_api::Command`, and the method id plus parameters that ADR-0162 step 2 adds for registered types. The window then runs plugin commands the same way, with no window path of their own.

## Consequences

- One place to validate, test and publish each operation. Remote control, Stream Deck and AI tools can do what the window does by construction, and the plugin SDK and MCP work (held back by the 2026-11-10 architecture review until there is one path) can start.
- `Command` shrinks from 161 variants to about 50 (27 `WindowOnly`, the `Translates` ones, `Command::Api`), and the window wrappers and their `Host::answer` arms go away; `check-size` counts fall.
- The window gets the API's checks. Where they are stricter, the step relaxes the API with a spec change or changes the window, and says which.
- Per window command, one `ApiTarget::run` dispatch on the control thread: a `match` and `output_refusal`. Not a hot path (docs/architecture/hot-paths.md); `Box` keeps the channel message small; no threads, polling or resources are added (P13).
- Step 0 also updates `CLAUDE.md` ("UI mutations go through `studio_host::Command` until they are also `rbs_api::Command`"), `docs/status.md` (U20 command parity) and the control API spec's Tests section.
- Follow-ups outside this ADR: the settings files behind `Mirror` move to the host; registry entries could run their methods directly instead of through `menu_bar::Run`; G-02's layers-only path (perf plan C2).

## Alternatives considered

- **Keep both enums and add a parity test only.** It finds a missing API method but keeps two wirings and two sets of checks that drift.
- **Generate `studio_host::Command` from `rbs_api::Command`.** A macro hides the window-only list instead of naming each reason, and window-only operations still need their own variants.
- **Send window commands over the in-process API channel as a session.** Adds a trip through the API thread, its rate limits and scopes to every window action; `Command::Api` keeps one thread and today's ordering.
- **Let the window call `Host` methods directly.** The window runs on the UI thread and the host owns the studio on the control thread; that boundary (ADR-0159) stays.

## Review

### Round 1: independent AI session, 2026-11-21, changes requested

1. `item.set_box` / `item.set_crop` already have `canvas` (protocol 1.44); only the window drifts. Fixed: Context and decision 8 corrected, vertical `SetSourceBox` is plain `Pending`.
2. Seven "already one path" rows call `api_*` helpers or host methods without `run_ui_api` (skipping `output_refusal` and containment). Fixed: reclassified as `Pending`; counts then 18 / 97 / 12 / 34.
3. The window-only reasons went beyond the spec's local-user list, OS permission prompts were wrongly said to be listed, and only `Pending` and `Missing` were capped. Fixed: decision 6 amends the spec in step 0; decision 5 ratchets every reason and `Translates`, tests that `View` / `Preview` change nothing, and checks `Translates` against the methods actually run.
4. Wrong rows: `Rehearse` (engine state, now `Missing`), `SetSwapAfterTransition` (now `Mirror`), `AddImage` / `AddMedia` / `AddBrowser` (`source.add` has no path or URL, now `Missing`); `RemoveMarkers` runs several calls. Fixed in the inventory and the `Translates` definition; `Mirror` explained against loudness in decision 6.
5. Moved variants need answers (`BoxPlaced`, `CropPlaced`, `NotStarted` on refusal or panic, query results). Fixed: decision 2 adds tickets and `Update::ApiAnswer` in step 0, with the list.
6. Behaviour differences (switch-pending refusal, refusal logging, refusal text wrapping). Fixed: listed in the inventory; decision 9 says which behaviour wins and when the owner decides.
7. G-02 needs a fallback and an equality check. Fixed in decision 8.
8. `rbs_api::Command` stays closed until ADR-0162 step 2. Fixed in decision 10.
9. Caller-check wording. Fixed in decision 3.
10. `Pending` carries a slice; mention `UI_METHODS`. Fixed in decisions 4 and 6.

### Round 2: independent AI session, 2026-11-21, changes requested

Round 1 items 1, 2, 4, 5, 8 and 10 confirmed resolved; 3, 6, 7 and 9 partly, completed by the fixes below.

1. `SwitchCollection` carries a loaded collection; `collection.switch` asks the window to load one. Fixed: `Mirror`.
2. `AddGlobalSource` adds a new source as global: `source.add` with `global: true`, not `global.add`. Fixed in the inventory.
3. `Mirror` hid parity gaps: `settings.set` covers only profiles, canvas, stream and loudness. Fixed: `Mirror` is limited to settings the API changes; seven rows are `Missing` (decision 4).
4. Only `item.set_box` takes `gesture`. Fixed: decision 8 corrected; new `Path::Contract` for rows that need a protocol minor first (gesture, start requests, virtual camera size).
5. `SetSourceFilled` also changes the item's style in one undo step. Fixed: `Missing`.
6. A caller check inside `source.set_settings` (text file) would block the window. Fixed: decision 3 moves such checks into `handle_as`.
7. Mixed rows (Add source's permission prompt and picker, Measure view refusals, start requests, silent T-bar and Interact input). Fixed: split rule in decision 4, ticket and quiet answers in decision 2, rows and differences in the inventory.
8. The ratchet blocked a `Pending` to `Translates` move. Fixed in decision 5.
9. Some `View` rows publish. Fixed: the test names the allowed topics per variant.

### Round 3: independent AI session, 2026-11-21, approve with changes

All round-2 items confirmed resolved; counts consistent. Row-level findings, fixed in the inventory: `SetSourceRegion` and `SetSourceCaptureRegion` were swapped (`source.set_region` is the capture region); `AddSourceWithAudio` is `source.add` with `with_audio: true` (one step with rollback); `SetCanvas` is `settings.set {canvas}`, so `Pending`, not `Mirror`; `PauseRecording`, `HoldStream` and `Break` already run `ApiTarget::run` (now `Translates`, their own error sentence a known difference). Known differences added: Undo/Redo with nothing to undo (decision 2 quiet answers), `RemoveFromScene`'s `Update::Deleted` for the undo toast, `ForgetBrowserSignIn` stopping the page first, `SetFilterValues` restoring stored values on an unchanged release, `Media` forcing the next media-state publish, the Test this computer refusal of `output_refusal`. Nits fixed: `Contract` methods checked against `METHODS`, the Context table's `Translates` note, the broken differences table. Final counts 21 / 83 / 10 / 20 / 27.

Owner's approval: Salih accepted on 2026-11-21 (decision card in the WP-M thread).
