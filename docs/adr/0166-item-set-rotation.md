# ADR-0166: `item.set_rotation`, a free angle for an item

- Status: Accepted
- Date: 2026-11-21
- Deciders: Salih Cantekin (owner)

## Context

[ADR-0163](0163-one-command-path-for-the-window-and-the-api.md) moves every window `studio_host::Command` that has a control API method into `Command::Api`. [ADR-0164](0164-protocol-minor-for-the-window-commands-left.md) adds what the remaining rows lack in protocol 1.100. One row is still blocked after it:

- `TransformSource` carries a `TransformOp` (`rbs-studio-host` `crop.rs`). Most ops have a method: `item.flip`, `item.rotate`, `item.fit`, `item.align` and `item.reset_transform` (protocol 1.18, ADR-0084), which take `gesture` in 1.100.
- `TransformOp::SetAngle(degrees)` has no method. Properties' rotation field and its drag send it (`rbs-app` `set_source_rotation`). `item.rotate` only turns by a non-zero multiple of 90 relative to the current angle.
- `TransformOp::ResetRotation` has no method either. No window code sends it, and its effect, `Layout::unturned`, equals `Layout::angled(0)` (`rbs-scene` `arrange.rs`).

So a client cannot set an item's angle, and the window cannot send its angle edit as an API call. The owner chose a short new ADR for this on 2026-11-21 (21:10 UTC).

## Decision

1. **New method `item.set_rotation {item, degrees, canvas?, gesture?}`**, scope `control`, effect `edit`.
   - `degrees` is the clockwise rotation about the box's centre: a JSON number (f64), -360 to 360 inclusive, else `invalid_params`. It is converted to f32 and wrapped into `0..360` by `Layout::angled`; -0 is stored as 0. JSON carries no NaN or infinity, so "not finite" matters only to typed Rust callers, which get `invalid_params` too. The box, flips and crop stay.
   - `degrees: 0` resets the rotation. No separate reset method is added.
   - `canvas` and `gesture` work as on the other transform methods. Their rules are the `canvas` rule of protocol 1.44 and the gesture rule of ADR-0164 decision 2, bit 63 included (`clear_window_gesture` clears it).
   - It records `EditKind::Transform`, as `item.flip` and the other transform methods do. Calls with one gesture across `item.rotate` and `item.set_rotation` therefore join one step, which the gesture rule allows.
   - A locked item is `conflict`, and an unknown item is `not_found`, as for `item.rotate`. An angle equal to the stored one after the wrap (360 on an item at 0) answers `{}`, changes nothing and records no undo step.
   - The on-air check lists it with the other transform methods, by its item (`on_air.rs` `BY_TARGET`).
2. **It joins protocol 1.100.** ADR-0164's steps are still landing, and no build with 1.100 has shipped: the repository has no release or tag, and GitHub release builds are not made yet (owner, 2026-11-21). So the minor gains one method instead of a 1.101. It changes no existing field's meaning. A 1.99 server, and a development build of 1.100 from before this change, answers `method_not_found`.
3. **The window moves.**
   - `set_source_rotation` sends `item.set_rotation` with the window gesture and `ApiReply::Shown`, so a refusal shows as today's `Update::Error` does. The item comes from the Sources row (`row_item`), as for the other item calls of ADR-0163, and the canvas from `props_target`. It wraps the field's value into `0..360` first, so a typed 720 still sets 0 as today, and drops a value that is not finite, which `Layout::angled` ignores today.
   - The other ops send their existing methods: a flip goes to `item.flip`, a turn to `item.rotate`, a fit to `item.fit`, a centre to `item.align` and a reset to `item.reset_transform`. Both entry points, `main.rs` `on_transform_source` (codes 0 to 9) and the menu bar's `Run::Transform`, go through `transform_source`, which maps the op. `Turn(f32)` becomes `item.rotate`'s `degrees` as the nearest `i32`; the window sends only 90, -90 and 180.
   - `Command::TransformSource` and `TransformOp::ResetRotation` are removed, and `CONTRACT` falls by one. `TransformOp` stays the host's op list; the window keeps it only as the input of `transform_source`'s mapping, and nothing sends it to the host any more.
   - Nothing the user sees changes.
4. **Extension-ready.** This is a generic item method with no type-specific branch and no new closed list (rule 19).

## Consequences

- Clients can set any angle, including during a drag that is one undo step.
- The ADR-0164 target of `CONTRACT` 0 becomes reachable, because `TransformSource` was the row it could not reach.
- Follow-up in the same change:
  - `rbs-api` `request.rs`: the command, method name, `METHODS` entry (`control`, `edit`), the `WireRequest` arm and its conversions; `Command::reaches_network` is unchanged.
  - `rbs-studio-host`: the `answer_item` dispatch, `clear_window_gesture`, `on_air.rs` `BY_TARGET`.
  - Tests: a JSON round-trip case; a host test (wrap, range, locked, unchanged, one undo step per gesture); a client gesture with bit 63 set does not join a window rotation drag; a window rotation of a global source and on the vertical canvas; `tests/studio/scenes/part_two.rs`, which sends `Command::TransformSource` today, rewritten on `item.flip`.
  - Docs: the `control-api.md` row, a bullet in its 1.100 section and its "Older servers" sentence, the inventory row, the `command_path.rs` count, and the `docs/status.md` WP-M row.

## Alternatives considered

- **Allow any multiple of 1 degree in `item.rotate`.** `item.rotate` is relative, so a drag would accumulate rounding, and its range and "non-zero" rule would change meaning. A minor must not change an existing field's meaning.
- **A separate `item.reset_rotation`.** `degrees: 0` already says it, and the window resets with `degrees: 0` (the rotation slider's reset sends `SetAngle(0)` today).
- **Leave `TransformSource` window-only.** Then a client lacks an edit the window has, which ADR-0163 exists to end.

## Review

- Round 1, independent AI session, 2026-11-21: approve with changes, no blocking concern. Applied:
  1. The wire type, the inclusive range, the wrap, -0 and the meaning of "unchanged" are stated (decision 1).
  2. The follow-up names every place a missing entry would break the contract silently, `clear_window_gesture` included, and a bit-63 test (Consequences).
  3. The window names its reply (`Shown`) and where the item and canvas come from, with tests for a global source and the vertical canvas (decision 3).
  4. The mapping of the other ops, both entry points and the `Turn` conversion are stated (decision 3).
  5. The test that sends `Command::TransformSource` is listed.
  6. The evidence for joining 1.100 is written down (decision 2).
  7. The gesture merge across the transform methods is stated (decision 1).
  8. The reset alternative is reworded.
- Owner: Salih Cantekin accepted it on 2026-11-21 (21:31 UTC).
