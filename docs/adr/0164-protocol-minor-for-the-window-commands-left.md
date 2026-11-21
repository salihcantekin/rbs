# ADR-0164: One protocol minor for the window commands that are left

- Status: Proposed
- Date: 2026-11-21
- Deciders: Salih Cantekin (owner)

## Context

[ADR-0163](0163-one-command-path-for-the-window-and-the-api.md) moves every window `studio_host::Command` that has a control API method into `Command::Api`. After steps 0 to 6, the [inventory](../research/command-path-inventory-2026-11.md) still has three groups that cannot move without a contract change:

- **Contract (10):** a method exists, but it lacks what the window sends. `item.set_crop`, `item.set_style`, `item.set_scroll`, `source.set_settings` and the `item.flip`/`rotate`/`fit`/`align`/`reset_transform` family have no `gesture`, so one drag would become many undo steps. `record.start`, `stream.start` and their vertical forms take no request, while the window builds its own `RecordRequest` / `StreamRequest`. `vcam.start` takes no size or rate.
- **Missing (20):** no method at all (the rows marked `Missing`).
- **Pending, blocked on the contract:** `AddOn` Update (install, then clean the old version), and `RetargetSource`. The API target keeps only `adapter` and `target`, but the window also sends the name, the app and the sound.

The owner approved one short ADR for one minor (2026-11-21 19:09 UTC). The protocol is at 1.99 (`rbs-api` `session.rs`), and minors only add things ([control API spec](../specs/control-api.md)). The window is the local user and skips caller checks (ADR-0163 decision 3). A remote client may not put an arbitrary local file on air until token scopes exist ([ADR-0129](0129-text-source.md) decision 8).

## Decision

1. **One minor, 1.100.** Everything below is added in protocol 1.100. Every new field is optional, and leaving it out keeps today's behaviour. No existing field, method or error kind changes meaning.
2. **Gestures.** `item.set_crop`, `item.set_style`, `item.set_scroll`, `source.set_settings` and the item transform methods take an optional `gesture: u64`. Consecutive calls with the same non-zero gesture on the same target are one undo step, as the window's drags are today. A gesture is only a merge hint and never refuses a call.
3. **Output starts build from host state.** `record.start`, `stream.start` and their vertical forms keep taking no request. The host builds the request from the settings the window already mirrors (`SetProfile`, `SetStreamProfile`, destinations). Before each start variant moves, a test must compare the window's builder with the host's for the same settings. Each difference is fixed in the host builder or listed in the inventory. `vcam.start` takes optional `width`, `height` and `fps`.
4. **Settings fields.** `settings.set` / `settings.get` add:
   - `swap_after_transition`
   - `vertical_profile` and `vertical_stream`, each optional (null means it follows the main one)
   - `separate_files`
   - `replay`, with the replay buffer's settings
   - `stream_destinations`, as names, URLs and on/off only.

   Stream keys stay window-only (`SaveStreamKey`, LocalOnly). They must never be in a settings payload or an event. `SetLibrary` is reclassified as WindowOnly Mirror: the window owns the library files, and `profile.*` / `collection.*` already read and switch them.
5. **New methods:**

   | Method | For |
   | --- | --- |
   | `scene.move` with an optional `to_scene` (takes that scene's place) | `MoveSceneTo` |
   | `item.move` with an optional `to_item` | `MoveSourceTo` |
   | `edit.undo` with an optional `only_kind: "delete"`: refused with `conflict` when the newest step is another kind | `UndoDelete` (the undo toast) |
   | `source.add` kinds `shape` (with `shape`), `image` and `media` (with `path`), `browser` (with `url`) | `AddStamp`, `AddImage`, `AddMedia`, `AddBrowser` |
   | `source.add_voice_chain` `{source}` | `AddVoiceChain` |
   | `collection.set_stinger_file` `{path}` | `SetStingerFile` |
   | `text.fit_box` `{source}` | `FitTextBox` |
   | `source.set_filled` `{source, on}`: one undo step with its border | `SetSourceFilled` |
   | `item.set_shown_region` `{item, region: full, canvas_centred, left_half, right_half}` | `SetSourceRegion` |
   | `canvas.rehearse` | `Rehearse` |
   | `addon.update` `{id}`: install, then clean the old version | `AddOn` Update |

   `source.retarget` takes optional `name`, `app` and `sound` (the window's `Target` fields).
6. **Local files are a caller check.** A `path`, a `file:` URL, and a text source's `file` setting are refused with `forbidden` for every remote caller, whatever its token's scope, until token scopes exist. The window passes. This check moves out of the `source.set_settings` handler into the caller checks, so the handler is the same for both.
7. **Order.** The code follows in steps, one namespace each, as in ADR-0163. Each step adds its fields, tests (a conformance case per method and a window test per move) and the spec rows, then moves the variants and lowers the ratchet. No step may change what the user sees without the owner's OK.

## Consequences

- When the steps are done, every row is `Moved`, WindowOnly or Translates, apart from the rows the inventory lists as staying for a visible reason (for example `SetCanvas`, `SaveReplay`). The `Pending` and `Missing` ratchet can reach its floor.
- Clients get the window's whole editing surface: shapes, images, media, browser pages, reordering, a fitted text box, rehearsal and add-on updates.
- The spec grows by about 15 methods or fields. The file rule (decision 6) keeps remote clients from reading local files, at the cost that a remote client cannot add an image or media file yet.
- Gesture merging now also applies to remote calls, so a client can make one undo step of a drag.
- Follow-up: `control-api.md` rows and a 1.100 changelog entry, conformance tests, and inventory rows per step. Then the `docs/status.md` WP-M row.

## Alternatives considered

- **One minor per namespace:** many small version bumps for one piece of work, and clients would see a moving target. The owner chose one minor.
- **Send the window's own `RecordRequest` through the API:** it would put encoder, path and transport details in the contract. Building from host state already serves clients and keeps one builder.
- **Allow local paths for remote callers:** that would put any readable file on air from the network, against ADR-0129 decision 8.
- **Keep the 31 rows as window commands:** the two paths keep drifting, which is the problem ADR-0163 solves.

## Review

Independent review per ADR-0014: pending.
