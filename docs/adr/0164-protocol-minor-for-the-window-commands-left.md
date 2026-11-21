# ADR-0164: One protocol minor for the window commands that are left

- Status: Proposed
- Date: 2026-11-21
- Deciders: Salih Cantekin (owner)

## Context

[ADR-0163](0163-one-command-path-for-the-window-and-the-api.md) moves every window `studio_host::Command` that has a control API method into `Command::Api`. After steps 0 to 6, the [inventory](../research/command-path-inventory-2026-11.md) still has rows that cannot move without a contract change:

- **Contract (10):** a method exists, but it lacks what the window sends.
  - Drags: `item.set_crop`, `item.set_style`, `item.set_scroll`, `source.set_settings` and the item transform methods have no `gesture`.
  - Starts: the window builds its own `RecordRequest` / `StreamRequest`.
  - Camera: `vcam.start`.
- **Missing (20):** no method.
- **Pending, blocked on the contract:** `AddOn` Update and `RetargetSource`.

The owner approved one short ADR for one minor (2026-11-21 19:09 UTC). The protocol is at 1.99 (`rbs-api` `session.rs`), and a minor only adds ([control API spec](../specs/control-api.md)). Several rules constrain the design:

- The window is the local user and skips caller checks (ADR-0163 decision 3).
- No API client reads or writes a stream server address or key. The virtual camera's size and rate are the local user's (spec, local-user list).
- Image and media `path` settings are allowed for any caller when they are local absolute paths ([ADR-0074](0074-still-image-and-colour-sources.md) decision 9, `network.rs`).
- A text source's `file` is refused for every API caller ([ADR-0129](0129-text-source.md) decision 8).

## Decision

1. **One minor, 1.100.**
   - Every addition below lands in protocol 1.100. No existing field, method or error kind changes meaning.
   - The minor is an integer, and versions compare numerically (1.100 > 1.99), never as a string or a float. A test checks this.
   - The window always talks to its own server. A client checks for 1.100 before it uses an addition, and the spec states the 1.99 behaviour for each changed request:
     - ignored: `SetItemStyle`, `SettingsPatch` and `AddSource` drop unknown fields.
     - refused with `invalid_params`: `SetItemCrop`, `SetItemScroll`, `SetSourceSettings`, the transform requests, `MoveItem` and `MoveScene` deny unknown fields.
   - A behaviour that would be harmful if it were ignored is a new method, not a field.
2. **Gestures.**
   - `item.set_crop`, `item.set_style`, `item.set_scroll`, `source.set_settings` and the item transform methods take an optional `gesture: u64`, as `item.set_box` does since 1.34.
   - The merge rule stays the one `History::record` has: same non-zero gesture and same edit kind as the newest step.
   - Gestures are split into two ranges:
     - The window numbers its gestures from 2^63, so a client's gesture never joins a window step.
     - Clients use values below 2^53 (the JSON safe range).
   - Two clients that reuse a number on the same kind can join steps. The spec says so, and no `Caller` change is needed.
   - A gesture never refuses a call on 1.100.
3. **Output starts: no new fields.**
   - The window sends `record.start {profile: "default", canvas, iso}` and `stream.start {canvas}`, as a client does.
   - The start gate is already the same: the window sends neither `acknowledge_heavy` nor `on_hardware_limit`.
   - The open difference is `iso`: the window always sends `true`, while `record.start` reads Separate files. That step makes the window send what the setting says, or lists it in the inventory.
   - The window's own checks become refusals with `data.reason`. Older clients ignore `data`. Each reason comes with an error kind:

     | `data.reason` | Kind |
     | --- | --- |
     | `recording_blocked` | `conflict` |
     | `profile_problem` | `invalid_params` |
     | `vertical_not_set_up` | `conflict` |
     | `streaming_off` | `conflict` |

     The window offers its "set up" action from the reason.
   - A test compares the window's request builder with the host's for the same settings, on an injected clock (rule 14).
4. **Virtual camera: no new fields.** The window sends plain `vcam.start` after its `SetVirtualCamera` mirror, and the host builds from `CameraSetup`, as it does for clients.
5. **Settings fields and mirrors.**
   - `settings.set` / `settings.get` add five fields:

     | Field | Refused with `conflict` |
     | --- | --- |
     | `swap_after_transition` | never |
     | `separate_files` | while a recording runs |
     | `replay` | never: a change applies at the next arm, and a running buffer keeps its settings |
     | `vertical_profile` | while a vertical recording uses it |
     | `vertical_stream` | while the vertical stream runs |

   - `vertical_stream` follows the `stream` rule: no server and no key.
   - For `vertical_profile` and `vertical_stream`, `null` removes the saved section, and the main one applies.
   - A patch is checked whole before anything is applied, as today.
   - `rbs-app` owns all five files, so these variants become WindowOnly Mirror:
     - `SetSwapAfterTransition`
     - `SetSeparateFiles`
     - `SetReplay`
     - `SetVerticalProfile`
     - `SetVerticalStreamProfile`
   - A client's change comes back to the window, which saves it without overwriting a form being edited, as the `Update::StreamProfile` arm does. Each field has its own update: `Update::SwapAfterTransition`, `Update::SeparateFiles`, `Update::ReplaySetup`, `Update::VerticalProfile` and `Update::VerticalStreamProfile`.
   - **Stream destinations get no settings field.**
     - `SetStreamDestinations` splits. Switching a destination on or off goes through `stream.destination.set_enabled` (1.74) via `Command::Api`.
     - Everything else stays LocalOnly: adding, removing, renaming, server, key, encoding, order, reconnect and SRT latency. The spec's local-user list gains rename, encoding, order, reconnect and SRT latency.
6. **New methods and fields.** A client has no edited scene, so the methods that change an item take an item id.

   | Addition | Scope / effect | For |
   | --- | --- | --- |
   | `scene.move` / `item.move`: `direction` becomes optional, with new `to_scene` / `to_item`. Exactly one is given, else `invalid_params`. The moved one takes the target's position, and the ones between shift by one toward its old place. `to_item` must be a picture layer in the same scene. | control / edit | `MoveSceneTo`, `MoveSourceTo` (the window sends item ids) |
   | `edit.undo_delete`: undoes the newest step if it is a delete or a remove from scene, else `conflict`. The window answers quietly. On 1.99 it is `method_not_found`. | control / edit | `UndoDelete` (the undo toast) |
   | `source.add` kind `color` with an optional `stamp` (see below the table) | control / edit | `AddStamp` |
   | `source.add` kinds `image` and `media` with a `path` (exclusive with `target`), and `browser` with a `url`. An image box is sized from the picture. A file that does not decode is `invalid_params`, without the path in the text (ADR-0074 decision 9). `stamp`, `path` and `url` on any other kind are `invalid_params`, as `text` is today. | control / edit | `AddImage`, `AddMedia`, `AddBrowser` |
   | `filter.add_voice_chain {source}`: the four voice filters on a sound source, one undo step. Any other source is `invalid_params`. | control / edit | `AddVoiceChain` |
   | `collection.set_stinger_file {path}` | control / edit | `SetStingerFile` |
   | `item.fit_text_box {item, canvas}`: keeps the centre and the scale | control / edit | `FitTextBox` |
   | `item.set_filled {item, on, canvas}`: the fill is the source's setting, shared by every scene that places it. Turning it off adds a border to that item's style on `canvas`. One undo step. | control / edit | `SetSourceFilled` |
   | `item.set_shown_region {item, region: full / canvas_centred / left_half / right_half, canvas}` | control / edit | `SetSourceRegion` |
   | `canvas.rehearse`: refused with `conflict` and a `data.reason` (`not_studio_mode`, `transition_running`, `rehearsing`, `no_duration`). The window answers quietly. The `ui.transition.rehearse` registry entry resolves to it. | control / edit (preview only, not on air) | `Rehearse` |
   | `addon.update {id}`: installs the new version, then cleans the old one. It reaches the network as `addon.install` does. | plugins / destructive | `AddOn` Update |

   The `stamp` values for `source.add` kind `color` are `circle`, `square`, `rectangle`, `rounded_rectangle`, `ellipse`, `triangle` and `arrow`. The spec gives each one's box and radius as `add_stamp` has them.

   `addon.update` keeps the old version whenever the update does not finish:
   - If the install fails or is cancelled, the old version stays.
   - The clean starts from the install's done event, with no polling (P13).
   - If the app quits between the install and the clean, the next start uses the newest complete version and cleans the older one once, at start.

   `source.retarget` gets no new fields. For every caller, the host fills the name, the app and the sound from its own target list. A picker target stays in the window, split as `AddSource`.
7. **Local files and the network: today's rules, one place.**
   - A `path` (image, media, stinger) must be a local absolute path, as ADR-0052 decision 4 and `network.rs` `is_local_path` require today.
   - `source.add` with kind `browser` needs the irreversible-actions permission whatever the URL. So does writing any key in `BROWSER_NETWORK_KEYS`.
   - The new `path` fields join `Command::reaches_network` on the API thread.
   - A text source's `file` stays refused for every API caller except the window. "API caller" means every caller but the window, with or without a token, `rbs-cli` and MCP over IPC included.
   - That check moves from the `source.set_settings` handler to `handle_as` on the control thread, next to `check_network`, because it needs the source's kind.
8. **Extension-ready.** `item.fit_text_box`, `item.set_filled` and `filter.add_voice_chain` are type-specific methods. They test an existing `SourceKind`, which rule 19 allows, and add no closed list. When the type registry exists (ADR-0162), they become actions of their type's entry.
9. **Steps and ratchet.**
   - The code follows one namespace per step, as in ADR-0163. Each step adds its fields with a conformance case (including the 1.99 behaviour), a window test per move and the spec rows, then lowers the counts.
   - `MIRROR` may rise from 3 to 8 (decision 5) and `LOCAL_ONLY` from 9 to 10 (the destination split). The owner's approval of this ADR is that approval.
   - No step may change what the user sees without the owner's OK.
   - Target after the steps: `CONTRACT` 0, `MISSING` 1 (`SetLibrary`, decision 10), `PENDING` 15. Those 15 are:
     - rows that stay for a visible reason: `SetCanvas`, `SaveReplay`, `StopVirtualCamera`, `SetFilterValues`, `BrowserControl` and `InteractInput` (see the inventory)
     - `ForgetBrowserSignIn`, until `browser.forget_sign_in` also stops the page first
     - eight rows ADR-0163 moves later without a contract change (ticketed answers): `ListTargets`, `AddSource`, `AddSourceWithAudio`, `AddGlobalSource`, `SetSourceBox`, `ListAddOns`, `StartSyncMeasure` and `CloseSyncMeasure`.
10. **`SetLibrary` stays Missing.**
    - The methods exist: `ui.profile.*` and `ui.collection.*` (1.23), routed through the window.
    - `SetLibrary` is the window reporting its own library list for `profile.list` / `collection.list`. The row goes when the library files move to the host, which is not part of this ADR.
    - The inventory reason is corrected to say so.

## Consequences

- Clients get the window's editing surface: shapes, images, media, browser pages, reordering, a fitted text box, the voice chain, rehearsal and add-on updates. No file, network, server or key rule becomes looser.
- The additions are the 11 rows of decision 6, the five settings fields, the `gesture` field, the start reasons and their updates.
- Window drags and client drags never join one undo step.
- Follow-up: the `control-api.md` rows and a 1.100 changelog entry, the numeric-version test, the conformance cases, the inventory rows per step (`SetLibrary`'s reason first), and the `docs/status.md` WP-M row.

## Alternatives considered

- **One minor per namespace:** many small version bumps for one piece of work. The owner chose one minor.
- **Send the window's own `RecordRequest` or camera size through the API:** it puts encoder, path and local-user details in the contract. One host builder already serves clients.
- **A session id in `Caller` for gestures:** it is a semver change to `rbs-api` and changes `item.set_box`. Separate number ranges give the window the same protection.
- **A `stream_destinations` settings field:** it repeats `stream.destinations` (1.74), and the spec keeps destinations with the local user.
- **The voice chain as a look:** looks are picture-only (`look.apply`, `look.list`), so it would change an existing method.
- **Forbid all local paths for API callers:** it removes what ADR-0052 and ADR-0074 allow today, and that needs a versioning exception the owner has not given.
- **`edit.undo` with a field:** a 1.99 server would ignore the field and undo any step.

## Review

### Round 1: independent AI session, 2026-11-21, changes requested

- Reviewer: independent AI session
- Date: 2026-11-21
- Verdict: changes requested

Eighteen concerns were raised. Each is resolved in this revision, as follows:

1. **Destination URLs in settings broke the spec's no-server rule.** Now names, on/off, `protocol` and `key_saved` only. URL entry is LocalOnly, and `vertical_stream` follows the `stream` rule (decision 5).
2. **The local-file rule was ambiguous against today's allowed paths.** It now keeps today's rules (decision 7). "API caller" is defined, and the check sits in `handle_as`.
3. **`record.start` / `stream.start` already take params, and the gate, `iso` and setup checks differ.** Decision 3 is reworded: no new fields, the differences are listed, `data.reason` is added, and the builder test runs on an injected clock.
4. **`vcam.start` size and rate are the local user's.** No new fields (decision 4).
5. **Old servers ignore unknown fields.** Decision 1 adds per-field spec text and the numeric-version test. Undo-delete becomes a new method (decision 6).
6. **Gesture merge key.** Now the session, the gesture and the edit kind, with the window as a session of its own (decision 2).
7. **Undo-delete scope.** "Delete" covers both delete and remove, and the window's refusal is quiet (decision 6).
8. **Shape is not a source type.** Now `source.add` kind `color` with `shape` (decision 6).
9. **`scene.move` / `item.move` already exist.** `direction` becomes optional, with exactly one of `direction` / `to_*`, defined place semantics and item ids (decision 6).
10. **Source-addressed methods edit an item.** They are now addressed by item (decision 6).
11. **Mirror rise.** Allowed by the number of rows that become Mirror (decision 9).
12. **`SetLibrary` did not meet ADR-0163's Mirror definition.** It stays Missing and is named as a parity gap (decision 10).
13. **Retarget hints a client could spoof.** No new fields: the host looks the target up (decision 6).
14. **`addon.update` effect and failure cases.** Destructive, the old version stays on failure, and the clean is event-driven (decision 6).
15. **Rule 19.** The two type-specific methods are covered by decision 8. The voice chain becomes a built-in look.
16. **"The ratchet can reach its floor" could not be checked.** The rows left, the target constants, the exact additions and the rehearse refusal reasons are now listed (decisions 6 and 9).
17. **Replay and vertical settings details.** Replay applies at the next arm; vertical `null` removes the saved section (decision 5).
18. **Old-server tests.** A conformance case covers the 1.99 behaviour of each field (decision 9).

### Round 2: independent AI session, 2026-11-21, changes requested

- Reviewer: independent AI session
- Date: 2026-11-21
- Verdict: changes requested

Round 1 items 3, 4, 5, 7, 9, 13, 17 and 18 were confirmed resolved. Sixteen new or remaining concerns were raised, and each is resolved in this revision:

1. **`builtin:voice_chain` would change `look.apply`.** Now a separate method, `filter.add_voice_chain` (decision 6).
2. **`stream_destinations` repeated 1.74 and broke the local-user rule.** The field is dropped. The variant splits into `stream.destination.set_enabled` and LocalOnly, and the local-user list grows (decision 5).
3. **The gesture session needed a `Caller` change.** The window and clients now use separate gesture ranges, so there is no `Caller` change and `item.set_box` is unchanged (decision 2).
4. **The Mirror rows were unspecified.** The five rows, the updates back and `MIRROR` 3 to 8 are now named (decisions 5 and 9).
5. **`shape` could not express the stamps.** Now a `stamp` preset with seven values, boxes from `add_stamp`, and other kinds refused (decision 6).
6. **Decision 10 was wrong.** Corrected: the methods exist, and `SetLibrary` is the window's report of its own library list.
7. **The browser rule was misstated.** Restated; the new paths join `reaches_network`, and ADR-0052 is cited (decision 7).
8. **The 1.99 behaviour list was incomplete.** It is now given per request (decision 1).
9. **Scope and effect were missing.** Added per method, with the rehearse registry entry (decision 6).
10. **Start reasons were not named.** Now named with their kinds. The gate difference is closed, and `iso` is the open one (decision 3).
11. **Refusal rules per settings field.** Added: atomic patch, `null` semantics (decision 5).
12. **`addon.update` when the app quits before the clean.** Covered (decision 6).
13. **`item.set_filled` changes every scene.** Stated, with the canvas (decision 6).
14. **Decision 8 wording.** Reworded.
15. **Counts.** Corrected, and the constants are stated (decision 9).
16. **Image decode errors.** No path in the text, `invalid_params` (decision 6).
