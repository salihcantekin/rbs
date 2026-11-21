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
   - Some 1.99 params ignore unknown fields: `Empty`, `SettingsPatch`, `AddSource`. For each new field there, the spec says that a 1.99 server ignores it and that a client checks for 1.100 first. A new behaviour that would be harmful if it were ignored gets a new method instead of a field.
2. **Gestures.**
   - `item.set_crop`, `item.set_style`, `item.set_scroll`, `source.set_settings` and the item transform methods take an optional `gesture: u64`. A client keeps it below 2^53.
   - A call joins the newest undo step when it has the same caller session, the same non-zero gesture and the same edit kind. The window counts as a session of its own. Today's `History::record` is extended with the session.
   - A gesture never refuses a call.
3. **Output starts: no new fields.**
   - The window sends `record.start {profile: "default", canvas, iso}` and `stream.start {canvas}`, as a client does.
   - Three differences must be closed or listed in the inventory before each start moves:
     - the start gate (`StartGate::Older` against `start_gate`)
     - the `iso` default
     - the window's own checks (`recording_blocked`, `problem()`, vertical setup, streaming off), whose "set up" action needs the reason in the error data. 1.100 adds `data.reason` for these refusals.
   - A test compares the window's request builder with the host's for the same settings, on an injected clock (rule 14).
4. **Virtual camera: no new fields.** The window sends plain `vcam.start` after its `SetVirtualCamera` mirror, and the host builds from `CameraSetup`, as it does for clients.
5. **Settings fields.** `settings.set` / `settings.get` add:
   - `swap_after_transition`
   - `separate_files`
   - `replay`. A change applies at the next arm: a running buffer keeps its settings, as today.
   - `vertical_profile`, where `null` removes the saved vertical section and the main one applies.
   - `vertical_stream`, under the `stream` rule: no server and no key.
   - `stream_destinations`, as names, on/off, `protocol` and `key_saved` only.

   Entering a destination's URL or key stays LocalOnly: `SetStreamDestinations` splits as `AddSource` does. Folder settings follow the existing `profiles.folder` rule.

   Some values are stored by the host. For those, the window sends `settings.set` through `Command::Api`. Where `rbs-app` owns the file, the variant becomes WindowOnly Mirror.
6. **Methods and fields.** "Edited item" below means an item id: a client has no edited scene.

   | Addition | For |
   | --- | --- |
   | `scene.move` / `item.move`: `direction` becomes optional, with a new `to_scene` / `to_item`. Exactly one of the two is given, else `invalid_params`. The moved one takes the target's position, and the ones between shift by one toward its old place. `to_item` must be a picture layer in the same scene. | `MoveSceneTo`, `MoveSourceTo` (the window sends item ids) |
   | New `edit.undo_delete`: undoes the newest step if it is a delete or a remove from scene, else refused with `conflict`. The window answers quietly. On 1.99 it is `method_not_found`. | `UndoDelete` (the undo toast) |
   | `source.add` kind `color` with an optional `shape` (the `ItemStyle` shape values). Kinds `image` and `media` take a `path`, and `browser` takes a `url`. Boxes are sized as the window does today (an image from its picture size). | `AddStamp`, `AddImage`, `AddMedia`, `AddBrowser` |
   | `look.apply` with a new built-in look, `builtin:voice_chain` | `AddVoiceChain` |
   | `collection.set_stinger_file {path}` | `SetStingerFile` |
   | `item.fit_text_box {item}`: keeps the centre and the scale | `FitTextBox` |
   | `item.set_filled {item, on}`: the fill and the border are one undo step | `SetSourceFilled` |
   | `item.set_shown_region {item, region: full / canvas_centred / left_half / right_half}` | `SetSourceRegion` |
   | `canvas.rehearse`. Refused with `conflict` and a `data.reason` outside studio mode, while a transition runs or holds, during a rehearsal, and for Cut or a duration of 0. The window answers quietly. | `Rehearse` |
   | `addon.update {id}`: install the new version, then clean the old one. Its effect is `destructive`, so the irreversible-actions permission applies. If the install fails, is cancelled or the app quits first, the old version stays. The clean is started by the install's done event, with no polling (P13). | `AddOn` Update |

   `source.retarget` gets no new fields. For every caller, the host fills the name, the app and the sound from its own target list. A picker target stays in the window, split as `AddSource`.
7. **Local files: today's rules, one place.**
   - New path and URL fields follow the rules the same values have in `source.set_settings` today:
     - an image, media or stinger `path` must be a local absolute path (ADR-0074 decision 9)
     - a browser `file:` URL needs the irreversible-actions permission.
   - A text source's `file` stays refused for every API caller except the window. "API caller" means every caller but the window, with or without a token, `rbs-cli` and MCP over IPC included.
   - That check moves from the `source.set_settings` handler to `handle_as` on the control thread, next to `check_network`, because it needs the source's kind.
8. **Extension-ready.** `item.fit_text_box` and `item.set_filled` extend the existing closed lists (`SourceKind`), as rule 19 allows until the type registry (ADR-0162) exists. They then become actions of their type's registry entry. No new closed list is added.
9. **Steps and ratchet.**
   - The code follows one namespace per step, as in ADR-0163. Each step adds its fields with a conformance case (including the 1.99 behaviour), a window test per move and the spec rows, then lowers the counts.
   - `MIRROR` may rise by exactly the number of `Missing` rows that become Mirror under decision 5. The owner's approval of this ADR is that approval.
   - No step may change what the user sees without the owner's OK.
   - Target after the steps: `CONTRACT` 0, `MISSING` 1 (`SetLibrary`, decision 10). `PENDING` keeps only these rows:
     - rows that stay for a visible reason: `SetCanvas`, `SaveReplay`, `StopVirtualCamera`, `SetFilterValues`, `BrowserControl`, `InteractInput` (see the inventory)
     - `ForgetBrowserSignIn`, until `browser.forget_sign_in` also stops the page first
     - rows ADR-0163 moves later without a contract change (ticketed answers): `ListTargets`, `AddSource`, `AddSourceWithAudio`, `AddGlobalSource`, `SetSourceBox`, `ListAddOns`, `StartSyncMeasure`, `CloseSyncMeasure`.
10. **Out of scope.** Library management (create, rename, duplicate, delete profiles and collections) stays a parity gap. `SetLibrary` stays Missing.

## Consequences

- Clients get the window's editing surface: shapes, images, media, browser pages, reordering, a fitted text box, rehearsal and add-on updates. No file, server or key rule becomes looser.
- The additions are the ten rows of decision 6, the five settings fields, the `gesture` field, the start reasons and `data.reason`.
- Gesture merging also serves clients, and the session keeps one client's drag from joining another's step.
- Follow-up: the `control-api.md` rows and a 1.100 changelog entry, the numeric-version test, the conformance cases, the inventory rows per step, and the `docs/status.md` WP-M row.

## Alternatives considered

- **One minor per namespace:** many small version bumps for one piece of work. The owner chose one minor.
- **Send the window's own `RecordRequest` or camera size through the API:** it puts encoder, path and local-user details in the contract. One host builder already serves clients.
- **Forbid all local paths for API callers:** it would remove what ADR-0074 decision 9 allows today, and that needs a versioning exception the owner has not given.
- **`edit.undo` with a field:** a 1.99 server would ignore the field and undo any step.
- **A `shape` source kind:** a shape is a colour source with a shaped style, not a type.

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
